//! A repository: the `.git` directory, its object database and refs, and the work tree.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::Config;
use crate::date;
use crate::index::Index;
use crate::object::{Kind, Signature, TreeEntry, mode, serialize_tree};
use crate::odb::Odb;
use crate::oid::Oid;
use crate::refs::Refs;

/// path -> (mode, blob id) for every file in a tree, recursively.
pub type FlatTree = BTreeMap<String, (u32, Oid)>;

pub struct Repo {
    pub git_dir: PathBuf,
    pub work_tree: PathBuf,
    pub odb: Odb,
    pub refs: Refs,
    pub config: Config,
}

#[derive(Clone, Copy)]
pub enum Role {
    Author,
    Committer,
}

impl Repo {
    /// Find the repository containing `start` by walking up to the nearest `.git`.
    pub fn discover(start: &Path) -> Result<Repo> {
        let start = start.canonicalize().with_context(|| format!("cannot access {}", start.display()))?;
        let mut dir = start.as_path();
        loop {
            let dotgit = dir.join(".git");
            if dotgit.is_dir() {
                return Repo::open(dir, &dotgit);
            }
            if dotgit.is_file() {
                // worktrees and submodules use a ".git" file: "gitdir: <path>"
                let text = fs::read_to_string(&dotgit)?;
                let target = text.trim().strip_prefix("gitdir:").ok_or_else(|| anyhow!("bad .git file"))?.trim();
                return Repo::open(dir, &dir.join(target));
            }
            dir = dir.parent().ok_or_else(|| anyhow!("not a git repository (or any of the parent directories): .git"))?;
        }
    }

    pub fn open(work_tree: &Path, git_dir: &Path) -> Result<Repo> {
        let git_dir = git_dir.canonicalize()?;
        if !git_dir.join("HEAD").is_file() || !git_dir.join("objects").is_dir() {
            bail!("{} is not a git directory", git_dir.display());
        }
        Ok(Repo {
            odb: Odb::open(&git_dir.join("objects"))?,
            refs: Refs::new(&git_dir),
            config: Config::load(Some(&git_dir)),
            work_tree: work_tree.canonicalize()?,
            git_dir,
        })
    }

    /// Create an empty repository (like `git init`). Re-running it on an existing one is harmless.
    pub fn init(path: &Path, initial_branch: Option<&str>) -> Result<(Repo, bool)> {
        fs::create_dir_all(path)?;
        let git_dir = path.join(".git");
        let existed = git_dir.join("HEAD").exists();
        for d in ["objects/info", "objects/pack", "refs/heads", "refs/tags", "info", "hooks"] {
            fs::create_dir_all(git_dir.join(d))?;
        }
        if !existed {
            let branch = match initial_branch {
                Some(b) => b.to_string(),
                None => Config::load(None).get("init.defaultBranch").unwrap_or("main").to_string(),
            };
            if !crate::refs::valid_name(&branch) {
                bail!("invalid branch name: {branch}");
            }
            fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
            fs::write(
                git_dir.join("config"),
                "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n",
            )?;
            fs::write(git_dir.join("description"), "Unnamed repository; edit this file 'description' to name the repository.\n")?;
            fs::write(git_dir.join("info/exclude"), "# git ls-files --others --exclude-from=.git/info/exclude\n")?;
        }
        Ok((Repo::open(path, &git_dir)?, existed))
    }

    pub fn index_path(&self) -> PathBuf {
        self.git_dir.join("index")
    }

    pub fn load_index(&self) -> Result<Index> {
        Index::load(&self.index_path())
    }

    pub fn head(&self) -> Result<Option<Oid>> {
        self.refs.resolve("HEAD")
    }

    /// Short branch name HEAD is on ("main"), or None if detached.
    pub fn current_branch(&self) -> Result<Option<String>> {
        Ok(self.refs.head_target()?.map(|t| t.strip_prefix("refs/heads/").unwrap_or(&t).to_string()))
    }

    pub fn file_mode_enabled(&self) -> bool {
        self.config.get_bool("core.filemode").unwrap_or(true)
    }

    /// Author or committer identity from GIT_* variables, then user.name / user.email.
    pub fn identity(&self, role: Role) -> Result<Signature> {
        let prefix = match role {
            Role::Author => "GIT_AUTHOR",
            Role::Committer => "GIT_COMMITTER",
        };
        let env = |k: &str| std::env::var(format!("{prefix}_{k}")).ok().filter(|v| !v.is_empty());
        let name = env("NAME").or_else(|| self.config.get("user.name").map(String::from));
        let email = env("EMAIL").or_else(|| self.config.get("user.email").map(String::from));
        let (Some(name), Some(email)) = (name, email) else {
            bail!(
                "Author identity unknown\n\n*** Please tell me who you are.\n\nRun\n\n  \
                 twig config user.email \"you@example.com\"\n  twig config user.name \"Your Name\"\n"
            );
        };
        let (time, tz) = match env("DATE") {
            Some(d) => date::parse_env_date(&d)?,
            None => {
                let t = date::now();
                (t, date::local_offset(t))
            }
        };
        Ok(Signature { name, email, time, tz })
    }

    /// Who to record in reflogs; empty (no reflog entry) if no identity is configured.
    pub fn reflog_who(&self) -> String {
        self.identity(Role::Committer).map(|s| s.to_string()).unwrap_or_default()
    }

    pub fn commit_tree(&self, commit: &Oid) -> Result<Oid> {
        Ok(self.odb.read_commit(commit)?.tree)
    }

    /// Every file in `tree`, keyed by its full path.
    pub fn flatten_tree(&self, tree: &Oid) -> Result<FlatTree> {
        let mut out = FlatTree::new();
        self.flatten_into(tree, "", &mut out)?;
        Ok(out)
    }

    fn flatten_into(&self, tree: &Oid, prefix: &str, out: &mut FlatTree) -> Result<()> {
        for e in self.odb.read_tree(tree)? {
            let path = if prefix.is_empty() { e.name.clone() } else { format!("{prefix}/{}", e.name) };
            if e.is_tree() {
                self.flatten_into(&e.oid, &path, out)?;
            } else {
                out.insert(path, (e.mode, e.oid));
            }
        }
        Ok(())
    }

    /// Flattened tree of a commit, or empty for None (an unborn branch).
    pub fn flatten_commit(&self, commit: Option<Oid>) -> Result<FlatTree> {
        match commit {
            Some(c) => self.flatten_tree(&self.commit_tree(&c)?),
            None => Ok(FlatTree::new()),
        }
    }

    /// Write nested tree objects for a flat path -> (mode, oid) map; returns the root tree id.
    pub fn write_tree(&self, files: &FlatTree) -> Result<Oid> {
        let items: Vec<(&str, u32, Oid)> = files.iter().map(|(p, (m, o))| (p.as_str(), *m, *o)).collect();
        self.write_subtree(&items)
    }

    fn write_subtree(&self, items: &[(&str, u32, Oid)]) -> Result<Oid> {
        let mut entries = Vec::new();
        let mut i = 0;
        while i < items.len() {
            let (path, m, oid) = items[i];
            match path.split_once('/') {
                None => {
                    entries.push(TreeEntry { mode: m, name: path.to_string(), oid });
                    i += 1;
                }
                Some((dir, _)) => {
                    let prefix = format!("{dir}/");
                    let mut children = Vec::new();
                    while i < items.len() && items[i].0.starts_with(&prefix) {
                        children.push((&items[i].0[prefix.len()..], items[i].1, items[i].2));
                        i += 1;
                    }
                    let sub = self.write_subtree(&children)?;
                    entries.push(TreeEntry { mode: mode::TREE, name: dir.to_string(), oid: sub });
                }
            }
        }
        self.odb.write(Kind::Tree, &serialize_tree(&mut entries))
    }

    /// Tree for the stage-0 entries of the index (fails while there are conflicts).
    pub fn write_index_tree(&self, index: &Index) -> Result<Oid> {
        if index.has_conflicts() {
            bail!("you need to resolve your current index first");
        }
        let files: FlatTree = index.entries().iter().map(|e| (e.path.clone(), (e.mode, e.oid))).collect();
        self.write_tree(&files)
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        self.work_tree.join(rel)
    }

    /// Turn a path given on the command line (relative to the cwd) into a repo-relative
    /// '/'-separated path. Errors if it's outside the work tree.
    pub fn rel_path(&self, p: &Path) -> Result<String> {
        let cwd = std::env::current_dir()?;
        let joined = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
        // normalise ".." and "." lexically (the path might not exist any more)
        let mut parts: Vec<std::ffi::OsString> = Vec::new();
        for c in joined.components() {
            match c {
                Component::ParentDir => {
                    parts.pop();
                }
                Component::CurDir => {}
                Component::Normal(s) => parts.push(s.to_os_string()),
                Component::RootDir | Component::Prefix(_) => parts.clear(),
            }
        }
        let normal: PathBuf = std::iter::once(std::ffi::OsString::from("/")).chain(parts).collect();
        let base = self.work_tree.as_path();
        // the cwd may be reached through a symlink (/tmp vs /private/tmp on macOS)
        let canon = normal
            .ancestors()
            .find_map(|a| a.canonicalize().ok().map(|c| c.join(normal.strip_prefix(a).unwrap())))
            .unwrap_or(normal.clone());
        let rel = canon
            .strip_prefix(base)
            .or_else(|_| normal.strip_prefix(base))
            .map_err(|_| anyhow!("{}: '{}' is outside repository", p.display(), base.display()))?;
        Ok(rel.to_string_lossy().replace('\\', "/"))
    }
}
