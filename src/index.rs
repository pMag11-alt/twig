//! The index (staging area) in Git's binary `.git/index` format, versions 2 and 3.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::object::mode;
use crate::oid::Oid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub ctime: (u32, u32),
    pub mtime: (u32, u32),
    pub dev: u32,
    pub ino: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u32,
    pub oid: Oid,
    /// 0 for normal entries; 1 (base), 2 (ours), 3 (theirs) during a conflicted merge.
    pub stage: u8,
    pub path: String,
}

impl Entry {
    pub fn new(path: &str, oid: Oid, mode: u32) -> Entry {
        Entry { ctime: (0, 0), mtime: (0, 0), dev: 0, ino: 0, mode, uid: 0, gid: 0, size: 0, oid, stage: 0, path: path.to_string() }
    }

    pub fn from_metadata(path: &str, oid: Oid, md: &fs::Metadata) -> Entry {
        let mut e = Entry::new(path, oid, file_mode(md));
        e.refresh_stat(md);
        e
    }

    pub fn refresh_stat(&mut self, md: &fs::Metadata) {
        self.ctime = (md.ctime() as u32, md.ctime_nsec() as u32);
        self.mtime = (md.mtime() as u32, md.mtime_nsec() as u32);
        self.dev = md.dev() as u32;
        self.ino = md.ino() as u32;
        self.uid = md.uid();
        self.gid = md.gid();
        self.size = md.len() as u32;
    }

    /// Cheap "is the file unchanged?" test from cached stat data, like Git's.
    pub fn stat_matches(&self, md: &fs::Metadata) -> bool {
        self.mtime == (md.mtime() as u32, md.mtime_nsec() as u32)
            && self.ctime == (md.ctime() as u32, md.ctime_nsec() as u32)
            && self.size == md.len() as u32
            && self.ino == md.ino() as u32
            && self.mode == file_mode(md)
    }
}

/// The mode Git records for a file on disk.
pub fn file_mode(md: &fs::Metadata) -> u32 {
    if md.file_type().is_symlink() {
        mode::SYMLINK
    } else if md.mode() & 0o111 != 0 {
        mode::EXEC
    } else {
        mode::FILE
    }
}

#[derive(Clone, Debug, Default)]
pub struct Index {
    /// Sorted by (path bytes, stage).
    entries: Vec<Entry>,
}

impl Index {
    pub fn load(path: &Path) -> Result<Index> {
        match fs::read(path) {
            Ok(data) => Index::parse(&data).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Index::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn parse(data: &[u8]) -> Result<Index> {
        if data.len() < 12 + 20 || &data[..4] != b"DIRC" {
            bail!("not an index file");
        }
        let body_end = data.len() - 20;
        if sha1_smol::Sha1::from(&data[..body_end]).digest().bytes() != data[body_end..] {
            bail!("index checksum mismatch");
        }
        let version = be32(data, 4);
        if version != 2 && version != 3 {
            bail!("index version {version} is not supported (only 2 and 3)");
        }
        let count = be32(data, 8) as usize;
        let mut entries = Vec::with_capacity(count);
        let mut pos = 12;
        for _ in 0..count {
            let start = pos;
            let f = |i: usize| be32(data, start + 4 * i);
            let oid = Oid::from_bytes(&data[start + 40..start + 60])?;
            let flags = u16::from_be_bytes([data[start + 60], data[start + 61]]);
            pos = start + 62;
            if flags & 0x4000 != 0 {
                if version < 3 {
                    bail!("extended index flags in a version 2 index");
                }
                pos += 2;
            }
            let nul = data[pos..body_end].iter().position(|&b| b == 0).context("unterminated path in index")?;
            let path = String::from_utf8_lossy(&data[pos..pos + nul]).into_owned();
            pos += nul;
            // Entries are NUL-padded to a multiple of 8 bytes (at least one NUL).
            let len = pos - start;
            pos = start + (len + 8) / 8 * 8;
            entries.push(Entry {
                ctime: (f(0), f(1)),
                mtime: (f(2), f(3)),
                dev: f(4),
                ino: f(5),
                mode: f(6),
                uid: f(7),
                gid: f(8),
                size: f(9),
                oid,
                stage: ((flags >> 12) & 3) as u8,
                path,
            });
        }
        // Remaining bytes before the checksum are extensions (TREE, REUC, ...), which we can
        // safely drop: Git rebuilds them as needed.
        let mut idx = Index { entries };
        idx.sort();
        Ok(idx)
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"DIRC");
        out.extend_from_slice(&2u32.to_be_bytes());
        out.extend_from_slice(&(self.entries.len() as u32).to_be_bytes());
        for e in &self.entries {
            let start = out.len();
            for v in [e.ctime.0, e.ctime.1, e.mtime.0, e.mtime.1, e.dev, e.ino, e.mode, e.uid, e.gid, e.size] {
                out.extend_from_slice(&v.to_be_bytes());
            }
            out.extend_from_slice(&e.oid.0);
            let flags = ((e.stage as u16 & 3) << 12) | (e.path.len().min(0xfff) as u16);
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(e.path.as_bytes());
            let len = out.len() - start;
            out.resize(start + (len + 8) / 8 * 8, 0);
        }
        let digest = sha1_smol::Sha1::from(&out).digest().bytes();
        out.extend_from_slice(&digest);
        out
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let lock = path.with_extension("lock");
        fs::write(&lock, self.serialize())?;
        fs::rename(&lock, path)?;
        Ok(())
    }

    fn sort(&mut self) {
        self.entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()).then(a.stage.cmp(&b.stage)));
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Stage-0 entry for `path`.
    pub fn get(&self, path: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.path == path && e.stage == 0)
    }

    pub fn get_mut(&mut self, path: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.path == path && e.stage == 0)
    }

    /// Add or replace the entry for `entry.path`. Adding a stage-0 entry resolves any conflict,
    /// and replaces a file with a directory (or vice versa) if paths collide.
    pub fn add(&mut self, entry: Entry) {
        let path = entry.path.clone();
        let dir_prefix = format!("{path}/");
        self.entries.retain(|e| {
            if entry.stage == 0 && e.path == path {
                return false;
            }
            if e.path == path && e.stage == entry.stage {
                return false;
            }
            // a file "a" replaces everything under "a/", and "a/b" replaces a file "a"
            !(e.path.starts_with(&dir_prefix) || path.starts_with(&format!("{}/", e.path)))
        });
        self.entries.push(entry);
        self.sort();
    }

    /// Remove `path` at every stage. Returns whether anything was removed.
    pub fn remove(&mut self, path: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.path != path);
        before != self.entries.len()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(|e| e.stage != 0)
    }

    /// Paths with conflict stages, each listed once.
    pub fn conflicted_paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.entries.iter().filter(|e| e.stage != 0).map(|e| e.path.clone()).collect();
        v.dedup();
        v
    }

    /// Is `dir` (a path without trailing slash) a prefix of any tracked path?
    pub fn tracks_under(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        let i = self.entries.partition_point(|e| e.path.as_bytes() < prefix.as_bytes());
        self.entries.get(i).is_some_and(|e| e.path.starts_with(&prefix))
    }
}

fn be32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(s: &str) -> Oid {
        Oid::hash_object(crate::object::Kind::Blob, s.as_bytes())
    }

    #[test]
    fn roundtrip_and_ordering() {
        let mut idx = Index::default();
        idx.add(Entry::new("b.txt", oid("b"), mode::FILE));
        idx.add(Entry::new("a/b", oid("ab"), mode::EXEC));
        idx.add(Entry::new("a.txt", oid("a"), mode::FILE));
        idx.add(Entry::new("a-very-long-name-that-needs-padding.txt", oid("x"), mode::FILE));
        let bytes = idx.serialize();
        let back = Index::parse(&bytes).unwrap();
        let paths: Vec<_> = back.entries().iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a-very-long-name-that-needs-padding.txt", "a.txt", "a/b", "b.txt"]);
        assert_eq!(back.get("a/b").unwrap().mode, mode::EXEC);
        assert!(back.tracks_under("a"));
        assert!(!back.tracks_under("a.txt"));
    }

    #[test]
    fn stages_and_conflict_resolution() {
        let mut idx = Index::default();
        for stage in 1..=3 {
            let mut e = Entry::new("f", oid(&stage.to_string()), mode::FILE);
            e.stage = stage;
            idx.add(e);
        }
        assert!(idx.has_conflicts());
        assert_eq!(idx.conflicted_paths(), ["f"]);
        let back = Index::parse(&idx.serialize()).unwrap();
        assert_eq!(back.entries().iter().map(|e| e.stage).collect::<Vec<_>>(), [1, 2, 3]);
        idx.add(Entry::new("f", oid("resolved"), mode::FILE));
        assert!(!idx.has_conflicts());
        assert_eq!(idx.entries().len(), 1);
    }

    #[test]
    fn file_directory_replacement() {
        let mut idx = Index::default();
        idx.add(Entry::new("x/one", oid("1"), mode::FILE));
        idx.add(Entry::new("x/two", oid("2"), mode::FILE));
        idx.add(Entry::new("x", oid("file"), mode::FILE));
        assert_eq!(idx.entries().len(), 1);
        idx.add(Entry::new("x/three", oid("3"), mode::FILE));
        assert_eq!(idx.entries().iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["x/three"]);
    }

    #[test]
    fn rejects_corruption() {
        let mut bytes = Index::default().serialize();
        let n = bytes.len();
        bytes[n - 1] ^= 1;
        assert!(Index::parse(&bytes).is_err());
    }
}
