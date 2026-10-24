//! Packfiles (`objects/pack/*.pack`) and their v2 indexes (`*.idx`).
//!
//! A pack is "PACK", a version, an object count, then entries back to back:
//! a varint type+size header followed by a zlib stream. Delta entries refer to
//! their base either by offset within the pack (OFS_DELTA) or by id (REF_DELTA).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use anyhow::{Context, Result, anyhow, bail};
use flate2::{Decompress, FlushDecompress, Status};

use crate::delta;
use crate::object::Kind;
use crate::oid::Oid;

const OFS_DELTA: u8 = 6;
const REF_DELTA: u8 = 7;

// ---------------------------------------------------------------------------
// Index (.idx v2)
// ---------------------------------------------------------------------------

pub struct PackIndex {
    fanout: [u32; 256],
    oids: Vec<Oid>,
    offsets: Vec<u64>,
}

impl PackIndex {
    pub fn parse(data: &[u8]) -> Result<PackIndex> {
        if data.len() < 8 + 256 * 4 || &data[..4] != b"\xfftOc" || be32(data, 4) != 2 {
            bail!("unsupported pack index (only version 2 is supported)");
        }
        let mut fanout = [0u32; 256];
        for (i, f) in fanout.iter_mut().enumerate() {
            *f = be32(data, 8 + i * 4);
        }
        let n = fanout[255] as usize;
        let oid_start = 8 + 256 * 4;
        let crc_start = oid_start + n * 20;
        let off_start = crc_start + n * 4;
        let large_start = off_start + n * 4;
        if data.len() < large_start + 40 {
            bail!("truncated pack index");
        }
        let oids = (0..n).map(|i| Oid::from_bytes(&data[oid_start + i * 20..oid_start + i * 20 + 20])).collect::<Result<_>>()?;
        let offsets = (0..n)
            .map(|i| {
                let small = be32(data, off_start + i * 4);
                if small & 0x8000_0000 == 0 {
                    Ok(small as u64)
                } else {
                    let at = large_start + (small & 0x7fff_ffff) as usize * 8;
                    let bytes = data.get(at..at + 8).ok_or_else(|| anyhow!("bad large offset"))?;
                    Ok(u64::from_be_bytes(bytes.try_into().unwrap()))
                }
            })
            .collect::<Result<_>>()?;
        Ok(PackIndex { fanout, oids, offsets })
    }

    pub fn find(&self, oid: &Oid) -> Option<u64> {
        let first = oid.0[0] as usize;
        let lo = if first == 0 { 0 } else { self.fanout[first - 1] as usize };
        let hi = self.fanout[first] as usize;
        self.oids[lo..hi].binary_search(oid).ok().map(|i| self.offsets[lo + i])
    }

    /// Ids in this index that start with the given hex prefix.
    pub fn with_prefix(&self, prefix: &str) -> Vec<Oid> {
        self.oids.iter().filter(|o| o.to_hex().starts_with(prefix)).copied().collect()
    }

    pub fn len(&self) -> usize {
        self.oids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.oids.is_empty()
    }

    pub fn oids(&self) -> &[Oid] {
        &self.oids
    }
}

/// Write a v2 index for `entries` (id, offset, crc32 of the raw entry bytes).
pub fn write_index(entries: &mut [(Oid, u64, u32)], pack_checksum: &[u8; 20]) -> Vec<u8> {
    entries.sort_by_key(|e| e.0);
    let mut out = Vec::new();
    out.extend_from_slice(b"\xfftOc");
    out.extend_from_slice(&2u32.to_be_bytes());
    let mut counts = [0u32; 256];
    for (oid, _, _) in entries.iter() {
        counts[oid.0[0] as usize] += 1;
    }
    let mut running = 0;
    for c in counts {
        running += c;
        out.extend_from_slice(&running.to_be_bytes());
    }
    for (oid, _, _) in entries.iter() {
        out.extend_from_slice(&oid.0);
    }
    for (_, _, crc) in entries.iter() {
        out.extend_from_slice(&crc.to_be_bytes());
    }
    let mut large = Vec::new();
    for (_, off, _) in entries.iter() {
        if *off < 0x8000_0000 {
            out.extend_from_slice(&(*off as u32).to_be_bytes());
        } else {
            out.extend_from_slice(&(0x8000_0000 | large.len() as u32).to_be_bytes());
            large.push(*off);
        }
    }
    for off in large {
        out.extend_from_slice(&off.to_be_bytes());
    }
    out.extend_from_slice(pack_checksum);
    let digest = sha1_smol::Sha1::from(&out).digest().bytes();
    out.extend_from_slice(&digest);
    out
}

fn be32(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

// ---------------------------------------------------------------------------
// Entry decoding shared by readers and the indexer
// ---------------------------------------------------------------------------

struct EntryHeader {
    code: u8,
    size: usize,
    /// For OFS_DELTA: absolute offset of the base entry.
    base_offset: Option<u64>,
    /// For REF_DELTA: id of the base object.
    base_oid: Option<Oid>,
    /// Where the zlib stream starts.
    data_start: usize,
}

fn read_entry_header(pack: &[u8], offset: u64) -> Result<EntryHeader> {
    let mut pos = offset as usize;
    let byte_at = |p: usize| pack.get(p).copied().ok_or_else(|| anyhow!("truncated pack entry at {offset}"));
    let mut c = byte_at(pos)?;
    pos += 1;
    let code = (c >> 4) & 7;
    let mut size = (c & 15) as usize;
    let mut shift = 4;
    while c & 0x80 != 0 {
        c = byte_at(pos)?;
        pos += 1;
        size |= ((c & 0x7f) as usize) << shift;
        shift += 7;
    }
    let mut base_offset = None;
    let mut base_oid = None;
    if code == OFS_DELTA {
        let mut c = byte_at(pos)?;
        pos += 1;
        let mut rel = (c & 0x7f) as u64;
        while c & 0x80 != 0 {
            c = byte_at(pos)?;
            pos += 1;
            rel = ((rel + 1) << 7) | (c & 0x7f) as u64;
        }
        base_offset = Some(offset.checked_sub(rel).ok_or_else(|| anyhow!("OFS_DELTA points before pack start"))?);
    } else if code == REF_DELTA {
        base_oid = Some(Oid::from_bytes(pack.get(pos..pos + 20).ok_or_else(|| anyhow!("truncated REF_DELTA"))?)?);
        pos += 20;
    }
    Ok(EntryHeader { code, size, base_offset, base_oid, data_start: pos })
}

/// Inflate the zlib stream at `start`, returning the data and how many compressed bytes it used.
fn inflate(pack: &[u8], start: usize, size: usize) -> Result<(Vec<u8>, usize)> {
    let mut z = Decompress::new(true);
    let mut out = Vec::with_capacity(size);
    loop {
        if out.capacity() == out.len() {
            out.reserve(64.max(size));
        }
        let input = &pack[start + z.total_in() as usize..];
        let status = z.decompress_vec(input, &mut out, FlushDecompress::None).context("corrupt zlib stream in pack")?;
        match status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError if input.is_empty() => bail!("truncated zlib stream in pack"),
            Status::Ok | Status::BufError => {}
        }
    }
    if out.len() != size {
        bail!("pack entry inflated to {} bytes, expected {size}", out.len());
    }
    Ok((out, z.total_in() as usize))
}

// ---------------------------------------------------------------------------
// Reading objects out of an indexed pack
// ---------------------------------------------------------------------------

type Resolved = (Kind, Rc<Vec<u8>>);

/// Looks up REF_DELTA bases that aren't in the pack being read.
pub type ExternalBase<'a> = &'a dyn Fn(&Oid) -> Result<(Kind, Vec<u8>)>;

pub struct Pack {
    data: memmap2::Mmap,
    pub index: PackIndex,
    /// Recently materialised entries, so walking history doesn't re-apply long delta chains.
    cache: RefCell<HashMap<u64, Resolved>>,
}

const CACHE_LIMIT: usize = 512;

impl Pack {
    pub fn open(idx_path: &Path) -> Result<Pack> {
        let index = PackIndex::parse(&std::fs::read(idx_path)?).with_context(|| format!("reading {}", idx_path.display()))?;
        let pack_path = idx_path.with_extension("pack");
        let file = std::fs::File::open(&pack_path).with_context(|| format!("opening {}", pack_path.display()))?;
        // SAFETY: packfiles are immutable once written (Git and twig only ever create new ones),
        // so the mapping can't change underneath us.
        let data = unsafe { memmap2::Mmap::map(&file)? };
        if data.len() < 32 || &data[..4] != b"PACK" {
            bail!("{} is not a packfile", pack_path.display());
        }
        Ok(Pack { data, index, cache: RefCell::new(HashMap::new()) })
    }

    pub fn contains(&self, oid: &Oid) -> bool {
        self.index.find(oid).is_some()
    }

    /// Read an object; `external` resolves REF_DELTA bases that live outside this pack.
    pub fn read(&self, oid: &Oid, external: ExternalBase) -> Result<Option<(Kind, Vec<u8>)>> {
        let Some(offset) = self.index.find(oid) else { return Ok(None) };
        let (kind, data) = self.read_at(offset, external)?;
        Ok(Some((kind, Rc::try_unwrap(data).unwrap_or_else(|rc| (*rc).clone()))))
    }

    fn read_at(&self, offset: u64, external: ExternalBase) -> Result<Resolved> {
        if let Some(hit) = self.cache.borrow().get(&offset) {
            return Ok(hit.clone());
        }
        let h = read_entry_header(&self.data, offset)?;
        let (raw, _) = inflate(&self.data, h.data_start, h.size)?;
        let resolved = match (h.base_offset, h.base_oid) {
            (Some(base_off), _) => {
                let (kind, base) = self.read_at(base_off, external)?;
                (kind, Rc::new(delta::apply(&base, &raw)?))
            }
            (None, Some(base_oid)) => {
                let (kind, base) = match self.index.find(&base_oid) {
                    Some(off) => self.read_at(off, external)?,
                    None => {
                        let (k, d) = external(&base_oid)?;
                        (k, Rc::new(d))
                    }
                };
                (kind, Rc::new(delta::apply(&base, &raw)?))
            }
            _ => {
                let kind = Kind::from_pack_code(h.code).ok_or_else(|| anyhow!("bad pack object type {}", h.code))?;
                (kind, Rc::new(raw))
            }
        };
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(offset, resolved.clone());
        Ok(resolved)
    }
}

// ---------------------------------------------------------------------------
// Indexing a freshly received pack (what `git index-pack` does)
// ---------------------------------------------------------------------------

pub struct IndexedPack {
    /// (id, offset, crc32) for every object.
    pub entries: Vec<(Oid, u64, u32)>,
    pub checksum: [u8; 20],
}

/// Parse every entry, resolve all deltas and compute object ids, like `git index-pack`.
///
/// Memory stays bounded by the longest delta chain rather than the pack's unpacked size:
/// non-delta objects are hashed as they're read, and deltas are resolved by walking from each
/// base to the deltas that use it, so a base's content is only kept while its children are
/// being resolved.
pub fn index_pack(pack: &[u8], mut progress: impl FnMut(usize, usize)) -> Result<IndexedPack> {
    if pack.len() < 32 || &pack[..4] != b"PACK" {
        bail!("not a packfile");
    }
    let version = be32(pack, 4);
    if version != 2 && version != 3 {
        bail!("unsupported pack version {version}");
    }
    let count = be32(pack, 8) as usize;
    let body_end = pack.len() - 20;
    let expected: [u8; 20] = pack[body_end..].try_into().unwrap();
    if sha1_smol::Sha1::from(&pack[..body_end]).digest().bytes() != expected {
        bail!("pack checksum mismatch: the download is corrupt");
    }

    struct Entry {
        offset: u64,
        data_start: usize,
        size: usize,
        crc: u32,
        resolved: Option<(Kind, Oid)>,
    }
    let mut entries: Vec<Entry> = Vec::with_capacity(count);
    let mut ofs_children: HashMap<u64, Vec<usize>> = HashMap::new();
    let mut ref_children: HashMap<Oid, Vec<usize>> = HashMap::new();
    let mut pos = 12usize;
    let mut done = 0;
    for i in 0..count {
        let h = read_entry_header(pack, pos as u64)?;
        let (data, used) = inflate(pack, h.data_start, h.size)?;
        let end = h.data_start + used;
        let resolved = match (h.base_offset, h.base_oid) {
            (Some(off), _) => {
                ofs_children.entry(off).or_default().push(i);
                None
            }
            (None, Some(oid)) => {
                ref_children.entry(oid).or_default().push(i);
                None
            }
            _ => {
                let kind = Kind::from_pack_code(h.code).ok_or_else(|| anyhow!("bad pack object type {}", h.code))?;
                done += 1;
                Some((kind, Oid::hash_object(kind, &data)))
            }
        };
        entries.push(Entry { offset: pos as u64, data_start: h.data_start, size: h.size, crc: crc32(&pack[pos..end]), resolved });
        pos = end;
    }
    if pos != body_end {
        bail!("pack has {} trailing bytes", body_end - pos);
    }
    progress(done, count);

    struct Walk<'a> {
        pack: &'a [u8],
        entries: &'a mut Vec<Entry>,
        ofs_children: &'a HashMap<u64, Vec<usize>>,
        ref_children: &'a HashMap<Oid, Vec<usize>>,
        done: usize,
    }
    impl Walk<'_> {
        fn children(&self, i: usize) -> Vec<usize> {
            let e = &self.entries[i];
            let mut kids = self.ofs_children.get(&e.offset).cloned().unwrap_or_default();
            if let Some((_, oid)) = e.resolved
                && let Some(more) = self.ref_children.get(&oid)
            {
                kids.extend(more);
            }
            kids
        }

        /// Resolve every delta that (transitively) builds on entry `i`, whose content is `data`.
        fn resolve_from(&mut self, i: usize, kind: Kind, data: &[u8], progress: &mut dyn FnMut(usize)) -> Result<()> {
            for c in self.children(i) {
                if self.entries[c].resolved.is_some() {
                    continue;
                }
                let (delta_data, _) = inflate(self.pack, self.entries[c].data_start, self.entries[c].size)?;
                let content = delta::apply(data, &delta_data)?;
                self.entries[c].resolved = Some((kind, Oid::hash_object(kind, &content)));
                self.done += 1;
                if self.done.is_multiple_of(1000) {
                    progress(self.done);
                }
                self.resolve_from(c, kind, &content, progress)?;
            }
            Ok(())
        }
    }

    let mut walk = Walk { pack, entries: &mut entries, ofs_children: &ofs_children, ref_children: &ref_children, done };
    let mut report = |d: usize| progress(d, count);
    for i in 0..count {
        let Some((kind, _)) = walk.entries[i].resolved else { continue };
        let is_root = Kind::from_pack_code(read_entry_header(pack, walk.entries[i].offset)?.code).is_some();
        if !is_root || walk.children(i).is_empty() {
            continue;
        }
        let (data, _) = inflate(pack, walk.entries[i].data_start, walk.entries[i].size)?;
        walk.resolve_from(i, kind, &data, &mut report)?;
    }
    let done = walk.done;
    report(done);
    if done != count {
        bail!("pack has {} deltas with missing bases (thin pack?)", count - done);
    }
    let entries = entries.iter().map(|e| (e.resolved.unwrap().1, e.offset, e.crc)).collect();
    Ok(IndexedPack { entries, checksum: expected })
}

pub fn crc32(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, slot) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *slot = c;
        }
        t
    });
    let mut crc = !0u32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_reference() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn ofs_delta_offset_encoding() {
        // Entry at offset 300 whose base is 300 - 200 = 100. 200 encodes as [0x80|0, 0x48]:
        // rel = 0; then ((0 + 1) << 7) | 0x48 = 128 + 72 = 200.
        let mut pack = vec![0u8; 300];
        pack.extend_from_slice(&[(OFS_DELTA << 4) | 3, 0x80, 0x48]);
        let h = read_entry_header(&pack, 300).unwrap();
        assert_eq!(h.code, OFS_DELTA);
        assert_eq!(h.size, 3);
        assert_eq!(h.base_offset, Some(100));
        assert_eq!(h.data_start, 303);
    }
}
