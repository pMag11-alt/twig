//! branch, switch, checkout, restore, reset, tag and merge.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Result, bail};

use super::{Out, changes, print_stat, snapshot_of_tree};
use crate::color::{self, paint};
use crate::index::Entry;
use crate::merge;
use crate::object::{Commit, Kind, Tag};
use crate::oid::Oid;
use crate::refs::valid_name;
use crate::repo::{Repo, Role};
use crate::revparse;
use crate::worktree;

pub struct BranchOptions {
    pub name: Option<String>,
    pub start: Option<String>,
    pub delete: bool,
    pub force_delete: bool,
    pub rename: bool,
    pub verbose: bool,
    pub all: bool,
}

pub fn branch(repo: &Repo, o: BranchOptions, out: Out) -> Result<()> {
    let current = repo.current_branch()?;
    if o.delete || o.force_delete {
        let Some(name) = o.name else { bail!("branch name required") };
        if current.as_deref() == Some(name.as_str()) {
            bail!("cannot delete branch '{name}' used by the current HEAD");
        }
        let full = format!("refs/heads/{name}");
        let Some(oid) = repo.refs.resolve(&full)? else { bail!("branch '{name}' not found") };
        if !o.force_delete
            && let Some(head) = repo.head()?
            && !merge::is_ancestor(repo, oid, head)?
        {
            bail!("the branch '{name}' is not fully merged.\nIf you are sure you want to delete it, run 'twig branch -D {name}'");
        }
        repo.refs.delete(&full)?;
        writeln!(out, "Deleted branch {name} (was {}).", oid.short())?;
        return Ok(());
    }
    if o.rename {
        let (old, new) = match (o.name, o.start) {
            (Some(new), None) => (current.clone().ok_or_else(|| anyhow::anyhow!("not on a branch"))?, new),
            (Some(old), Some(new)) => (old, new),
            _ => bail!("usage: twig branch -m [<old>] <new>"),
        };
        if !valid_name(&new) {
            bail!("'{new}' is not a valid branch name");
        }
        let Some(oid) = repo.refs.resolve(&format!("refs/heads/{old}"))? else { bail!("no branch named '{old}'") };
        if repo.refs.exists(&format!("refs/heads/{new}"))? {
            bail!("a branch named '{new}' already exists");
        }
        repo.refs.update(
            &format!("refs/heads/{new}"),
            &oid,
            &format!("branch: renamed refs/heads/{old} to refs/heads/{new}"),
            &repo.reflog_who(),
        )?;
        repo.refs.delete(&format!("refs/heads/{old}"))?;
        if current.as_deref() == Some(old.as_str()) {
            repo.refs.set_symbolic("HEAD", &format!("refs/heads/{new}"))?;
        }
        return Ok(());
    }
    if let Some(name) = o.name {
        if !valid_name(&name) {
            bail!("'{name}' is not a valid branch name");
        }
        let full = format!("refs/heads/{name}");
        if repo.refs.exists(&full)? {
            bail!("a branch named '{name}' already exists");
        }
        let start = revparse::commit(repo, o.start.as_deref().unwrap_or("HEAD"))?;
        repo.refs.update(&full, &start, &format!("branch: Created from {}", o.start.as_deref().unwrap_or("HEAD")), &repo.reflog_who())?;
        return Ok(());
    }
    // list
    let mut rows: Vec<(String, Oid, bool, bool)> = Vec::new();
    if current.is_none()
        && let Some(h) = repo.head()?
    {
        rows.push((format!("(HEAD detached at {})", h.short()), h, true, false));
    }
    for (name, oid) in repo.refs.list("refs/heads/")? {
        let short = name.trim_start_matches("refs/heads/").to_string();
        let is_current = current.as_deref() == Some(short.as_str());
        rows.push((short, oid, is_current, false));
    }
    if o.all {
        for (name, oid) in repo.refs.list("refs/remotes/")? {
            if !name.ends_with("/HEAD") {
                rows.push((format!("remotes/{}", name.trim_start_matches("refs/remotes/")), oid, false, true));
            }
        }
    }
    let width = rows.iter().map(|r| r.0.len()).max().unwrap_or(0);
    for (name, oid, is_current, remote) in rows {
        let marker = if is_current { "* " } else { "  " };
        let painted = if is_current {
            paint(color::GREEN, &format!("{name:width$}"))
        } else if remote {
            paint(color::RED, &format!("{name:width$}"))
        } else {
            format!("{name:width$}")
        };
        if o.verbose {
            let summary = repo.odb.read_commit(&oid).map(|c| c.summary().to_string()).unwrap_or_default();
            writeln!(out, "{marker}{painted} {} {summary}", paint(color::YELLOW, &oid.short()))?;
        } else {
            writeln!(out, "{marker}{}", painted.trim_end())?;
        }
    }
    Ok(())
}

/// Check out `target` (a commit) into the work tree and index, then point HEAD at `head_ref`
/// (a branch) or detach it.
fn move_head(repo: &Repo, target: Oid, branch: Option<&str>, force: bool, reflog: &str) -> Result<()> {
    let mut index = repo.load_index()?;
    let from = repo.flatten_commit(repo.head()?)?;
    let to = repo.flatten_commit(Some(target))?;
    worktree::switch_trees(repo, &mut index, &from, &to, force)?;
    index.save(&repo.index_path())?;
    let who = repo.reflog_who();
    match branch {
        Some(b) => {
            repo.refs.set_symbolic("HEAD", &format!("refs/heads/{b}"))?;
            if !who.is_empty() {
                let path = repo.git_dir.join("logs/HEAD");
                fs::create_dir_all(path.parent().unwrap())?;
                let old = from_oid(repo);
                let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
                writeln!(f, "{old} {target} {who}\t{reflog}")?;
            }
        }
        None => repo.refs.set_detached("HEAD", &target)?,
    }
    Ok(())
}

fn from_oid(repo: &Repo) -> Oid {
    repo.head().ok().flatten().unwrap_or(Oid::ZERO)
}

pub struct SwitchOptions {
    pub target: Option<String>,
    pub create: Option<String>,
    pub detach: bool,
    pub force: bool,
    /// `checkout <commit>` detaches HEAD without being asked; `switch` insists on --detach.
    pub implicit_detach: bool,
}

pub fn switch(repo: &Repo, o: SwitchOptions, out: Out) -> Result<()> {
    let from_name = repo.current_branch()?.unwrap_or_else(|| repo.head().ok().flatten().map(|h| h.short()).unwrap_or_default());
    if let Some(new) = o.create {
        if !valid_name(&new) {
            bail!("'{new}' is not a valid branch name");
        }
        let full = format!("refs/heads/{new}");
        if repo.refs.exists(&full)? {
            bail!("a branch named '{new}' already exists");
        }
        let start = match &o.target {
            Some(t) => Some(revparse::commit(repo, t)?),
            None => repo.head()?,
        };
        match start {
            Some(start) => {
                move_head(repo, start, Some(&new), o.force, &format!("checkout: moving from {from_name} to {new}"))?;
                repo.refs.update(
                    &full,
                    &start,
                    &format!("branch: Created from {}", o.target.as_deref().unwrap_or("HEAD")),
                    &repo.reflog_who(),
                )?;
            }
            None => repo.refs.set_symbolic("HEAD", &full)?, // new branch on an unborn repo
        }
        if let Some(t) = &o.target {
            set_upstream_if_remote(repo, &new, t)?;
        }
        writeln!(out, "Switched to a new branch '{new}'")?;
        return Ok(());
    }
    let Some(target) = o.target else { bail!("missing branch or commit argument") };
    if !o.detach && repo.refs.exists(&format!("refs/heads/{target}"))? {
        if repo.current_branch()?.as_deref() == Some(target.as_str()) {
            writeln!(out, "Already on '{target}'")?;
            return Ok(());
        }
        let oid = repo.refs.resolve(&format!("refs/heads/{target}"))?.unwrap();
        move_head(repo, oid, Some(&target), o.force, &format!("checkout: moving from {from_name} to {target}"))?;
        writeln!(out, "Switched to branch '{target}'")?;
        if let Some((up, ahead, behind)) = super::staging::upstream_info(repo, &target)? {
            match (ahead, behind) {
                (0, 0) => writeln!(out, "Your branch is up to date with '{up}'.")?,
                (a, 0) => writeln!(out, "Your branch is ahead of '{up}' by {a} commit{}.", if a == 1 { "" } else { "s" })?,
                (0, b) => writeln!(
                    out,
                    "Your branch is behind '{up}' by {b} commit{}, and can be fast-forwarded.",
                    if b == 1 { "" } else { "s" }
                )?,
                _ => writeln!(out, "Your branch and '{up}' have diverged.")?,
            }
        }
        return Ok(());
    }
    // "switch feature" with only origin/feature: create a tracking branch, like Git does
    if !o.detach
        && let Some(oid) = repo.refs.resolve(&format!("refs/remotes/origin/{target}"))?
    {
        move_head(repo, oid, Some(&target), o.force, &format!("checkout: moving from {from_name} to {target}"))?;
        repo.refs.update(&format!("refs/heads/{target}"), &oid, &format!("branch: Created from origin/{target}"), &repo.reflog_who())?;
        set_upstream_if_remote(repo, &target, &format!("origin/{target}"))?;
        writeln!(out, "branch '{target}' set up to track 'origin/{target}'.")?;
        writeln!(out, "Switched to a new branch '{target}'")?;
        return Ok(());
    }
    let oid = revparse::commit(repo, &target)?;
    if !o.detach && !o.implicit_detach {
        bail!("a branch is expected, got '{target}'\nhint: If you want to detach HEAD at the commit, try again with the --detach option.");
    }
    move_head(repo, oid, None, o.force, &format!("checkout: moving from {from_name} to {target}"))?;
    let c = repo.odb.read_commit(&oid)?;
    writeln!(out, "HEAD is now at {} {}", oid.short(), c.summary())?;
    Ok(())
}

fn set_upstream_if_remote(repo: &Repo, branch: &str, start: &str) -> Result<()> {
    let name = start.trim_start_matches("refs/remotes/");
    if let Some((remote, rbranch)) = name.split_once('/')
        && repo.refs.exists(&format!("refs/remotes/{remote}/{rbranch}"))?
    {
        let cfg = repo.git_dir.join("config");
        crate::config::set(&cfg, &format!("branch.{branch}.remote"), remote)?;
        crate::config::set(&cfg, &format!("branch.{branch}.merge"), &format!("refs/heads/{rbranch}"))?;
    }
    Ok(())
}

/// Restore files in the work tree (from the index, or `source`), or with `staged`, restore
/// index entries from HEAD (or `source`).
pub fn restore(repo: &Repo, paths: &[PathBuf], staged: bool, worktree_too: bool, source: Option<&str>, out: Out) -> Result<()> {
    let _ = out;
    if paths.is_empty() {
        bail!("you must specify path(s) to restore");
    }
    let mut index = repo.load_index()?;
    let specs: Vec<String> = paths.iter().map(|p| repo.rel_path(p)).collect::<Result<_>>()?;
    let matches = |p: &str| specs.iter().any(|s| s.is_empty() || p == s || p.starts_with(&format!("{s}/")));
    let source_tree = match source {
        Some(rev) => Some(repo.flatten_tree(&revparse::peel(repo, revparse::resolve(repo, rev)?, Kind::Tree)?)?),
        None if staged => Some(repo.flatten_commit(repo.head()?)?),
        None => None,
    };
    let mut any = false;
    if staged {
        let tree = source_tree.as_ref().unwrap();
        let mut paths: std::collections::BTreeSet<String> =
            index.entries().iter().filter(|e| matches(&e.path)).map(|e| e.path.clone()).collect();
        paths.extend(tree.keys().filter(|p| matches(p)).cloned());
        for p in paths {
            any = true;
            match tree.get(&p) {
                Some(&(m, oid)) => {
                    // keep stat info if the content matches, so the file doesn't look modified
                    let mut e = index.get(&p).cloned().unwrap_or_else(|| Entry::new(&p, oid, m));
                    if e.oid != oid || e.mode != m {
                        e = Entry::new(&p, oid, m);
                    }
                    index.remove(&p);
                    index.add(e);
                }
                None => {
                    index.remove(&p);
                }
            }
        }
    }
    if worktree_too || !staged {
        let entries: Vec<(String, u32, Oid)> = match (&source_tree, staged) {
            (Some(t), _) => t.iter().filter(|(p, _)| matches(p)).map(|(p, &(m, o))| (p.clone(), m, o)).collect(),
            (None, _) => {
                index.entries().iter().filter(|e| e.stage == 0 && matches(&e.path)).map(|e| (e.path.clone(), e.mode, e.oid)).collect()
            }
        };
        for (p, m, oid) in entries {
            any = true;
            let md = worktree::write_file(repo, &p, m, &oid)?;
            if let Some(e) = index.get_mut(&p)
                && e.oid == oid
            {
                e.refresh_stat(&md);
            }
        }
    }
    if !any {
        bail!("pathspec '{}' did not match any file(s) known to twig", specs.join(" "));
    }
    index.save(&repo.index_path())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

pub fn reset(repo: &Repo, mode: ResetMode, target: Option<&str>, paths: &[PathBuf], out: Out) -> Result<()> {
    if !paths.is_empty() {
        return restore(repo, paths, true, false, target, out);
    }
    let target = revparse::commit(repo, target.unwrap_or("HEAD"))?;
    let old = repo.head()?;
    if let Some(o) = old {
        fs::write(repo.git_dir.join("ORIG_HEAD"), format!("{o}\n"))?;
    }
    let tree = repo.flatten_commit(Some(target))?;
    match mode {
        ResetMode::Soft => {
            if repo.git_dir.join("MERGE_HEAD").exists() {
                bail!("Cannot do a soft reset in the middle of a merge.");
            }
        }
        ResetMode::Mixed => {
            let old_index = repo.load_index()?;
            let mut index = crate::index::Index::default();
            for (p, &(m, oid)) in &tree {
                // carry over stat data for unchanged entries so status stays fast and accurate
                let e = match old_index.get(p) {
                    Some(e) if e.oid == oid && e.mode == m => e.clone(),
                    _ => Entry::new(p, oid, m),
                };
                index.add(e);
            }
            index.save(&repo.index_path())?;
        }
        ResetMode::Hard => {
            let mut index = repo.load_index()?;
            let from = repo.flatten_commit(old)?;
            worktree::switch_trees(repo, &mut index, &from, &tree, true)?;
            index.save(&repo.index_path())?;
        }
    }
    repo.refs.update("HEAD", &target, &format!("reset: moving to {}", target), &repo.reflog_who())?;
    for f in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
        let _ = fs::remove_file(repo.git_dir.join(f));
    }
    if mode == ResetMode::Hard {
        let c = repo.odb.read_commit(&target)?;
        writeln!(out, "HEAD is now at {} {}", target.short(), c.summary())?;
    } else if mode == ResetMode::Mixed {
        let index = repo.load_index()?;
        let st = worktree::status(repo, &index)?;
        if !st.unstaged.is_empty() {
            writeln!(out, "Unstaged changes after reset:")?;
            for (p, c) in &st.unstaged {
                writeln!(out, "{}\t{p}", c.code())?;
            }
        }
    }
    Ok(())
}

pub struct TagOptions {
    pub name: Option<String>,
    pub target: Option<String>,
    pub annotate: bool,
    pub message: Option<String>,
    pub delete: bool,
    pub force: bool,
}

pub fn tag(repo: &Repo, o: TagOptions, out: Out) -> Result<()> {
    let Some(name) = o.name else {
        for name in repo.refs.list("refs/tags/")?.keys() {
            writeln!(out, "{}", name.trim_start_matches("refs/tags/"))?;
        }
        return Ok(());
    };
    let full = format!("refs/tags/{name}");
    if o.delete {
        let Some(oid) = repo.refs.resolve(&full)? else { bail!("tag '{name}' not found.") };
        repo.refs.delete(&full)?;
        writeln!(out, "Deleted tag '{name}' (was {})", oid.short())?;
        return Ok(());
    }
    if !valid_name(&name) {
        bail!("'{name}' is not a valid tag name.");
    }
    if repo.refs.exists(&full)? && !o.force {
        bail!("tag '{name}' already exists");
    }
    let target = revparse::resolve(repo, o.target.as_deref().unwrap_or("HEAD"))?;
    let oid = if o.annotate || o.message.is_some() {
        let Some(msg) = o.message else { bail!("annotated tags need a message (-m)") };
        let (kind, _) = repo.odb.read(&target)?;
        let t = Tag {
            object: target,
            kind,
            name: name.clone(),
            tagger: Some(repo.identity(Role::Committer)?),
            message: super::clean_message(&msg),
        };
        repo.odb.write(Kind::Tag, &t.serialize())?
    } else {
        target
    };
    repo.refs.update(&full, &oid, "", "")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// merge
// ---------------------------------------------------------------------------

pub fn merge_abort(repo: &Repo, out: Out) -> Result<()> {
    if !repo.git_dir.join("MERGE_HEAD").exists() {
        bail!("There is no merge to abort (MERGE_HEAD missing).");
    }
    reset(repo, ResetMode::Hard, Some("HEAD"), &[], &mut std::io::sink())?;
    let _ = out;
    Ok(())
}

pub fn merge_cmd(repo: &Repo, rev: &str, message: Option<String>, no_ff: bool, out: Out) -> Result<()> {
    if repo.git_dir.join("MERGE_HEAD").exists() {
        bail!("You have not concluded your merge (MERGE_HEAD exists).\nPlease, commit your changes before you merge.");
    }
    let Some(head) = repo.head()? else { bail!("cannot merge into an empty branch") };
    let theirs = revparse::commit(repo, rev)?;
    let mut index = repo.load_index()?;
    let head_tree = repo.flatten_commit(Some(head))?;
    let st = worktree::status(repo, &index)?;
    if !st.staged.is_empty() || !st.conflicts.is_empty() {
        bail!("Your local changes would be overwritten by merge.\nPlease commit your changes or stash them before you merge.");
    }

    let bases = merge::merge_bases(repo, head, theirs)?;
    if bases.contains(&theirs) {
        writeln!(out, "Already up to date.")?;
        return Ok(());
    }
    let label = rev.to_string();
    if bases.contains(&head) && !no_ff {
        writeln!(out, "Updating {}..{}", head.short(), theirs.short())?;
        writeln!(out, "Fast-forward")?;
        let to = repo.flatten_commit(Some(theirs))?;
        worktree::switch_trees(repo, &mut index, &head_tree, &to, false)?;
        index.save(&repo.index_path())?;
        repo.refs.update("HEAD", &theirs, &format!("merge {label}: Fast-forward"), &repo.reflog_who())?;
        print_stat(repo, &changes(&snapshot_of_tree(&head_tree), &snapshot_of_tree(&to)), out)?;
        return Ok(());
    }

    let base_tree = repo.flatten_commit(bases.first().copied())?;
    let their_tree = repo.flatten_commit(Some(theirs))?;
    let result = merge::merge_trees(repo, &base_tree, &head_tree, &their_tree, "HEAD", &label)?;

    // Refuse if unstaged edits sit on files the merge needs to change.
    let racy = worktree::index_mtime(repo);
    let touched: Vec<&String> = result
        .clean
        .iter()
        .filter(|(p, v)| head_tree.get(*p) != Some(v))
        .map(|(p, _)| p)
        .chain(head_tree.keys().filter(|p| !result.clean.contains_key(*p)))
        .collect();
    let dirty: Vec<&String> = touched
        .into_iter()
        .filter(|p| index.get(p).is_some_and(|e| worktree::worktree_change(repo, e, racy).ok().flatten().is_some()))
        .collect();
    if !dirty.is_empty() {
        bail!(
            "Your local changes to the following files would be overwritten by merge:\n\t{}\nPlease commit your changes or stash them before you merge.",
            dirty.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n\t")
        );
    }

    for m in &result.messages {
        writeln!(out, "{m}")?;
    }
    // Clean paths: write what changed relative to HEAD.
    let conflicted: std::collections::HashSet<&String> = result.conflicts.iter().map(|c| &c.path).collect();
    for (p, &(m, oid)) in &result.clean {
        if head_tree.get(p) != Some(&(m, oid)) {
            let md = worktree::write_file(repo, p, m, &oid)?;
            let mut e = Entry::from_metadata(p, oid, &md);
            e.mode = m;
            index.add(e);
        }
    }
    for p in head_tree.keys() {
        if !result.clean.contains_key(p) && !conflicted.contains(p) {
            worktree::remove_file(repo, p)?;
            index.remove(p);
        }
    }
    for c in &result.conflicts {
        index.remove(&c.path);
        for (stage, side) in [(1u8, c.base), (2, c.ours), (3, c.theirs)] {
            if let Some((m, oid)) = side {
                let mut e = Entry::new(&c.path, oid, m);
                e.stage = stage;
                index.add(e);
            }
        }
        match &c.worktree {
            Some((m, data)) => {
                let full = repo.abs(&c.path);
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                if full.symlink_metadata().is_ok() {
                    fs::remove_file(&full)?;
                }
                if *m == crate::object::mode::SYMLINK {
                    std::os::unix::fs::symlink(String::from_utf8_lossy(data).as_ref(), &full)?;
                } else {
                    fs::write(&full, data)?;
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&full, fs::Permissions::from_mode(if *m == crate::object::mode::EXEC { 0o755 } else { 0o644 }))?;
                }
            }
            None => worktree::remove_file(repo, &c.path)?,
        }
    }
    index.save(&repo.index_path())?;

    let into = repo.current_branch()?.unwrap_or_else(|| "HEAD".into());
    let default_msg = match revparse::expand_ref(repo, rev)? {
        Some((full, _)) if full.starts_with("refs/heads/") => format!("Merge branch '{rev}'"),
        Some((full, _)) if full.starts_with("refs/remotes/") => format!("Merge remote-tracking branch '{rev}'"),
        Some((full, _)) if full.starts_with("refs/tags/") => format!("Merge tag '{rev}'"),
        _ => format!("Merge commit '{rev}'"),
    };
    let default_msg = if into == "main" || into == "master" { default_msg } else { format!("{default_msg} into {into}") };
    let msg = message.unwrap_or(default_msg);

    if !result.conflicts.is_empty() {
        fs::write(repo.git_dir.join("MERGE_HEAD"), format!("{theirs}\n"))?;
        let mut m = format!("{msg}\n\n# Conflicts:\n");
        for c in &result.conflicts {
            m.push_str(&format!("#\t{}\n", c.path));
        }
        fs::write(repo.git_dir.join("MERGE_MSG"), m)?;
        fs::write(repo.git_dir.join("MERGE_MODE"), if no_ff { "no-ff\n" } else { "" })?;
        writeln!(out, "Automatic merge failed; fix conflicts and then commit the result.")?;
        return Err(crate::Exit(1).into());
    }

    let tree = repo.write_index_tree(&index)?;
    let commit = Commit {
        tree,
        parents: vec![head, theirs],
        author: repo.identity(Role::Author)?,
        committer: repo.identity(Role::Committer)?,
        extra: Vec::new(),
        message: format!("{msg}\n"),
    };
    let oid = repo.odb.write(Kind::Commit, &commit.serialize())?;
    repo.refs.update("HEAD", &oid, &format!("merge {label}: Merge made by the 'twig' strategy."), &repo.reflog_who())?;
    writeln!(out, "Merge made by the 'twig' strategy.")?;
    print_stat(repo, &changes(&snapshot_of_tree(&head_tree), &snapshot_of_tree(&repo.flatten_tree(&tree)?)), out)?;
    Ok(())
}
