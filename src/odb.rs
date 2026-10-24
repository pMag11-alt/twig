//! The object database: loose objects in `objects/xx/...` plus packfiles.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use crate::object::{Commit, Kind, Tag, TreeEntry, parse_tree};
use crate::oid::Oid;
use crate::pack::Pack;

pub struct Odb {
    dir: PathBuf,
    packs: Vec<Pack>,
}

impl Odb {
    pub fn open(objects_dir: &Path) -> Result<Odb> {
        let mut packs = Vec::new();
        let pack_dir = objects_dir.join("pack");
        if let Ok(rd) = fs::read_dir(&pack_dir) {
            let mut idx_paths: Vec<_> =
                rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "idx")).collect();
            idx_paths.sort();
            for p in idx_paths {
                packs.push(Pack::open(&p)?);
            }
        }
        Ok(Odb { dir: objects_dir.to_path_buf(), packs })
    }

    fn loose_path(&self, oid: &Oid) -> PathBuf {
        let hex = oid.to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    pub fn contains(&self, oid: &Oid) -> bool {
        self.loose_path(oid).exists() || self.packs.iter().any(|p| p.contains(oid))
    }

    pub fn read(&self, oid: &Oid) -> Result<(Kind, Vec<u8>)> {
        let path = self.loose_path(oid);
        if path.exists() {
            return read_loose(&path).with_context(|| format!("reading object {oid}"));
        }
        let external = |base: &Oid| self.read(base);
        for pack in &self.packs {
            if let Some(obj) = pack.read(oid, &external)? {
                return Ok(obj);
            }
        }
        bail!("object {oid} not found")
    }

    pub fn read_kind(&self, oid: &Oid, want: Kind) -> Result<Vec<u8>> {
        let (kind, data) = self.read(oid)?;
        if kind != want {
            bail!("object {} is a {kind}, not a {want}", oid.short());
        }
        Ok(data)
    }

    pub fn read_commit(&self, oid: &Oid) -> Result<Commit> {
        Commit::parse(&self.read_kind(oid, Kind::Commit)?).with_context(|| format!("parsing commit {oid}"))
    }

    pub fn read_tree(&self, oid: &Oid) -> Result<Vec<TreeEntry>> {
        parse_tree(&self.read_kind(oid, Kind::Tree)?).with_context(|| format!("parsing tree {oid}"))
    }

    pub fn read_tag(&self, oid: &Oid) -> Result<Tag> {
        Tag::parse(&self.read_kind(oid, Kind::Tag)?)
    }

    /// Store an object as a loose file (no-op if it already exists anywhere).
    pub fn write(&self, kind: Kind, data: &[u8]) -> Result<Oid> {
        let oid = Oid::hash_object(kind, data);
        if self.contains(&oid) {
            return Ok(oid);
        }
        let path = self.loose_path(&oid);
        fs::create_dir_all(path.parent().unwrap())?;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(format!("{kind} {}\0", data.len()).as_bytes())?;
        enc.write_all(data)?;
        let compressed = enc.finish()?;
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, compressed)?;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_readonly(true);
        fs::set_permissions(&tmp, perms)?;
        fs::rename(&tmp, &path)?;
        Ok(oid)
    }

    /// All object ids that start with `prefix` (lowercase hex, at least 4 chars).
    pub fn find_prefix(&self, prefix: &str) -> Result<Vec<Oid>> {
        let prefix = prefix.to_ascii_lowercase();
        let mut found = Vec::new();
        if let Ok(rd) = fs::read_dir(self.dir.join(&prefix[..2])) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let hex = format!("{}{name}", &prefix[..2]);
                if hex.len() == 40 && hex.starts_with(&prefix) {
                    found.push(Oid::from_hex(&hex)?);
                }
            }
        }
        for pack in &self.packs {
            found.extend(pack.index.with_prefix(&prefix));
        }
        found.sort();
        found.dedup();
        Ok(found)
    }

    pub fn reload(&mut self) -> Result<()> {
        *self = Odb::open(&self.dir)?;
        Ok(())
    }
}

fn read_loose(path: &Path) -> Result<(Kind, Vec<u8>)> {
    let mut raw = Vec::new();
    ZlibDecoder::new(fs::File::open(path)?).read_to_end(&mut raw)?;
    let nul = raw.iter().position(|&b| b == 0).ok_or_else(|| anyhow!("corrupt object header"))?;
    let header = std::str::from_utf8(&raw[..nul])?;
    let (kind, size) = header.split_once(' ').ok_or_else(|| anyhow!("corrupt object header"))?;
    let size: usize = size.parse()?;
    let data = raw[nul + 1..].to_vec();
    if data.len() != size {
        bail!("object size mismatch: header says {size}, found {}", data.len());
    }
    Ok((Kind::parse(kind)?, data))
}
