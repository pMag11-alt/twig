//! log, show and diff.

use std::collections::HashSet;

use anyhow::{Result, bail};

use super::{DiffOptions, Out, decoration_suffix, decorations, print_diff, snapshot_of_index, snapshot_of_tree, snapshot_of_worktree};
use crate::color::{self, paint};
use crate::date;
use crate::merge::walk_history;
use crate::object::{Commit, Kind};
use crate::oid::Oid;
use crate::repo::Repo;
use crate::revparse;

pub enum Format {
    Medium,
    Oneline,
    /// A `--format` template with %H, %h, %s, %an, ... placeholders.
    Template(String),
}

pub struct LogOptions {
    pub format: Format,
    pub max_count: Option<usize>,
    pub all: bool,
    pub patch: bool,
    pub stat: bool,
    pub decorate: bool,
    pub revs: Vec<String>,
}

/// Expand a `--format` template for one commit.
pub fn expand_format(fmt: &str, oid: &Oid, c: &Commit) -> String {
    let mut out = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let mut code = String::new();
        // two-letter placeholders start with a, c or P
        if let Some(&n) = chars.peek() {
            code.push(n);
            chars.next();
            if matches!(n, 'a' | 'c')
                && let Some(&m) = chars.peek()
            {
                code.push(m);
                chars.next();
            }
        }
        let body = c.message.split_once('\n').map(|(_, b)| b.trim_start_matches('\n')).unwrap_or("");
        let v = match code.as_str() {
            "H" => oid.to_hex(),
            "h" => oid.short(),
            "T" => c.tree.to_hex(),
            "t" => c.tree.short(),
            "P" => c.parents.iter().map(|p| p.to_hex()).collect::<Vec<_>>().join(" "),
            "p" => c.parents.iter().map(|p| p.short()).collect::<Vec<_>>().join(" "),
            "s" => c.summary().to_string(),
            "b" => body.to_string(),
            "B" => c.message.clone(),
            "n" => "\n".to_string(),
            "%" => "%".to_string(),
            "an" => c.author.name.clone(),
            "ae" => c.author.email.clone(),
            "ad" => date::format_default(&c.author),
            "ai" => date::format_iso(&c.author),
            "at" => c.author.time.to_string(),
            "ar" => date::format_relative(c.author.time, date::now()),
            "cn" => c.committer.name.clone(),
            "ce" => c.committer.email.clone(),
            "cd" => date::format_default(&c.committer),
            "ci" => date::format_iso(&c.committer),
            "ct" => c.committer.time.to_string(),
            "cr" => date::format_relative(c.committer.time, date::now()),
            other => format!("%{other}"),
        };
        out.push_str(&v);
    }
    out
}

/// Starting points and exclusions from revision arguments ("main", "A..B", "^A").
fn parse_range(repo: &Repo, revs: &[String], all: bool) -> Result<(Vec<Oid>, HashSet<Oid>)> {
    let mut include = Vec::new();
    let mut exclude_tips = Vec::new();
    for r in revs {
        if let Some((a, b)) = r.split_once("..") {
            exclude_tips.push(revparse::commit(repo, if a.is_empty() { "HEAD" } else { a })?);
            include.push(revparse::commit(repo, if b.is_empty() { "HEAD" } else { b })?);
        } else if let Some(x) = r.strip_prefix('^') {
            exclude_tips.push(revparse::commit(repo, x)?);
        } else {
            include.push(revparse::commit(repo, r)?);
        }
    }
    if all {
        for prefix in ["refs/heads/", "refs/remotes/", "refs/tags/"] {
            for (_, oid) in repo.refs.list(prefix)? {
                if let Ok(c) = revparse::peel(repo, oid, Kind::Commit) {
                    include.push(c);
                }
            }
        }
        if let Some(h) = repo.head()? {
            include.push(h);
        }
    }
    if include.is_empty() {
        match repo.head()? {
            Some(h) => include.push(h),
            None => {
                let branch = repo.current_branch()?.unwrap_or_default();
                bail!("your current branch '{branch}' does not have any commits yet");
            }
        }
    }
    let mut excluded = HashSet::new();
    if !exclude_tips.is_empty() {
        walk_history(repo, &exclude_tips, |c, _| {
            excluded.insert(c);
            Ok(true)
        })?;
    }
    Ok((include, excluded))
}

pub fn log(repo: &Repo, opts: LogOptions, out: Out) -> Result<()> {
    let (starts, excluded) = parse_range(repo, &opts.revs, opts.all)?;
    let decor = if opts.decorate { decorations(repo)? } else { Default::default() };
    let mut shown = 0;
    let mut first = true;
    walk_history(repo, &starts, |oid, c| {
        if excluded.contains(&oid) {
            return Ok(true);
        }
        if opts.max_count.is_some_and(|n| shown >= n) {
            return Ok(false);
        }
        shown += 1;
        match &opts.format {
            Format::Oneline => {
                writeln!(out, "{}{} {}", paint(color::YELLOW, &oid.short()), decoration_suffix(&decor, &oid), c.summary())?;
            }
            Format::Template(t) => writeln!(out, "{}", expand_format(t, &oid, c))?,
            Format::Medium => {
                if !first {
                    writeln!(out)?;
                }
                print_commit_header(&oid, c, &decor, out)?;
            }
        }
        first = false;
        if opts.patch || opts.stat {
            let blank = matches!(opts.format, Format::Medium);
            print_commit_diff(repo, c, DiffOptions { stat: opts.stat, context: 3, ..Default::default() }, blank, out)?;
        }
        Ok(true)
    })
}

fn print_commit_header(oid: &Oid, c: &Commit, decor: &std::collections::HashMap<Oid, Vec<String>>, out: Out) -> Result<()> {
    writeln!(out, "{}{}", paint(color::YELLOW, &format!("commit {oid}")), decoration_suffix(decor, oid))?;
    if c.parents.len() > 1 {
        writeln!(out, "Merge: {}", c.parents.iter().map(|p| p.short()).collect::<Vec<_>>().join(" "))?;
    }
    writeln!(out, "Author: {} <{}>", c.author.name, c.author.email)?;
    writeln!(out, "Date:   {}", date::format_default(&c.author))?;
    writeln!(out)?;
    for line in c.message.trim_end_matches('\n').split('\n') {
        writeln!(out, "    {line}")?;
    }
    Ok(())
}

/// Diff of a commit against its first parent (nothing for merges, like `git log -p`).
fn print_commit_diff(repo: &Repo, c: &Commit, opts: DiffOptions, leading_blank: bool, out: Out) -> Result<()> {
    if c.parents.len() > 1 {
        return Ok(());
    }
    let new = snapshot_of_tree(&repo.flatten_tree(&c.tree)?);
    let old = match c.parents.first() {
        Some(p) => snapshot_of_tree(&repo.flatten_commit(Some(*p))?),
        None => Default::default(),
    };
    if leading_blank && old.len() + new.len() > 0 {
        writeln!(out)?;
    }
    print_diff(repo, &old, &new, opts, out)
}

pub fn show(repo: &Repo, rev: &str, opts: DiffOptions, out: Out) -> Result<()> {
    let mut oid = revparse::resolve(repo, rev)?;
    loop {
        let (kind, data) = repo.odb.read(&oid)?;
        match kind {
            Kind::Tag => {
                let tag = crate::object::Tag::parse(&data)?;
                super::plumbing::describe_tag(&tag, out)?;
                oid = tag.object;
            }
            Kind::Commit => {
                let c = Commit::parse(&data)?;
                let decor = if color::enabled() { decorations(repo)? } else { Default::default() };
                print_commit_header(&oid, &c, &decor, out)?;
                return print_commit_diff(repo, &c, opts, true, out);
            }
            Kind::Tree => {
                writeln!(out, "tree {rev}\n")?;
                for e in repo.odb.read_tree(&oid)? {
                    writeln!(out, "{}{}", e.name, if e.is_tree() { "/" } else { "" })?;
                }
                return Ok(());
            }
            Kind::Blob => {
                out.write_all(&data)?;
                return Ok(());
            }
        }
    }
}

/// `diff` (index vs work tree), `diff --cached [rev]` (rev or HEAD vs index),
/// `diff <rev>` (rev vs work tree) and `diff <a> <b>` (two commits).
pub fn diff(repo: &Repo, cached: bool, revs: &[String], opts: DiffOptions, out: Out) -> Result<()> {
    let index = repo.load_index()?;
    let tree_of = |rev: &str| -> Result<super::Snapshot> {
        let t = revparse::peel(repo, revparse::resolve(repo, rev)?, Kind::Tree)?;
        Ok(snapshot_of_tree(&repo.flatten_tree(&t)?))
    };
    let (old, new) = match (cached, revs) {
        (_, [range]) if range.contains("..") => {
            let (a, b) = range.split_once("..").unwrap();
            (tree_of(if a.is_empty() { "HEAD" } else { a })?, tree_of(if b.is_empty() { "HEAD" } else { b })?)
        }
        (false, []) => (snapshot_of_index(&index), snapshot_of_worktree(repo, &index)?),
        (true, []) => {
            let head = match repo.head()? {
                Some(h) => snapshot_of_tree(&repo.flatten_commit(Some(h))?),
                None => Default::default(),
            };
            (head, snapshot_of_index(&index))
        }
        (true, [rev]) => (tree_of(rev)?, snapshot_of_index(&index)),
        (false, [rev]) => (tree_of(rev)?, snapshot_of_worktree(repo, &index)?),
        (false, [a, b]) => (tree_of(a)?, tree_of(b)?),
        _ => bail!("usage: twig diff [--cached] [<commit> [<commit>]]"),
    };
    print_diff(repo, &old, &new, opts, out)
}
