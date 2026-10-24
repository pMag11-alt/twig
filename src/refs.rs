//! References: HEAD, branches, tags and remote-tracking refs, loose or packed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::oid::Oid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RefValue {
    Direct(Oid),
    Symbolic(String),
}

pub struct Refs {
    git_dir: PathBuf,
}

impl Refs {
    pub fn new(git_dir: &Path) -> Refs {
        Refs { git_dir: git_dir.to_path_buf() }
    }

    fn packed(&self) -> Result<BTreeMap<String, Oid>> {
        let mut map = BTreeMap::new();
        let Ok(text) = fs::read_to_string(self.git_dir.join("packed-refs")) else { return Ok(map) };
        for line in text.lines() {
            if line.starts_with('#') || line.starts_with('^') || line.is_empty() {
                continue;
            }
            if let Some((hex, name)) = line.split_once(' ') {
                map.insert(name.to_string(), Oid::from_hex(hex)?);
            }
        }
        Ok(map)
    }

    pub fn read(&self, name: &str) -> Result<Option<RefValue>> {
        let path = self.git_dir.join(name);
        if path.is_file() {
            let text = fs::read_to_string(&path)?;
            let text = text.trim();
            if let Some(target) = text.strip_prefix("ref:") {
                return Ok(Some(RefValue::Symbolic(target.trim().to_string())));
            }
            return Ok(Some(RefValue::Direct(Oid::from_hex(text).with_context(|| format!("bad ref {name}"))?)));
        }
        Ok(self.packed()?.get(name).copied().map(RefValue::Direct))
    }

    /// Follow symbolic refs to an object id. Ok(None) for an unborn branch.
    pub fn resolve(&self, name: &str) -> Result<Option<Oid>> {
        let mut name = name.to_string();
        for _ in 0..10 {
            match self.read(&name)? {
                None => return Ok(None),
                Some(RefValue::Direct(oid)) => return Ok(Some(oid)),
                Some(RefValue::Symbolic(target)) => name = target,
            }
        }
        bail!("symbolic ref loop at {name}")
    }

    pub fn exists(&self, name: &str) -> Result<bool> {
        Ok(self.read(name)?.is_some())
    }

    /// The ref name HEAD points at, or None when HEAD is detached.
    pub fn head_target(&self) -> Result<Option<String>> {
        Ok(match self.read("HEAD")? {
            Some(RefValue::Symbolic(t)) => Some(t),
            _ => None,
        })
    }

    /// Point `name` at `oid`. If `name` is a symbolic ref (like HEAD on a branch) its target moves.
    pub fn update(&self, name: &str, oid: &Oid, log_message: &str, who: &str) -> Result<()> {
        let mut target = name.to_string();
        for _ in 0..10 {
            match self.read(&target)? {
                Some(RefValue::Symbolic(t)) => target = t,
                _ => break,
            }
        }
        let old = self.resolve(&target)?.unwrap_or(Oid::ZERO);
        self.write_file(&target, &format!("{oid}\n"))?;
        self.append_log(&target, &old, oid, log_message, who)?;
        if target != name {
            self.append_log(name, &old, oid, log_message, who)?;
        }
        Ok(())
    }

    pub fn set_symbolic(&self, name: &str, target: &str) -> Result<()> {
        self.write_file(name, &format!("ref: {target}\n"))
    }

    pub fn set_detached(&self, name: &str, oid: &Oid) -> Result<()> {
        self.write_file(name, &format!("{oid}\n"))
    }

    fn write_file(&self, name: &str, contents: &str) -> Result<()> {
        let path = self.git_dir.join(name);
        fs::create_dir_all(path.parent().unwrap())?;
        let lock = path.with_file_name(format!("{}.lock", path.file_name().unwrap().to_string_lossy()));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .with_context(|| format!("cannot lock ref {name}: {} exists", lock.display()))?;
        f.write_all(contents.as_bytes())?;
        drop(f);
        fs::rename(&lock, &path)?;
        Ok(())
    }

    fn append_log(&self, name: &str, old: &Oid, new: &Oid, message: &str, who: &str) -> Result<()> {
        if who.is_empty() {
            return Ok(());
        }
        let path = self.git_dir.join("logs").join(name);
        fs::create_dir_all(path.parent().unwrap())?;
        let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{old} {new} {who}\t{message}")?;
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let path = self.git_dir.join(name);
        if path.is_file() {
            fs::remove_file(&path)?;
        }
        let _ = fs::remove_file(self.git_dir.join("logs").join(name));
        let packed_path = self.git_dir.join("packed-refs");
        if let Ok(text) = fs::read_to_string(&packed_path) {
            let mut out = String::new();
            let mut skip_peel = false;
            for line in text.lines() {
                if line.starts_with('^') {
                    if !skip_peel {
                        out.push_str(line);
                        out.push('\n');
                    }
                    continue;
                }
                skip_peel = line.split_once(' ').is_some_and(|(_, n)| n == name);
                if !skip_peel {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            if out != text {
                fs::write(&packed_path, out)?;
            }
        }
        Ok(())
    }

    /// All refs under `prefix` (e.g. "refs/heads/"), loose ones overriding packed ones.
    pub fn list(&self, prefix: &str) -> Result<BTreeMap<String, Oid>> {
        let mut map: BTreeMap<String, Oid> = self.packed()?.into_iter().filter(|(k, _)| k.starts_with(prefix)).collect();
        let root = self.git_dir.join(prefix);
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = fs::read_dir(&dir) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if !p.to_string_lossy().ends_with(".lock") {
                    let rel = p.strip_prefix(&self.git_dir).unwrap().to_string_lossy().replace('\\', "/");
                    if let Some(oid) = self.resolve(&rel)? {
                        map.insert(rel, oid);
                    }
                }
            }
        }
        Ok(map)
    }
}

/// Is this an acceptable branch or tag name? (A practical subset of `git check-ref-format`.)
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with(".lock")
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name != "@"
        && !name.split('/').any(|part| part.starts_with('.'))
        && !name.chars().any(|c| c.is_control() || " ~^:?*[\\".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_names() {
        for ok in ["main", "feature/login", "v1.0", "fix-#12", "a_b"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "-x", "a..b", "a b", "x.lock", "a/", "a//b", ".hidden", "a/.b", "a~1", "a^", "a:b", "@", "x@{1}"] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn loose_packed_symbolic_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let refs = Refs::new(dir.path());
        let a = Oid::hash_object(crate::object::Kind::Blob, b"a");
        let b = Oid::hash_object(crate::object::Kind::Blob, b"b");
        fs::write(
            dir.path().join("packed-refs"),
            format!("# pack-refs with: peeled fully-peeled sorted \n{a} refs/heads/old\n{a} refs/tags/v1\n^{b}\n"),
        )
        .unwrap();
        refs.set_symbolic("HEAD", "refs/heads/main").unwrap();
        assert_eq!(refs.resolve("HEAD").unwrap(), None); // unborn
        refs.update("HEAD", &b, "commit", "T <t@x> 0 +0000").unwrap();
        assert_eq!(refs.resolve("refs/heads/main").unwrap(), Some(b));
        assert_eq!(refs.head_target().unwrap().as_deref(), Some("refs/heads/main"));
        assert_eq!(refs.resolve("refs/tags/v1").unwrap(), Some(a));
        let heads = refs.list("refs/heads/").unwrap();
        assert_eq!(heads.keys().collect::<Vec<_>>(), ["refs/heads/main", "refs/heads/old"]);
        refs.delete("refs/tags/v1").unwrap();
        assert_eq!(refs.resolve("refs/tags/v1").unwrap(), None);
        let packed = fs::read_to_string(dir.path().join("packed-refs")).unwrap();
        assert!(!packed.contains('^'), "peeled line of the deleted tag must go too");
        assert!(packed.contains("refs/heads/old"));
        let log = fs::read_to_string(dir.path().join("logs/HEAD")).unwrap();
        assert!(log.starts_with(&format!("{} {b} T <t@x>", Oid::ZERO)));
    }
}
