//! init, add, rm, status and commit.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::{Out, changes, clean_message, snapshot_of_index, snapshot_of_tree};
use crate::color::{self, paint};
use crate::ignore::Ignore;
use crate::index::{Entry, Index};
use crate::object::{Commit, Kind};
use crate::oid::Oid;
use crate::repo::{Repo, Role};
use crate::worktree::{self, Change};

pub fn init(path: &Path, branch: Option<&str>, quiet: bool, out: Out) -> Result<()> {
    let (repo, existed) = Repo::init(path, branch)?;
    if !quiet {
        let verb = if existed { "Reinitialized existing" } else { "Initialized empty" };
        writeln!(out, "{verb} Git repository in {}/", repo.git_dir.display())?;
    }
    Ok(())
}

/// Stage one file from disk. Skips hashing when the cached stat data says it's unchanged.
fn stage_file(repo: &Repo, index: &mut Index, path: &str, racy_after: (u32, u32)) -> Result<()> {
    let full = repo.abs(path);
    let md = fs::symlink_metadata(&full)?;
    if let Some(e) = index.get(path)
        && e.stat_matches(&md)
        && e.mtime < racy_after
    {
        return Ok(());
    }
    let data = worktree::read_worktree_file(&full, &md)?;
    let oid = repo.odb.write(Kind::Blob, &data)?;
    let mut entry = Entry::from_metadata(path, oid, &md);
    if !repo.file_mode_enabled()
        && let Some(old) = index.get(path)
    {
        entry.mode = old.mode;
    }
    index.add(entry);
    Ok(())
}

fn under(path: &str, dir: &str) -> bool {
    dir.is_empty() || path == dir || path.starts_with(&format!("{dir}/"))
}

pub fn add(repo: &Repo, paths: &[PathBuf], all: bool, force: bool, out: Out) -> Result<()> {
    let _ = out;
    let mut index = repo.load_index()?;
    let mut specs: Vec<String> = paths.iter().map(|p| repo.rel_path(p)).collect::<Result<_>>()?;
    if all && specs.is_empty() {
        specs.push(String::new());
    }
    if specs.is_empty() {
        bail!("Nothing specified, nothing added.\nhint: Maybe you wanted to say 'twig add .'?");
    }
    let mut ignore = Ignore::new(&repo.work_tree, &repo.git_dir);
    let racy_after = worktree::index_mtime(repo);
    for spec in specs {
        let full = repo.abs(&spec);
        let md = fs::symlink_metadata(&full).ok();
        let tracked: Vec<String> = index.entries().iter().filter(|e| under(&e.path, &spec)).map(|e| e.path.clone()).collect();
        match md {
            Some(md) if md.is_dir() => {
                // tracked files: stage modifications and deletions
                for p in &tracked {
                    if fs::symlink_metadata(repo.abs(p)).is_ok_and(|m| !m.is_dir()) {
                        stage_file(repo, &mut index, p, racy_after)?;
                    } else {
                        index.remove(p);
                    }
                }
                for p in worktree::untracked_files(repo, &spec, &index)? {
                    stage_file(repo, &mut index, &p, racy_after)?;
                }
            }
            Some(_) => {
                if tracked.is_empty() && !force && ignore.is_ignored(&spec, false) {
                    bail!(
                        "The following paths are ignored by one of your .gitignore files:\n{spec}\n\
                         hint: Use -f if you really want to add them."
                    );
                }
                stage_file(repo, &mut index, &spec, racy_after)?;
            }
            None => {
                if tracked.is_empty() {
                    bail!("pathspec '{}' did not match any files", spec);
                }
                for p in tracked {
                    index.remove(&p);
                }
            }
        }
    }
    index.save(&repo.index_path())
}

pub fn rm(repo: &Repo, paths: &[PathBuf], cached: bool, recursive: bool, force: bool, out: Out) -> Result<()> {
    let mut index = repo.load_index()?;
    let head = repo.flatten_commit(repo.head()?)?;
    let mut targets = Vec::new();
    for p in paths {
        let spec = repo.rel_path(p)?;
        let matched: Vec<String> = index.entries().iter().filter(|e| under(&e.path, &spec)).map(|e| e.path.clone()).collect();
        if matched.is_empty() {
            bail!("pathspec '{spec}' did not match any files");
        }
        if matched.iter().any(|m| *m != spec) && !recursive {
            bail!("not removing '{spec}' recursively without -r");
        }
        targets.extend(matched);
    }
    targets.dedup();
    if !force {
        let racy = (0, 0); // always compare contents
        for p in &targets {
            let Some(e) = index.get(p) else { continue };
            let staged = head.get(p) != Some(&(e.mode, e.oid));
            let modified = worktree::worktree_change(repo, e, racy)?.is_some_and(|c| c != Change::Deleted);
            if (staged && modified) || (!cached && (staged || modified)) {
                bail!(
                    "the following file has {}:\n    {p}\n(use --cached to keep the file, or -f to force removal)",
                    if staged { "changes staged in the index" } else { "local modifications" }
                );
            }
        }
    }
    for p in &targets {
        index.remove(p);
        if !cached {
            worktree::remove_file(repo, p)?;
        }
        writeln!(out, "rm '{p}'")?;
    }
    index.save(&repo.index_path())
}

/// How far `branch` is ahead of / behind its configured upstream.
pub fn upstream_info(repo: &Repo, branch: &str) -> Result<Option<(String, usize, usize)>> {
    let (Some(remote), Some(merge)) =
        (repo.config.get(&format!("branch.{branch}.remote")), repo.config.get(&format!("branch.{branch}.merge")))
    else {
        return Ok(None);
    };
    let upstream_name = format!("{remote}/{}", merge.trim_start_matches("refs/heads/"));
    let Some(up) = repo.refs.resolve(&format!("refs/remotes/{upstream_name}"))? else { return Ok(None) };
    let Some(local) = repo.refs.resolve(&format!("refs/heads/{branch}"))? else { return Ok(None) };
    let count_only_in = |a: Oid, b: Oid| -> Result<usize> {
        let mut excl = std::collections::HashSet::new();
        crate::merge::walk_history(repo, &[b], |c, _| {
            excl.insert(c);
            Ok(true)
        })?;
        let mut n = 0;
        crate::merge::walk_history(repo, &[a], |c, _| {
            if !excl.contains(&c) {
                n += 1;
            }
            Ok(true)
        })?;
        Ok(n)
    };
    Ok(Some((upstream_name, count_only_in(local, up)?, count_only_in(up, local)?)))
}

pub fn status(repo: &Repo, short: bool, porcelain: bool, out: Out) -> Result<()> {
    let index = repo.load_index()?;
    let st = worktree::status(repo, &index)?;
    if short || porcelain {
        for line in st.porcelain() {
            if porcelain || !color::enabled() {
                writeln!(out, "{line}")?;
            } else {
                let (x, rest) = line.split_at(1);
                let (y, path) = rest.split_at(1);
                let xc = if x == "?" { color::RED } else { color::GREEN };
                writeln!(out, "{}{}{path}", paint(xc, x), paint(color::RED, y))?;
            }
        }
        return Ok(());
    }

    match repo.current_branch()? {
        Some(b) => {
            writeln!(out, "On branch {b}")?;
            if let Some((up, ahead, behind)) = upstream_info(repo, &b)? {
                let msg = match (ahead, behind) {
                    (0, 0) => format!("Your branch is up to date with '{up}'."),
                    (a, 0) => format!("Your branch is ahead of '{up}' by {a} commit{}.", if a == 1 { "" } else { "s" }),
                    (0, b) => {
                        format!("Your branch is behind '{up}' by {b} commit{}, and can be fast-forwarded.", if b == 1 { "" } else { "s" })
                    }
                    (a, b) => format!("Your branch and '{up}' have diverged,\nand have {a} and {b} different commits each, respectively."),
                };
                writeln!(out, "{msg}")?;
            }
        }
        None => writeln!(out, "HEAD detached at {}", repo.head()?.map(|o| o.short()).unwrap_or_default())?,
    }
    if repo.head()?.is_none() {
        writeln!(out, "\nNo commits yet")?;
    }
    if repo.git_dir.join("MERGE_HEAD").exists() {
        if st.conflicts.is_empty() {
            writeln!(out, "\nAll conflicts fixed but you are still merging.\n  (use \"twig commit\" to conclude merge)")?;
        } else {
            writeln!(
                out,
                "\nYou have unmerged paths.\n  (fix conflicts and run \"twig commit\")\n  (use \"twig merge --abort\" to abort the merge)"
            )?;
        }
    }
    let section = |out: Out, title: &str, hints: &[&str]| -> Result<()> {
        writeln!(out, "\n{title}")?;
        for h in hints {
            writeln!(out, "  ({h})")?;
        }
        Ok(())
    };
    if !st.staged.is_empty() {
        section(out, "Changes to be committed:", &["use \"twig restore --staged <file>...\" to unstage"])?;
        for (p, c) in &st.staged {
            writeln!(out, "\t{}", paint(color::GREEN, &format!("{:<12}{p}", c.label())))?;
        }
    }
    if !st.conflicts.is_empty() {
        section(out, "Unmerged paths:", &["use \"twig add <file>...\" to mark resolution"])?;
        for (p, code) in &st.conflicts {
            let label = match *code {
                "UU" => "both modified:",
                "AA" => "both added:",
                "UD" => "deleted by them:",
                "DU" => "deleted by us:",
                "AU" => "added by us:",
                "UA" => "added by them:",
                _ => "both deleted:",
            };
            writeln!(out, "\t{}", paint(color::RED, &format!("{label:<17}{p}")))?;
        }
    }
    if !st.unstaged.is_empty() {
        section(
            out,
            "Changes not staged for commit:",
            &[
                "use \"twig add <file>...\" to update what will be committed",
                "use \"twig restore <file>...\" to discard changes in working directory",
            ],
        )?;
        for (p, c) in &st.unstaged {
            writeln!(out, "\t{}", paint(color::RED, &format!("{:<12}{p}", c.label())))?;
        }
    }
    if !st.untracked.is_empty() {
        section(out, "Untracked files:", &["use \"twig add <file>...\" to include in what will be committed"])?;
        for p in &st.untracked {
            writeln!(out, "\t{}", paint(color::RED, p))?;
        }
    }
    writeln!(out)?;
    if st.staged.is_empty() && st.conflicts.is_empty() {
        if !st.unstaged.is_empty() {
            writeln!(out, "no changes added to commit (use \"twig add\" and/or \"twig commit -a\")")?;
        } else if !st.untracked.is_empty() {
            writeln!(out, "nothing added to commit but untracked files present (use \"twig add\" to track)")?;
        } else if repo.head()?.is_none() {
            writeln!(out, "nothing to commit (create/copy files and use \"twig add\" to track)")?;
        } else {
            writeln!(out, "nothing to commit, working tree clean")?;
        }
    }
    Ok(())
}

pub struct CommitOptions {
    pub messages: Vec<String>,
    pub file: Option<String>,
    pub all: bool,
    pub amend: bool,
    pub allow_empty: bool,
    pub quiet: bool,
}

pub fn commit(repo: &Repo, opts: CommitOptions, out: Out) -> Result<()> {
    let mut index = repo.load_index()?;
    if opts.all {
        let racy = worktree::index_mtime(repo);
        for e in index.entries().to_vec() {
            if e.stage != 0 {
                continue;
            }
            match worktree::worktree_change(repo, &e, racy)? {
                Some(Change::Deleted) => {
                    index.remove(&e.path);
                }
                Some(_) => stage_file(repo, &mut index, &e.path, racy)?,
                None => {}
            }
        }
        index.save(&repo.index_path())?;
    }
    if index.has_conflicts() {
        bail!(
            "Committing is not possible because you have unmerged files.\nhint: Fix them up in the work tree, and then use 'twig add <file>'\nhint: as appropriate to mark resolution and make a commit."
        );
    }
    let head = repo.head()?;
    let merge_head_path = repo.git_dir.join("MERGE_HEAD");
    let merge_head = match fs::read_to_string(&merge_head_path) {
        Ok(s) => Some(Oid::from_hex(s.lines().next().unwrap_or(""))?),
        Err(_) => None,
    };
    if opts.amend && merge_head.is_some() {
        bail!("You are in the middle of a merge -- cannot amend.");
    }
    let old = match (opts.amend, head) {
        (true, Some(h)) => Some(repo.odb.read_commit(&h)?),
        (true, None) => bail!("You have nothing to amend."),
        _ => None,
    };

    let raw_message = if let Some(f) = &opts.file {
        super::read_message_file(f)?
    } else if !opts.messages.is_empty() {
        opts.messages.join("\n\n")
    } else if let Some(old) = &old {
        old.message.clone()
    } else if merge_head.is_some() {
        fs::read_to_string(repo.git_dir.join("MERGE_MSG")).unwrap_or_default()
    } else {
        bail!("no commit message given (use -m <msg> or -F <file>)");
    };
    let message = clean_message(&raw_message);
    if message.is_empty() {
        bail!("Aborting commit due to empty commit message.");
    }

    let tree = repo.write_index_tree(&index)?;
    let parents: Vec<Oid> = match (&old, head, merge_head) {
        (Some(old), _, _) => old.parents.clone(),
        (None, Some(h), Some(m)) => vec![h, m],
        (None, Some(h), None) => vec![h],
        (None, None, _) => vec![],
    };
    let parent_tree = match parents.first() {
        Some(p) => Some(repo.commit_tree(p)?),
        None => None,
    };
    if !opts.allow_empty && merge_head.is_none() && !opts.amend {
        let empty_tree = Oid::hash_object(Kind::Tree, b"");
        if parent_tree == Some(tree) || (parent_tree.is_none() && tree == empty_tree) {
            let st = worktree::status(repo, &index)?;
            if st.unstaged.is_empty() && st.untracked.is_empty() {
                bail!("nothing to commit, working tree clean");
            }
            bail!("no changes added to commit (use \"twig add\" and/or \"twig commit -a\")");
        }
    }

    let author = match &old {
        Some(o) if std::env::var("GIT_AUTHOR_DATE").is_err() => {
            let mut a = repo.identity(Role::Author)?;
            a.time = o.author.time;
            a.tz = o.author.tz;
            a.name = o.author.name.clone();
            a.email = o.author.email.clone();
            a
        }
        _ => repo.identity(Role::Author)?,
    };
    let commit = Commit { tree, parents: parents.clone(), author, committer: repo.identity(Role::Committer)?, extra: Vec::new(), message };
    let oid = repo.odb.write(Kind::Commit, &commit.serialize())?;
    let kind = if opts.amend {
        " (amend)"
    } else if parents.is_empty() {
        " (initial)"
    } else if parents.len() > 1 {
        " (merge)"
    } else {
        ""
    };
    repo.refs.update("HEAD", &oid, &format!("commit{kind}: {}", commit.summary()), &repo.reflog_who())?;
    for f in ["MERGE_HEAD", "MERGE_MSG", "MERGE_MODE"] {
        let _ = fs::remove_file(repo.git_dir.join(f));
    }

    if !opts.quiet {
        let branch = repo.current_branch()?.unwrap_or_else(|| "detached HEAD".into());
        let root = if parents.is_empty() { " (root-commit)" } else { "" };
        writeln!(out, "[{branch}{root} {}] {}", oid.short(), commit.summary())?;
        let before = match parent_tree {
            Some(t) => snapshot_of_tree(&repo.flatten_tree(&t)?),
            None => Default::default(),
        };
        let after = snapshot_of_index(&index);
        let list = changes(&before, &after);
        let mut buf = Vec::new();
        super::print_stat(repo, &list, &mut buf)?;
        if let Some(summary) = String::from_utf8_lossy(&buf).lines().last() {
            writeln!(out, "{summary}")?;
        }
        for c in &list {
            match (&c.old, &c.new) {
                (None, Some(n)) => writeln!(out, " create mode {:06o} {}", n.mode, c.path)?,
                (Some(o), None) => writeln!(out, " delete mode {:06o} {}", o.mode, c.path)?,
                (Some(o), Some(n)) if o.mode != n.mode => writeln!(out, " mode change {:06o} => {:06o} {}", o.mode, n.mode, c.path)?,
                _ => {}
            }
        }
    }
    Ok(())
}
