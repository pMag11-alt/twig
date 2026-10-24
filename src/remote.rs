//! Talking to other repositories: Git's smart HTTP protocol (v0) and local paths.
//!
//! A fetch is two requests. `GET /info/refs?service=git-upload-pack` returns the server's
//! refs as pkt-lines. Then `POST /git-upload-pack` sends "want <id>" lines (plus "have" lines
//! for objects we already have) and gets back a packfile, multiplexed over side-band
//! channels: 1 carries pack data, 2 carries progress messages and 3 carries errors.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};

use crate::oid::Oid;
use crate::pack;
use crate::repo::Repo;

const AGENT: &str = concat!("git/twig-", env!("CARGO_PKG_VERSION"));

/// What a remote advertises.
#[derive(Debug, Default)]
pub struct Advertisement {
    /// ref name -> id, e.g. "refs/heads/main" (peeled "^{}" entries are skipped).
    pub refs: BTreeMap<String, Oid>,
    pub head: Option<Oid>,
    /// The branch HEAD points at, from the `symref=HEAD:refs/heads/main` capability.
    pub head_target: Option<String>,
    pub capabilities: Vec<String>,
}

// ---------------------------------------------------------------------------
// pkt-line
// ---------------------------------------------------------------------------

pub fn pkt_line(payload: &[u8]) -> Vec<u8> {
    let mut v = format!("{:04x}", payload.len() + 4).into_bytes();
    v.extend_from_slice(payload);
    v
}

/// Read one pkt-line. Ok(None) is a flush packet ("0000").
fn read_pkt(r: &mut impl Read) -> Result<Option<Vec<u8>>> {
    let mut len_hex = [0u8; 4];
    r.read_exact(&mut len_hex).context("connection closed mid-response")?;
    let len = usize::from_str_radix(std::str::from_utf8(&len_hex)?, 16).context("bad pkt-line length")?;
    if len < 4 {
        return Ok(None); // 0000 flush (0001/0002 only appear in protocol v2)
    }
    let mut buf = vec![0u8; len - 4];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub fn parse_advertisement(mut body: &[u8]) -> Result<Advertisement> {
    let mut adv = Advertisement::default();
    let first = read_pkt(&mut body)?.ok_or_else(|| anyhow!("empty ref advertisement"))?;
    if first.starts_with(b"# service=") {
        // smart HTTP prefixes the advertisement with a service line and a flush
        read_pkt(&mut body)?;
    } else {
        body = &[]; // not expected: the caller passed something else
    }
    let mut first_line = true;
    while let Some(line) = read_pkt(&mut body)? {
        let line = String::from_utf8_lossy(&line).trim_end_matches('\n').to_string();
        let (refpart, caps) = match line.split_once('\0') {
            Some((r, c)) => (r.to_string(), Some(c.to_string())),
            None => (line.clone(), None),
        };
        if let Some(caps) = caps.filter(|_| first_line) {
            adv.capabilities = caps.split(' ').map(String::from).collect();
            for c in &adv.capabilities {
                if let Some(t) = c.strip_prefix("symref=HEAD:") {
                    adv.head_target = Some(t.to_string());
                }
            }
        }
        first_line = false;
        let Some((hex, name)) = refpart.split_once(' ') else { continue };
        // skip peeled tags and refs we never fetch (GitHub also advertises refs/pull/*)
        if name.ends_with("^{}") || !(name == "HEAD" || name.starts_with("refs/heads/") || name.starts_with("refs/tags/")) {
            continue;
        }
        let oid = Oid::from_hex(hex)?;
        if name == "HEAD" {
            adv.head = Some(oid);
        } else {
            adv.refs.insert(name.to_string(), oid);
        }
    }
    // Servers that don't send symref: guess the branch HEAD matches.
    if adv.head_target.is_none()
        && let Some(h) = adv.head
    {
        let mut branches = adv.refs.iter().filter(|(n, o)| n.starts_with("refs/heads/") && **o == h);
        adv.head_target = branches.next().map(|(n, _)| n.clone());
    }
    Ok(adv)
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

pub enum Remote {
    Http(String),
    Local(Repo),
}

impl Remote {
    pub fn open(url: &str) -> Result<Remote> {
        if url.starts_with("http://") || url.starts_with("https://") {
            return Ok(Remote::Http(url.trim_end_matches('/').to_string()));
        }
        let path = url.strip_prefix("file://").unwrap_or(url);
        let p = Path::new(path);
        let repo = if p.join(".git").exists() {
            Repo::discover(p)?
        } else if p.join("HEAD").is_file() && p.join("objects").is_dir() {
            Repo::open(p, p)? // bare repository
        } else {
            bail!("repository '{url}' does not exist");
        };
        Ok(Remote::Local(repo))
    }

    pub fn advertise(&self) -> Result<Advertisement> {
        match self {
            Remote::Http(url) => {
                let agent = http_agent();
                let mut resp = agent
                    .get(&format!("{url}/info/refs?service=git-upload-pack"))
                    .header("User-Agent", AGENT)
                    .call()
                    .with_context(|| format!("unable to access '{url}'"))?;
                let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                let body = resp.body_mut().with_config().limit(64 << 20).read_to_vec()?;
                if ctype != "application/x-git-upload-pack-advertisement" {
                    bail!("{url} doesn't look like a Git repository (got {ctype:?}; dumb HTTP isn't supported)");
                }
                parse_advertisement(&body)
            }
            Remote::Local(repo) => {
                let mut adv = Advertisement {
                    refs: repo.refs.list("refs/heads/")?,
                    head: repo.head()?,
                    head_target: repo.refs.head_target()?,
                    capabilities: Vec::new(),
                };
                adv.refs.extend(repo.refs.list("refs/tags/")?);
                Ok(adv)
            }
        }
    }

    /// Get every object reachable from `wants` that isn't reachable from `haves` into `into`.
    pub fn fetch_objects(&self, into: &Repo, wants: &[Oid], haves: &[Oid], progress: bool) -> Result<usize> {
        match self {
            Remote::Http(url) => {
                let mut body = Vec::new();
                for (i, w) in wants.iter().enumerate() {
                    let line = if i == 0 {
                        format!("want {w} ofs-delta side-band-64k{} agent={}\n", if progress { "" } else { " no-progress" }, &AGENT[4..])
                    } else {
                        format!("want {w}\n")
                    };
                    body.extend(pkt_line(line.as_bytes()));
                }
                body.extend_from_slice(b"0000");
                for h in haves {
                    body.extend(pkt_line(format!("have {h}\n").as_bytes()));
                }
                body.extend(pkt_line(b"done\n"));
                let agent = http_agent();
                let resp = agent
                    .post(&format!("{url}/git-upload-pack"))
                    .header("User-Agent", AGENT)
                    .header("Content-Type", "application/x-git-upload-pack-request")
                    .header("Accept", "application/x-git-upload-pack-result")
                    .send(&body[..])
                    .with_context(|| format!("fetching from '{url}'"))?;
                let pack = read_upload_pack_response(resp.into_body().into_reader(), progress)?;
                store_pack(into, &pack, progress)
            }
            Remote::Local(src) => copy_objects(src, into, wants, haves),
        }
    }
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_connect(Some(std::time::Duration::from_secs(30))).build().into()
}

/// Skip ACK/NAK lines, then demultiplex the side-band stream into the raw pack.
fn read_upload_pack_response(mut r: impl Read, progress: bool) -> Result<Vec<u8>> {
    loop {
        let line = read_pkt(&mut r)?.ok_or_else(|| anyhow!("server ended the response before sending a pack"))?;
        if line.starts_with(b"NAK") || line.starts_with(b"ACK") {
            if line.starts_with(b"NAK") || !line.ends_with(b"continue\n") {
                break;
            }
            continue;
        }
        if line.starts_with(b"ERR ") {
            bail!("remote error: {}", String::from_utf8_lossy(&line[4..]).trim());
        }
        bail!("unexpected line from server: {:?}", String::from_utf8_lossy(&line));
    }
    let mut pack = Vec::new();
    let mut stderr = std::io::stderr();
    let mut pending = Vec::new(); // progress text not yet terminated by \r or \n
    let mut last_report = 0;
    let mut mid_line = false; // our "Receiving objects" line is unterminated
    while let Some(pkt) = read_pkt(&mut r)? {
        match pkt.first() {
            Some(1) => {
                pack.extend_from_slice(&pkt[1..]);
                if progress && pack.len() - last_report >= 1 << 20 {
                    last_report = pack.len();
                    write!(stderr, "\rReceiving objects: {:.1} MiB", pack.len() as f64 / (1 << 20) as f64)?;
                    mid_line = true;
                }
            }
            Some(2) if progress => {
                // prefix every progress line, as `git` does
                for &b in &pkt[1..] {
                    pending.push(b);
                    if b == b'\r' || b == b'\n' {
                        if mid_line {
                            stderr.write_all(b"\n")?;
                            mid_line = false;
                        }
                        stderr.write_all(b"remote: ")?;
                        stderr.write_all(&pending)?;
                        pending.clear();
                    }
                }
            }
            Some(2) => {}
            Some(3) => bail!("remote error: {}", String::from_utf8_lossy(&pkt[1..]).trim()),
            _ => bail!("bad side-band packet"),
        }
    }
    if progress && last_report > 0 {
        writeln!(stderr, "\rReceiving objects: {:.1} MiB, done.", pack.len() as f64 / (1 << 20) as f64)?;
    }
    Ok(pack)
}

/// Verify and index a received pack, then add it to `repo`'s object store.
pub fn store_pack(repo: &Repo, pack_data: &[u8], progress: bool) -> Result<usize> {
    if pack_data.len() < 32 {
        bail!("received an empty or truncated pack");
    }
    let mut last = 0;
    let mut indexed = pack::index_pack(pack_data, |done, total| {
        if progress && (done == total || done - last >= 500) {
            last = done;
            eprint!("\rResolving objects: {:3}% ({done}/{total})", done * 100 / total.max(1));
        }
    })?;
    if progress {
        eprintln!(", done.");
    }
    let n = indexed.entries.len();
    let name = format!("pack-{}", hex(&indexed.checksum));
    let dir = repo.git_dir.join("objects/pack");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join(format!("{name}.pack")), pack_data)?;
    fs::write(dir.join(format!("{name}.idx")), pack::write_index(&mut indexed.entries, &indexed.checksum))?;
    Ok(n)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Local transport: copy every object reachable from `wants` (and not from `haves`).
fn copy_objects(src: &Repo, dst: &Repo, wants: &[Oid], haves: &[Oid]) -> Result<usize> {
    let mut seen: HashSet<Oid> = HashSet::new();
    let mut stop: HashSet<Oid> = HashSet::new();
    for h in haves {
        if src.odb.contains(h) {
            crate::merge::walk_history(src, &[*h], |c, _| {
                stop.insert(c);
                Ok(true)
            })?;
        }
    }
    let mut stack: Vec<Oid> = wants.iter().copied().filter(|w| !stop.contains(w)).collect();
    let mut copied = 0;
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) || stop.contains(&oid) {
            continue;
        }
        let (kind, data) = src.odb.read(&oid)?;
        if !dst.odb.contains(&oid) {
            dst.odb.write(kind, &data)?;
            copied += 1;
        }
        match kind {
            crate::object::Kind::Commit => {
                let c = crate::object::Commit::parse(&data)?;
                stack.push(c.tree);
                stack.extend(c.parents);
            }
            crate::object::Kind::Tree => {
                for e in crate::object::parse_tree(&data)? {
                    if !crate::object::mode::is_gitlink(e.mode) {
                        stack.push(e.oid);
                    }
                }
            }
            crate::object::Kind::Tag => stack.push(crate::object::Tag::parse(&data)?.object),
            crate::object::Kind::Blob => {}
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkt_lines() {
        assert_eq!(pkt_line(b"done\n"), b"0009done\n");
        let mut r: &[u8] = b"0009done\n0000";
        assert_eq!(read_pkt(&mut r).unwrap().unwrap(), b"done\n");
        assert_eq!(read_pkt(&mut r).unwrap(), None);
    }

    #[test]
    fn advertisement() {
        let a = "1111111111111111111111111111111111111111";
        let b = "2222222222222222222222222222222222222222";
        let mut body = pkt_line(b"# service=git-upload-pack\n");
        body.extend_from_slice(b"0000");
        body.extend(pkt_line(format!("{a} HEAD\0multi_ack ofs-delta symref=HEAD:refs/heads/trunk agent=git/2.40\n").as_bytes()));
        body.extend(pkt_line(format!("{a} refs/heads/trunk\n").as_bytes()));
        body.extend(pkt_line(format!("{b} refs/tags/v1\n").as_bytes()));
        body.extend(pkt_line(format!("{a} refs/tags/v1^{{}}\n").as_bytes()));
        body.extend_from_slice(b"0000");
        let adv = parse_advertisement(&body).unwrap();
        assert_eq!(adv.head, Some(Oid::from_hex(a).unwrap()));
        assert_eq!(adv.head_target.as_deref(), Some("refs/heads/trunk"));
        assert_eq!(adv.refs.len(), 2);
        assert_eq!(adv.refs["refs/tags/v1"], Oid::from_hex(b).unwrap());
        assert!(adv.capabilities.contains(&"ofs-delta".to_string()));
    }
}
