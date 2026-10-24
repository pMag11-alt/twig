//! clone, fetch and pull.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::Out;
use crate::config;
use crate::index::{Entry, Index};
use crate::oid::Oid;
use crate::remote::Remote;
use crate::repo::Repo;
use crate::worktree;

fn default_dir(url: &str) -> PathBuf {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let name = trimmed.rsplit(['/', ':']).next().filter(|s| !s.is_empty()).unwrap_or("repo");
    PathBuf::from(name)
}

pub fn clone(url: &str, dir: Option<&Path>, quiet: bool, out: Out) -> Result<()> {
    let dest = dir.map(Path::to_path_buf).unwrap_or_else(|| default_dir(url));
    if dest.exists() && fs::read_dir(&dest)?.next().is_some() {
        bail!("destination path '{}' already exists and is not an empty directory.", dest.display());
    }
    let remote = Remote::open(url)?;
    // a local path is recorded as an absolute URL, like Git does
    let url = match &remote {
        Remote::Local(r) => r.work_tree.display().to_string(),
        Remote::Http(u) => u.clone(),
    };
    if !quiet {
        eprintln!("Cloning into '{}'...", dest.display());
    }
    let adv = remote.advertise()?;
    let created = !dest.exists();
    let result = (|| -> Result<()> {
        let branch = adv.head_target.as_deref().and_then(|t| t.strip_prefix("refs/heads/")).map(String::from);
        let (repo, _) = Repo::init(&dest, Some(branch.as_deref().unwrap_or("main")))?;
        let cfg = repo.git_dir.join("config");
        config::set(&cfg, "remote.origin.url", &url)?;
        config::set(&cfg, "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")?;
        if adv.refs.is_empty() {
            eprintln!("warning: You appear to have cloned an empty repository.");
            return Ok(());
        }
        let wants: BTreeSet<Oid> = adv.refs.values().copied().chain(adv.head).collect();
        let wants: Vec<Oid> = wants.into_iter().collect();
        let n = remote.fetch_objects(&repo, &wants, &[], !quiet)?;
        let repo = Repo::open(&repo.work_tree, &repo.git_dir)?; // pick up the new pack
        let who = repo.reflog_who();
        for (name, oid) in &adv.refs {
            if let Some(b) = name.strip_prefix("refs/heads/") {
                repo.refs.update(&format!("refs/remotes/origin/{b}"), oid, &format!("clone: from {url}"), &who)?;
            } else if name.starts_with("refs/tags/") {
                repo.refs.update(name, oid, "", "")?;
            }
        }
        match (&branch, adv.head) {
            (Some(b), Some(head)) => {
                repo.refs.set_symbolic("refs/remotes/origin/HEAD", &format!("refs/remotes/origin/{b}"))?;
                repo.refs.update(&format!("refs/heads/{b}"), &head, &format!("clone: from {url}"), &who)?;
                repo.refs.set_symbolic("HEAD", &format!("refs/heads/{b}"))?;
                config::set(&cfg, &format!("branch.{b}.remote"), "origin")?;
                config::set(&cfg, &format!("branch.{b}.merge"), &format!("refs/heads/{b}"))?;
                checkout_fresh(&repo, head)?;
            }
            (None, Some(head)) => {
                repo.refs.set_detached("HEAD", &head)?;
                checkout_fresh(&repo, head)?;
            }
            _ => {}
        }
        if !quiet {
            writeln!(out, "Received {n} objects; checked out '{}'.", branch.as_deref().unwrap_or("HEAD"))?;
        }
        Ok(())
    })();
    if result.is_err() && created {
        let _ = fs::remove_dir_all(&dest);
    }
    result
}

/// Populate an empty work tree and index from `commit`.
fn checkout_fresh(repo: &Repo, commit: Oid) -> Result<()> {
    let mut index = Index::default();
    for (path, (m, oid)) in repo.flatten_commit(Some(commit))? {
        let md = worktree::write_file(repo, &path, m, &oid)?;
        let mut e = Entry::from_metadata(&path, oid, &md);
        e.mode = m;
        index.add(e);
    }
    index.save(&repo.index_path())
}

/// Download new objects and update `refs/remotes/<remote>/*`. Returns the reopened repository.
pub fn fetch(repo: Repo, remote_name: &str, quiet: bool, out: Out) -> Result<Repo> {
    let url = repo
        .config
        .get(&format!("remote.{remote_name}.url"))
        .with_context(|| format!("'{remote_name}' does not appear to be a git repository"))?
        .to_string();
    let remote = Remote::open(&url)?;
    let adv = remote.advertise()?;
    let wants: BTreeSet<Oid> = adv.refs.values().copied().filter(|o| !repo.odb.contains(o)).collect();
    if !wants.is_empty() {
        let mut haves: BTreeSet<Oid> = repo.refs.list("refs/heads/")?.into_values().collect();
        haves.extend(repo.refs.list(&format!("refs/remotes/{remote_name}/"))?.into_values());
        let wants: Vec<Oid> = wants.into_iter().collect();
        let haves: Vec<Oid> = haves.into_iter().collect();
        remote.fetch_objects(&repo, &wants, &haves, !quiet)?;
    }
    let repo = Repo::open(&repo.work_tree, &repo.git_dir)?;
    let who = repo.reflog_who();
    let mut lines = Vec::new();
    for (name, oid) in &adv.refs {
        let (local, short, is_tag) = if let Some(b) = name.strip_prefix("refs/heads/") {
            (format!("refs/remotes/{remote_name}/{b}"), b.to_string(), false)
        } else if let Some(t) = name.strip_prefix("refs/tags/") {
            (name.clone(), t.to_string(), true)
        } else {
            continue;
        };
        let target = if is_tag { short.clone() } else { format!("{remote_name}/{short}") };
        match repo.refs.resolve(&local)? {
            Some(old) if old == *oid => {}
            Some(_) if is_tag => {} // never move existing tags
            Some(old) => {
                let forced = !crate::merge::is_ancestor(&repo, old, *oid)?;
                repo.refs.update(&local, oid, &format!("fetch: {}", if forced { "forced-update" } else { "fast-forward" }), &who)?;
                let sep = if forced { "..." } else { ".." };
                lines.push(format!(
                    " {} {:<17} {short:<10} -> {target}",
                    if forced { "+" } else { " " },
                    format!("{}{sep}{}", old.short(), oid.short())
                ));
            }
            None => {
                repo.refs.update(&local, oid, "fetch: storing head", &who)?;
                let what = if is_tag { "[new tag]" } else { "[new branch]" };
                lines.push(format!(" * {what:<17} {short:<10} -> {target}"));
            }
        }
    }
    // branches deleted on the remote are left alone, like `git fetch` without --prune
    if !lines.is_empty() && !quiet {
        writeln!(out, "From {url}")?;
        for l in lines {
            writeln!(out, "{l}")?;
        }
    }
    Ok(repo)
}

pub fn pull(repo: Repo, remote_name: Option<&str>, quiet: bool, out: Out) -> Result<()> {
    let Some(branch) = repo.current_branch()? else { bail!("You are not currently on a branch.") };
    let remote_name = remote_name
        .map(String::from)
        .or_else(|| repo.config.get(&format!("branch.{branch}.remote")).map(String::from))
        .unwrap_or_else(|| "origin".into());
    let merge_ref = repo.config.get(&format!("branch.{branch}.merge")).map(String::from).unwrap_or_else(|| format!("refs/heads/{branch}"));
    let repo = fetch(repo, &remote_name, quiet, out)?;
    let upstream = format!("{remote_name}/{}", merge_ref.trim_start_matches("refs/heads/"));
    if repo.refs.resolve(&format!("refs/remotes/{upstream}"))?.is_none() {
        bail!("no such ref was fetched: {merge_ref}");
    }
    super::branching::merge_cmd(&repo, &upstream, None, false, out)
}
