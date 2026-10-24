//! The porcelain and plumbing commands, plus helpers they share.

pub mod branching;
pub mod history;
pub mod plumbing;
pub mod remote;
pub mod staging;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;

use anyhow::Result;

use crate::color::{self, paint};
use crate::diff;
use crate::index::Index;
use crate::object::{Kind, mode};
use crate::oid::Oid;
use crate::repo::{FlatTree, Repo};
use crate::worktree;

pub type Out<'a> = &'a mut dyn Write;

/// One side of a file comparison: its mode and id, and its bytes if they came from disk.
#[derive(Clone)]
pub struct FileSide {
    pub mode: u32,
    pub oid: Oid,
    pub data: Option<Vec<u8>>,
}

impl FileSide {
    fn load(&self, repo: &Repo) -> Result<Vec<u8>> {
        match &self.data {
            Some(d) => Ok(d.clone()),
            None if mode::is_gitlink(self.mode) => Ok(format!("Subproject commit {}\n", self.oid).into_bytes()),
            None => repo.odb.read_kind(&self.oid, Kind::Blob),
        }
    }
}

pub type Snapshot = BTreeMap<String, FileSide>;

pub fn snapshot_of_tree(flat: &FlatTree) -> Snapshot {
    flat.iter().map(|(p, &(m, oid))| (p.clone(), FileSide { mode: m, oid, data: None })).collect()
}

pub fn snapshot_of_index(index: &Index) -> Snapshot {
    index.entries().iter().filter(|e| e.stage == 0).map(|e| (e.path.clone(), FileSide { mode: e.mode, oid: e.oid, data: None })).collect()
}

/// The index, with every file that differs on disk replaced by its on-disk version.
pub fn snapshot_of_worktree(repo: &Repo, index: &Index) -> Result<Snapshot> {
    let mut snap = snapshot_of_index(index);
    let st = worktree::status(repo, index)?;
    for (path, change) in &st.unstaged {
        if *change == worktree::Change::Deleted {
            snap.remove(path);
            continue;
        }
        let full = repo.abs(path);
        let md = fs::symlink_metadata(&full)?;
        let data = worktree::read_worktree_file(&full, &md)?;
        let m = if repo.file_mode_enabled() { crate::index::file_mode(&md) } else { index.get(path).map_or(mode::FILE, |e| e.mode) };
        snap.insert(path.clone(), FileSide { mode: m, oid: Oid::hash_object(Kind::Blob, &data), data: Some(data) });
    }
    Ok(snap)
}

#[derive(Default, Clone, Copy)]
pub struct DiffOptions {
    pub stat: bool,
    pub name_only: bool,
    pub name_status: bool,
    pub context: usize,
}

pub struct FileChange {
    pub path: String,
    pub old: Option<FileSide>,
    pub new: Option<FileSide>,
}

pub fn changes(old: &Snapshot, new: &Snapshot) -> Vec<FileChange> {
    let mut paths: BTreeSet<&String> = old.keys().collect();
    paths.extend(new.keys());
    let mut out = Vec::new();
    for p in paths {
        let (o, n) = (old.get(p), new.get(p));
        let same = match (o, n) {
            (Some(a), Some(b)) => a.oid == b.oid && a.mode == b.mode,
            _ => false,
        };
        if same {
            continue;
        }
        // file <-> symlink shows as a deletion plus an addition, like Git
        let type_change = matches!((o, n), (Some(a), Some(b)) if (a.mode == mode::SYMLINK) != (b.mode == mode::SYMLINK));
        if type_change {
            out.push(FileChange { path: p.clone(), old: o.cloned(), new: None });
            out.push(FileChange { path: p.clone(), old: None, new: n.cloned() });
        } else {
            out.push(FileChange { path: p.clone(), old: o.cloned(), new: n.cloned() });
        }
    }
    out
}

pub fn print_diff(repo: &Repo, old: &Snapshot, new: &Snapshot, opts: DiffOptions, out: Out) -> Result<()> {
    let list = changes(old, new);
    if opts.name_only {
        for c in &list {
            writeln!(out, "{}", c.path)?;
        }
        return Ok(());
    }
    if opts.name_status {
        for c in &list {
            let code = match (&c.old, &c.new) {
                (None, _) => 'A',
                (_, None) => 'D',
                _ => 'M',
            };
            writeln!(out, "{code}\t{}", c.path)?;
        }
        return Ok(());
    }
    if opts.stat {
        return print_stat(repo, &list, out);
    }
    for c in &list {
        print_file_diff(repo, c, opts.context, out)?;
    }
    Ok(())
}

fn print_file_diff(repo: &Repo, c: &FileChange, context: usize, out: Out) -> Result<()> {
    let p = &c.path;
    let meta = |s: &str| paint(color::BOLD, s);
    writeln!(out, "{}", meta(&format!("diff --git a/{p} b/{p}")))?;
    let short = |s: &Option<FileSide>| s.as_ref().map_or("0000000".to_string(), |f| f.oid.short());
    match (&c.old, &c.new) {
        (None, Some(n)) => {
            writeln!(out, "{}", meta(&format!("new file mode {:06o}", n.mode)))?;
            writeln!(out, "{}", meta(&format!("index {}..{}", short(&c.old), short(&c.new))))?;
        }
        (Some(o), None) => {
            writeln!(out, "{}", meta(&format!("deleted file mode {:06o}", o.mode)))?;
            writeln!(out, "{}", meta(&format!("index {}..{}", short(&c.old), short(&c.new))))?;
        }
        (Some(o), Some(n)) => {
            if o.mode != n.mode {
                writeln!(out, "{}", meta(&format!("old mode {:06o}", o.mode)))?;
                writeln!(out, "{}", meta(&format!("new mode {:06o}", n.mode)))?;
            }
            if o.oid == n.oid {
                return Ok(()); // pure mode change
            }
            let suffix = if o.mode == n.mode { format!(" {:06o}", o.mode) } else { String::new() };
            writeln!(out, "{}", meta(&format!("index {}..{}{suffix}", short(&c.old), short(&c.new))))?;
        }
        (None, None) => return Ok(()),
    }
    let old_data = match &c.old {
        Some(s) => s.load(repo)?,
        None => Vec::new(),
    };
    let new_data = match &c.new {
        Some(s) => s.load(repo)?,
        None => Vec::new(),
    };
    let a_name = if c.old.is_some() { format!("a/{p}") } else { "/dev/null".into() };
    let b_name = if c.new.is_some() { format!("b/{p}") } else { "/dev/null".into() };
    if diff::is_binary(&old_data) || diff::is_binary(&new_data) {
        writeln!(out, "Binary files {a_name} and {b_name} differ")?;
        return Ok(());
    }
    let hunks = diff::unified(&old_data, &new_data, context);
    if hunks.is_empty() {
        return Ok(()); // e.g. a new empty file: Git prints no ---/+++ lines either
    }
    writeln!(out, "{}", meta(&format!("--- {a_name}")))?;
    writeln!(out, "{}", meta(&format!("+++ {b_name}")))?;
    for (kind, line) in hunks {
        match kind {
            '@' => {
                // color only the "@@ ... @@" part, not the function context
                let end = line[2..].find("@@").map_or(line.len(), |i| i + 4);
                writeln!(out, "{}{}", paint(color::CYAN, &line[..end]), &line[end..])?;
            }
            '+' => writeln!(out, "{}", paint(color::GREEN, &format!("+{line}")))?,
            '-' => writeln!(out, "{}", paint(color::RED, &format!("-{line}")))?,
            '\\' => writeln!(out, "{line}")?,
            _ => writeln!(out, " {line}")?,
        }
    }
    Ok(())
}

fn count_changes(repo: &Repo, c: &FileChange) -> Result<Option<(usize, usize)>> {
    let old = match &c.old {
        Some(s) => s.load(repo)?,
        None => Vec::new(),
    };
    let new = match &c.new {
        Some(s) => s.load(repo)?,
        None => Vec::new(),
    };
    if diff::is_binary(&old) || diff::is_binary(&new) {
        return Ok(None);
    }
    let edits = diff::diff(&diff::lines(&old), &diff::lines(&new));
    let ins = edits.iter().filter(|e| e.op == diff::Op::Insert).count();
    let del = edits.iter().filter(|e| e.op == diff::Op::Delete).count();
    Ok(Some((ins, del)))
}

pub fn print_stat(repo: &Repo, list: &[FileChange], out: Out) -> Result<()> {
    let mut rows = Vec::new();
    for c in list {
        rows.push((c.path.clone(), count_changes(repo, c)?));
    }
    let name_w = rows.iter().map(|(p, _)| p.len()).max().unwrap_or(0);
    let max_total = rows.iter().filter_map(|(_, s)| s.map(|(i, d)| i + d)).max().unwrap_or(0);
    let count_w = max_total.to_string().len();
    let bar_room = 80usize.saturating_sub(name_w + count_w + 6).clamp(10, 50);
    let (mut files, mut ins_total, mut del_total) = (0, 0, 0);
    for (path, s) in &rows {
        files += 1;
        match s {
            None => writeln!(out, " {path:name_w$} | Bin")?,
            Some((ins, del)) => {
                ins_total += ins;
                del_total += del;
                let total = ins + del;
                let (mut pi, mut pd) = (*ins, *del);
                if max_total > bar_room {
                    let scale = |n: usize| if n == 0 { 0 } else { (n * bar_room / max_total).max(1) };
                    pi = scale(*ins);
                    pd = scale(*del);
                }
                writeln!(
                    out,
                    " {path:name_w$} | {total:>count_w$} {}{}",
                    paint(color::GREEN, &"+".repeat(pi)),
                    paint(color::RED, &"-".repeat(pd))
                )?;
            }
        }
    }
    let plural = |n: usize, one: &str, many: &str| if n == 1 { one.to_string() } else { many.to_string() };
    let mut summary = format!(" {files} {} changed", plural(files, "file", "files"));
    if ins_total > 0 || del_total == 0 {
        summary.push_str(&format!(", {ins_total} {}(+)", plural(ins_total, "insertion", "insertions")));
    }
    if del_total > 0 || ins_total == 0 {
        summary.push_str(&format!(", {del_total} {}(-)", plural(del_total, "deletion", "deletions")));
    }
    writeln!(out, "{summary}")?;
    Ok(())
}

/// oid -> ref labels for `log --decorate`, e.g. ["HEAD -> main", "tag: v1", "origin/main"].
pub fn decorations(repo: &Repo) -> Result<HashMap<Oid, Vec<String>>> {
    let mut map: HashMap<Oid, Vec<String>> = HashMap::new();
    let head_target = repo.refs.head_target()?;
    let head = repo.head()?;
    let mut push = |oid: Oid, label: String| map.entry(oid).or_default().push(label);
    if let (Some(h), None) = (head, &head_target) {
        push(h, paint(color::BOLD_CYAN, "HEAD"));
    }
    for (name, oid) in repo.refs.list("refs/heads/")? {
        let short = name.trim_start_matches("refs/heads/").to_string();
        if head_target.as_deref() == Some(name.as_str()) {
            push(oid, format!("{} {}", paint(color::BOLD_CYAN, "HEAD ->"), paint(color::BOLD_GREEN, &short)));
        } else {
            push(oid, paint(color::BOLD_GREEN, &short));
        }
    }
    for (name, oid) in repo.refs.list("refs/remotes/")? {
        if name.ends_with("/HEAD") {
            continue;
        }
        push(oid, paint(color::BOLD_RED, name.trim_start_matches("refs/remotes/")));
    }
    for (name, oid) in repo.refs.list("refs/tags/")? {
        let target = crate::revparse::peel_tags(repo, oid).unwrap_or(oid);
        push(target, paint(color::BOLD_YELLOW, &format!("tag: {}", name.trim_start_matches("refs/tags/"))));
    }
    Ok(map)
}

pub fn decoration_suffix(decor: &HashMap<Oid, Vec<String>>, oid: &Oid) -> String {
    match decor.get(oid) {
        Some(labels) if !labels.is_empty() => {
            let sep = paint(color::YELLOW, ", ");
            format!(" {}{}{}", paint(color::YELLOW, "("), labels.join(&sep), paint(color::YELLOW, ")"))
        }
        _ => String::new(),
    }
}

/// Read every line of a file given by the user, or stdin for "-".
pub fn read_message_file(path: &str) -> Result<String> {
    if path == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s)?;
        Ok(s)
    } else {
        Ok(fs::read_to_string(path)?)
    }
}

/// Git's message cleanup: strip trailing whitespace, '#' comment lines and surrounding blank lines.
pub fn clean_message(raw: &str) -> String {
    let lines: Vec<&str> = raw.lines().filter(|l| !l.starts_with('#')).map(|l| l.trim_end()).collect();
    let mut out: Vec<&str> = Vec::new();
    for l in lines {
        if l.is_empty() && out.last().is_none_or(|p| p.is_empty()) {
            continue; // drop leading blanks and collapse runs
        }
        out.push(l);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    if out.is_empty() { String::new() } else { out.join("\n") + "\n" }
}
