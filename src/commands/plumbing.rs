//! Low-level commands: hash-object, cat-file, ls-tree, write-tree, commit-tree, rev-parse,
//! ls-files, merge-base and config.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use super::Out;
use crate::config;
use crate::object::{Commit, Kind, Tag, parse_tree};
use crate::oid::Oid;
use crate::repo::{Repo, Role};
use crate::revparse;

pub fn hash_object(repo: Option<&Repo>, kind: &str, write: bool, stdin: bool, files: &[PathBuf], out: Out) -> Result<()> {
    let kind = Kind::parse(kind)?;
    let mut inputs: Vec<Vec<u8>> = Vec::new();
    if stdin {
        let mut buf = Vec::new();
        std::io::stdin().read_to_end(&mut buf)?;
        inputs.push(buf);
    }
    for f in files {
        inputs.push(std::fs::read(f)?);
    }
    for data in inputs {
        let oid = if write {
            let Some(repo) = repo else { bail!("not a git repository: can't write objects") };
            repo.odb.write(kind, &data)?
        } else {
            Oid::hash_object(kind, &data)
        };
        writeln!(out, "{oid}")?;
    }
    Ok(())
}

pub enum CatMode {
    Type,
    Size,
    Pretty,
    Exists,
    Raw(Kind),
}

pub fn cat_file(repo: &Repo, mode: CatMode, rev: &str, out: Out) -> Result<()> {
    let oid = match revparse::resolve(repo, rev) {
        Ok(o) => o,
        Err(_) if matches!(mode, CatMode::Exists) => return Err(crate::Exit(1).into()),
        Err(e) => return Err(e),
    };
    let (kind, data) = repo.odb.read(&oid)?;
    match mode {
        CatMode::Exists => {}
        CatMode::Type => writeln!(out, "{kind}")?,
        CatMode::Size => writeln!(out, "{}", data.len())?,
        CatMode::Raw(want) => {
            let oid = revparse::peel(repo, oid, want)?;
            out.write_all(&repo.odb.read_kind(&oid, want)?)?;
        }
        CatMode::Pretty => match kind {
            Kind::Tree => {
                for e in parse_tree(&data)? {
                    let k = if e.is_tree() {
                        "tree"
                    } else if crate::object::mode::is_gitlink(e.mode) {
                        "commit"
                    } else {
                        "blob"
                    };
                    writeln!(out, "{:06o} {k} {}\t{}", e.mode, e.oid, e.name)?;
                }
            }
            _ => out.write_all(&data)?,
        },
    }
    Ok(())
}

pub fn ls_tree(repo: &Repo, treeish: &str, recursive: bool, show_trees: bool, name_only: bool, out: Out) -> Result<()> {
    let tree = revparse::peel(repo, revparse::resolve(repo, treeish)?, Kind::Tree)?;
    fn walk(repo: &Repo, tree: &Oid, prefix: &str, rec: bool, show_trees: bool, name_only: bool, out: Out) -> Result<()> {
        for e in repo.odb.read_tree(tree)? {
            let path = format!("{prefix}{}", e.name);
            let descend = rec && e.is_tree();
            if !descend || show_trees {
                if name_only {
                    writeln!(out, "{path}")?;
                } else {
                    let k = if e.is_tree() {
                        "tree"
                    } else if crate::object::mode::is_gitlink(e.mode) {
                        "commit"
                    } else {
                        "blob"
                    };
                    writeln!(out, "{:06o} {k} {}\t{path}", e.mode, e.oid)?;
                }
            }
            if descend {
                walk(repo, &e.oid, &format!("{path}/"), rec, show_trees, name_only, out)?;
            }
        }
        Ok(())
    }
    walk(repo, &tree, "", recursive, show_trees, name_only, out)
}

pub fn write_tree(repo: &Repo, out: Out) -> Result<()> {
    let oid = repo.write_index_tree(&repo.load_index()?)?;
    writeln!(out, "{oid}")?;
    Ok(())
}

pub fn commit_tree(repo: &Repo, tree: &str, parents: &[String], messages: &[String], out: Out) -> Result<()> {
    let tree = revparse::peel(repo, revparse::resolve(repo, tree)?, Kind::Tree)?;
    let parents = parents.iter().map(|p| revparse::commit(repo, p)).collect::<Result<Vec<_>>>()?;
    let message = if messages.is_empty() {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        messages.join("\n\n") + "\n"
    };
    let commit = Commit {
        tree,
        parents,
        author: repo.identity(Role::Author)?,
        committer: repo.identity(Role::Committer)?,
        extra: Vec::new(),
        message,
    };
    writeln!(out, "{}", repo.odb.write(Kind::Commit, &commit.serialize())?)?;
    Ok(())
}

pub fn rev_parse(repo: &Repo, revs: &[String], short: bool, abbrev_ref: bool, out: Out) -> Result<()> {
    for rev in revs {
        if abbrev_ref {
            let name = if rev == "HEAD" || rev == "@" {
                repo.current_branch()?.unwrap_or_else(|| "HEAD".into())
            } else {
                match revparse::expand_ref(repo, rev)? {
                    Some((full, _)) => full
                        .trim_start_matches("refs/heads/")
                        .trim_start_matches("refs/tags/")
                        .trim_start_matches("refs/remotes/")
                        .to_string(),
                    None => bail!("ambiguous argument '{rev}'"),
                }
            };
            writeln!(out, "{name}")?;
            continue;
        }
        let oid = revparse::resolve(repo, rev)?;
        writeln!(out, "{}", if short { oid.short() } else { oid.to_hex() })?;
    }
    Ok(())
}

pub fn ls_files(repo: &Repo, stage: bool, out: Out) -> Result<()> {
    let index = repo.load_index()?;
    let mut last = "";
    for e in index.entries() {
        if stage {
            writeln!(out, "{:06o} {} {}\t{}", e.mode, e.oid, e.stage, e.path)?;
        } else if e.path != last {
            writeln!(out, "{}", e.path)?;
        }
        last = &e.path;
    }
    Ok(())
}

pub fn merge_base(repo: &Repo, a: &str, b: &str, is_ancestor: bool, out: Out) -> Result<()> {
    let (a, b) = (revparse::commit(repo, a)?, revparse::commit(repo, b)?);
    if is_ancestor {
        return if crate::merge::is_ancestor(repo, a, b)? { Ok(()) } else { Err(crate::Exit(1).into()) };
    }
    match crate::merge::merge_bases(repo, a, b)?.first() {
        Some(base) => writeln!(out, "{base}")?,
        None => return Err(crate::Exit(1).into()),
    }
    Ok(())
}

pub fn config_cmd(repo: Option<&Repo>, global: bool, list: bool, key: Option<&str>, value: Option<&str>, out: Out) -> Result<()> {
    let file: PathBuf = if global {
        match std::env::var_os("GIT_CONFIG_GLOBAL") {
            Some(p) => PathBuf::from(p),
            None => Path::new(&std::env::var_os("HOME").unwrap_or_default()).join(".gitconfig"),
        }
    } else {
        match repo {
            Some(r) => r.git_dir.join("config"),
            None => bail!("not in a git repository (use --global)"),
        }
    };
    if list {
        let cfg = if global {
            config::Config::parse_str(&std::fs::read_to_string(&file).unwrap_or_default())
        } else {
            config::Config::load(repo.map(|r| r.git_dir.as_path()))
        };
        for (k, v) in cfg.entries() {
            writeln!(out, "{k}={v}")?;
        }
        return Ok(());
    }
    let Some(key) = key else { bail!("usage: twig config [--global] <key> [<value>]") };
    match value {
        Some(v) => config::set(&file, key, v),
        None => {
            let cfg = config::Config::load(repo.map(|r| r.git_dir.as_path()));
            match cfg.get(key) {
                Some(v) => {
                    writeln!(out, "{v}")?;
                    Ok(())
                }
                None => Err(crate::Exit(1).into()),
            }
        }
    }
}

/// `show` for tags and other non-commit objects.
pub fn describe_tag(tag: &Tag, out: Out) -> Result<()> {
    writeln!(out, "tag {}", tag.name)?;
    if let Some(t) = &tag.tagger {
        writeln!(out, "Tagger: {} <{}>", t.name, t.email)?;
        writeln!(out, "Date:   {}", crate::date::format_default(t))?;
    }
    writeln!(out)?;
    write!(out, "{}", tag.message)?;
    if !tag.message.is_empty() && !tag.message.ends_with('\n') {
        writeln!(out)?;
    }
    writeln!(out)?;
    Ok(())
}
