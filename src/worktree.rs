//! Comparing HEAD, the index and the files on disk; updating the work tree.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::ignore::Ignore;
use crate::index::{Entry, Index, file_mode};
use crate::object::{Kind, mode};
use crate::oid::Oid;
use crate::repo::{FlatTree, Repo};

/// Contents Git would store for the file at `path`: the bytes, or a symlink's target.
pub fn read_worktree_file(path: &Path, md: &fs::Metadata) -> Result<Vec<u8>> {
    if md.file_type().is_symlink() {
        Ok(fs::read_link(path)?.to_string_lossy().into_owned().into_bytes())
    } else {
        fs::read(path).with_context(|| format!("reading {}", path.display()))
    }
}

pub fn hash_worktree_file(path: &Path, md: &fs::Metadata) -> Result<Oid> {
    Ok(Oid::hash_object(Kind::Blob, &read_worktree_file(path, md)?))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    Added,
    Modified,
    Deleted,
    TypeChanged,
}

impl Change {
    pub fn code(self) -> char {
        match self {
            Change::Added => 'A',
            Change::Modified => 'M',
            Change::Deleted => 'D',
            Change::TypeChanged => 'T',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Change::Added => "new file:",
            Change::Modified => "modified:",
            Change::Deleted => "deleted:",
            Change::TypeChanged => "typechange:",
        }
    }
}

#[derive(Default, Debug)]
pub struct Status {
    /// HEAD vs index.
    pub staged: BTreeMap<String, Change>,
    /// Index vs work tree.
    pub unstaged: BTreeMap<String, Change>,
    /// Conflicted paths with their two-letter porcelain code ("UU", "AA", ...).
    pub conflicts: BTreeMap<String, &'static str>,
    /// Untracked files, and directories (ending in '/') with no tracked files in them.
    pub untracked: Vec<String>,
}

impl Status {
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.conflicts.is_empty()
    }

    /// `git status --porcelain` (v1) lines.
    pub fn porcelain(&self) -> Vec<String> {
        let mut paths: BTreeSet<&String> = self.staged.keys().collect();
        paths.extend(self.unstaged.keys());
        paths.extend(self.conflicts.keys());
        let mut out = Vec::new();
        for p in paths {
            if let Some(code) = self.conflicts.get(p) {
                out.push(format!("{code} {}", quote_path(p)));
                continue;
            }
            let x = self.staged.get(p).map_or(' ', |c| c.code());
            let y = self.unstaged.get(p).map_or(' ', |c| c.code());
            out.push(format!("{x}{y} {}", quote_path(p)));
        }
        out.extend(self.untracked.iter().map(|p| format!("?? {}", quote_path(p))));
        out
    }
}

/// Quote a path the way Git does when it contains special characters.
pub fn quote_path(p: &str) -> String {
    if !p.bytes().any(|b| b < 0x20 || b == b'"' || b == b'\\' || b >= 0x80) {
        return p.to_string();
    }
    let mut s = String::from("\"");
    for b in p.bytes() {
        match b {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            b'\t' => s.push_str("\\t"),
            b'\n' => s.push_str("\\n"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\{b:03o}")),
        }
    }
    s.push('"');
    s
}

/// Compare a file on disk with its index entry. None means unchanged.
pub fn worktree_change(repo: &Repo, entry: &Entry, racy_after: (u32, u32)) -> Result<Option<Change>> {
    let path = repo.abs(&entry.path);
    let md = match fs::symlink_metadata(&path) {
        Ok(md) if !md.is_dir() => md,
        Ok(_) => return Ok(Some(Change::Deleted)), // a directory now stands where the file was
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.kind() == std::io::ErrorKind::NotADirectory => {
            return Ok(Some(Change::Deleted));
        }
        Err(e) => return Err(e.into()),
    };
    if mode::is_gitlink(entry.mode) {
        return Ok(None);
    }
    let disk_mode = file_mode(&md);
    let is_link = |m: u32| m == mode::SYMLINK;
    if is_link(disk_mode) != is_link(entry.mode) {
        return Ok(Some(Change::TypeChanged));
    }
    let mode_differs = repo.file_mode_enabled() && disk_mode != entry.mode;
    // Trust cached stat data, except for "racily clean" entries written in the same instant as
    // the index itself, whose content could have changed without changing the stat data.
    let racy = entry.mtime >= racy_after;
    if !mode_differs && !racy && entry.stat_matches(&md) {
        return Ok(None);
    }
    if mode_differs || hash_worktree_file(&path, &md)? != entry.oid {
        return Ok(Some(Change::Modified));
    }
    Ok(None)
}

/// Modification time of the index file: entries modified at or after it are "racily clean".
pub fn index_mtime(repo: &Repo) -> (u32, u32) {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(repo.index_path()).map(|m| (m.mtime() as u32, m.mtime_nsec() as u32)).unwrap_or((u32::MAX, 0))
}

pub fn status(repo: &Repo, index: &Index) -> Result<Status> {
    let mut st = Status::default();
    let head = repo.flatten_commit(repo.head()?)?;
    let racy_after = index_mtime(repo);

    // conflicts: which stages exist for each path
    let mut stages: BTreeMap<&str, [bool; 3]> = BTreeMap::new();
    for e in index.entries().iter().filter(|e| e.stage != 0) {
        stages.entry(&e.path).or_default()[e.stage as usize - 1] = true;
    }
    for (path, s) in &stages {
        let code = match s {
            [true, true, true] => "UU",
            [false, true, true] => "AA",
            [true, true, false] => "UD",
            [true, false, true] => "DU",
            [false, true, false] => "AU",
            [false, false, true] => "UA",
            _ => "DD",
        };
        st.conflicts.insert(path.to_string(), code);
    }

    for e in index.entries().iter().filter(|e| e.stage == 0) {
        match head.get(&e.path) {
            None => {
                st.staged.insert(e.path.clone(), Change::Added);
            }
            Some(&(m, oid)) if oid != e.oid || m != e.mode => {
                let kind = if (m == mode::SYMLINK) != (e.mode == mode::SYMLINK) { Change::TypeChanged } else { Change::Modified };
                st.staged.insert(e.path.clone(), kind);
            }
            _ => {}
        }
        if let Some(c) = worktree_change(repo, e, racy_after)? {
            st.unstaged.insert(e.path.clone(), c);
        }
    }
    for path in head.keys() {
        if index.get(path).is_none() && !stages.contains_key(path.as_str()) {
            st.staged.insert(path.clone(), Change::Deleted);
        }
    }

    let mut ignore = Ignore::new(&repo.work_tree, &repo.git_dir);
    let tracked: BTreeSet<&str> = index.entries().iter().map(|e| e.path.as_str()).collect();
    collect_untracked(repo, "", index, &tracked, &mut ignore, &mut st.untracked)?;
    Ok(st)
}

fn sorted_dir(path: &Path) -> Result<Vec<(String, fs::Metadata)>> {
    let mut items = Vec::new();
    for e in fs::read_dir(path)? {
        let e = e?;
        items.push((e.file_name().to_string_lossy().into_owned(), e.path().symlink_metadata()?));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(items)
}

fn collect_untracked(
    repo: &Repo,
    dir: &str,
    index: &Index,
    tracked: &BTreeSet<&str>,
    ignore: &mut Ignore,
    out: &mut Vec<String>,
) -> Result<()> {
    for (name, md) in sorted_dir(&repo.work_tree.join(dir))? {
        if name == ".git" {
            continue;
        }
        let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
        if md.is_dir() {
            if ignore.is_ignored(&rel, true) {
                continue;
            }
            if index.tracks_under(&rel) {
                collect_untracked(repo, &rel, index, tracked, ignore, out)?;
            } else if has_unignored_file(repo, &rel, ignore)? {
                out.push(format!("{rel}/"));
            }
        } else if !tracked.contains(rel.as_str()) && !ignore.is_ignored(&rel, false) {
            out.push(rel);
        }
    }
    Ok(())
}

fn has_unignored_file(repo: &Repo, dir: &str, ignore: &mut Ignore) -> Result<bool> {
    if repo.work_tree.join(dir).join(".git").exists() {
        return Ok(true); // a nested repository shows up as an untracked directory
    }
    for (name, md) in sorted_dir(&repo.work_tree.join(dir))? {
        let rel = format!("{dir}/{name}");
        if md.is_dir() {
            if !ignore.is_ignored(&rel, true) && has_unignored_file(repo, &rel, ignore)? {
                return Ok(true);
            }
        } else if !ignore.is_ignored(&rel, false) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Every untracked, unignored file under `dir` (used by `add`), recursing into new directories.
pub fn untracked_files(repo: &Repo, dir: &str, index: &Index) -> Result<Vec<String>> {
    let mut ignore = Ignore::new(&repo.work_tree, &repo.git_dir);
    let mut out = Vec::new();
    walk_files(repo, dir, index, &mut ignore, &mut out)?;
    Ok(out)
}

fn walk_files(repo: &Repo, dir: &str, index: &Index, ignore: &mut Ignore, out: &mut Vec<String>) -> Result<()> {
    let full = repo.work_tree.join(dir);
    if !full.is_dir() {
        return Ok(());
    }
    for (name, md) in sorted_dir(&full)? {
        if name == ".git" {
            continue;
        }
        let rel = if dir.is_empty() { name } else { format!("{dir}/{name}") };
        if md.is_dir() {
            if !ignore.is_ignored(&rel, true) && !full.join(rel.rsplit('/').next().unwrap()).join(".git").exists() {
                walk_files(repo, &rel, index, ignore, out)?;
            }
        } else if index.get(&rel).is_none() && !ignore.is_ignored(&rel, false) {
            out.push(rel);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Writing the work tree
// ---------------------------------------------------------------------------

/// Write a blob to the work tree with the right file type and permissions.
pub fn write_file(repo: &Repo, path: &str, m: u32, oid: &Oid) -> Result<fs::Metadata> {
    let full = repo.abs(path);
    if let Some(parent) = full.parent() {
        // a file may stand where a directory is needed
        let mut p = parent;
        while p != repo.work_tree {
            if p.symlink_metadata().is_ok_and(|md| !md.is_dir()) {
                fs::remove_file(p)?;
                break;
            }
            p = p.parent().unwrap();
        }
        fs::create_dir_all(parent)?;
    }
    if let Ok(md) = full.symlink_metadata() {
        if md.is_dir() {
            fs::remove_dir_all(&full)?;
        } else {
            fs::remove_file(&full)?;
        }
    }
    if mode::is_gitlink(m) {
        fs::create_dir_all(&full)?;
        return Ok(full.symlink_metadata()?);
    }
    let data = repo.odb.read_kind(oid, Kind::Blob)?;
    if m == mode::SYMLINK {
        symlink(String::from_utf8_lossy(&data).as_ref(), &full)?;
    } else {
        fs::write(&full, &data)?;
        let perm = if m == mode::EXEC { 0o755 } else { 0o644 };
        fs::set_permissions(&full, fs::Permissions::from_mode(perm))?;
    }
    Ok(full.symlink_metadata()?)
}

/// Delete a tracked file and any directories that became empty.
pub fn remove_file(repo: &Repo, path: &str) -> Result<()> {
    let full = repo.abs(path);
    match full.symlink_metadata() {
        Ok(md) if md.is_dir() => {}
        Ok(_) => fs::remove_file(&full)?,
        Err(_) => {}
    }
    let mut dir = full.parent();
    while let Some(d) = dir {
        if d == repo.work_tree || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

/// Move the work tree and index from `from` (the current HEAD tree) to `to`, like
/// `git switch`. Local changes to files that differ between the two trees abort the switch
/// unless `force`; other local changes (staged or not) are carried over.
pub fn switch_trees(repo: &Repo, index: &mut Index, from: &FlatTree, to: &FlatTree, force: bool) -> Result<()> {
    if index.has_conflicts() && !force {
        bail!("you need to resolve your current index first");
    }
    let racy_after = index_mtime(repo);
    let mut changed: BTreeSet<&String> = BTreeSet::new();
    for (p, v) in from {
        if to.get(p) != Some(v) {
            changed.insert(p);
        }
    }
    for (p, v) in to {
        if from.get(p) != Some(v) {
            changed.insert(p);
        }
    }

    if !force {
        let mut dirty = Vec::new();
        let mut untracked_in_way = Vec::new();
        for p in &changed {
            match index.get(p) {
                Some(e) => {
                    let staged = from.get(*p) != Some(&(e.mode, e.oid));
                    if staged || worktree_change(repo, e, racy_after)?.is_some() {
                        dirty.push((*p).clone());
                    }
                }
                None => {
                    if from.contains_key(*p) {
                        // deleted from the index: fine unless the target wants the file back
                        // and something else sits on disk
                        if repo.abs(p).symlink_metadata().is_ok() && to.contains_key(*p) {
                            dirty.push((*p).clone());
                        }
                    } else if to.contains_key(*p) && repo.abs(p).symlink_metadata().is_ok() {
                        let md = repo.abs(p).symlink_metadata()?;
                        let same = !md.is_dir() && hash_worktree_file(&repo.abs(p), &md)? == to[*p].1;
                        if !same {
                            untracked_in_way.push((*p).clone());
                        }
                    }
                }
            }
        }
        if !dirty.is_empty() {
            bail!(
                "Your local changes to the following files would be overwritten by checkout:\n\t{}\n\
                 Please commit your changes or stash them before you switch branches.",
                dirty.join("\n\t")
            );
        }
        if !untracked_in_way.is_empty() {
            bail!(
                "The following untracked working tree files would be overwritten by checkout:\n\t{}\n\
                 Please move or remove them before you switch branches.",
                untracked_in_way.join("\n\t")
            );
        }
    }

    // removals first, so a file can turn into a directory and vice versa
    for p in &changed {
        if !to.contains_key(*p) {
            remove_file(repo, p)?;
            index.remove(p);
        }
    }
    for p in &changed {
        if let Some(&(m, oid)) = to.get(*p) {
            let md = write_file(repo, p, m, &oid)?;
            let mut e = Entry::from_metadata(p, oid, &md);
            e.mode = m;
            index.add(e);
        }
    }
    if force {
        // drop staged changes (and conflicts) to paths that didn't move
        let stale: Vec<String> = index
            .entries()
            .iter()
            .filter(|e| e.stage != 0 || (to.get(&e.path) != Some(&(e.mode, e.oid))))
            .map(|e| e.path.clone())
            .collect();
        for p in stale {
            index.remove(&p);
            match to.get(&p) {
                Some(&(m, oid)) => {
                    let md = write_file(repo, &p, m, &oid)?;
                    let mut e = Entry::from_metadata(&p, oid, &md);
                    e.mode = m;
                    index.add(e);
                }
                None => remove_file(repo, &p)?,
            }
        }
        // and restore unstaged edits to tracked files
        for e in index.entries().to_vec() {
            if worktree_change(repo, &e, racy_after)?.is_some() {
                let md = write_file(repo, &e.path, e.mode, &e.oid)?;
                if let Some(ie) = index.get_mut(&e.path) {
                    ie.refresh_stat(&md);
                }
            }
        }
    }
    Ok(())
}
