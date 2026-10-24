//! The four object kinds and their on-disk formats.

use std::cmp::Ordering;
use std::fmt;

use anyhow::{Context, Result, anyhow, bail};

use crate::oid::Oid;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Commit,
    Tree,
    Blob,
    Tag,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Commit => "commit",
            Kind::Tree => "tree",
            Kind::Blob => "blob",
            Kind::Tag => "tag",
        }
    }

    pub fn parse(s: &str) -> Result<Kind> {
        Ok(match s {
            "commit" => Kind::Commit,
            "tree" => Kind::Tree,
            "blob" => Kind::Blob,
            "tag" => Kind::Tag,
            _ => bail!("unknown object type {s:?}"),
        })
    }

    /// Type codes used in packfile entry headers.
    pub fn from_pack_code(code: u8) -> Option<Kind> {
        Some(match code {
            1 => Kind::Commit,
            2 => Kind::Tree,
            3 => Kind::Blob,
            4 => Kind::Tag,
            _ => return None,
        })
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Trees
// ---------------------------------------------------------------------------

pub mod mode {
    pub const TREE: u32 = 0o040000;
    pub const FILE: u32 = 0o100644;
    pub const EXEC: u32 = 0o100755;
    pub const SYMLINK: u32 = 0o120000;
    pub const GITLINK: u32 = 0o160000;

    pub fn is_tree(m: u32) -> bool {
        m & 0o170000 == TREE
    }

    pub fn is_gitlink(m: u32) -> bool {
        m & 0o170000 == GITLINK
    }

    /// Git only records these file modes; anything else is normalised.
    pub fn normalize(m: u32) -> u32 {
        match m & 0o170000 {
            0o040000 => TREE,
            0o120000 => SYMLINK,
            0o160000 => GITLINK,
            _ if m & 0o111 != 0 => EXEC,
            _ => FILE,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: u32,
    pub name: String,
    pub oid: Oid,
}

impl TreeEntry {
    pub fn is_tree(&self) -> bool {
        mode::is_tree(self.mode)
    }
}

/// Git's tree order: byte order, but a directory sorts as if its name ended in '/'.
pub fn tree_order(a: &TreeEntry, b: &TreeEntry) -> Ordering {
    let ka = a.name.as_bytes().iter().copied().chain(a.is_tree().then_some(b'/'));
    let kb = b.name.as_bytes().iter().copied().chain(b.is_tree().then_some(b'/'));
    ka.cmp(kb)
}

pub fn parse_tree(data: &[u8]) -> Result<Vec<TreeEntry>> {
    let mut entries = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let sp = data[pos..].iter().position(|&c| c == b' ').ok_or_else(|| anyhow!("corrupt tree: no mode"))?;
        let mode_str = std::str::from_utf8(&data[pos..pos + sp])?;
        let mode = u32::from_str_radix(mode_str, 8).with_context(|| format!("corrupt tree mode {mode_str:?}"))?;
        pos += sp + 1;
        let nul = data[pos..].iter().position(|&c| c == 0).ok_or_else(|| anyhow!("corrupt tree: no name"))?;
        let name = String::from_utf8_lossy(&data[pos..pos + nul]).into_owned();
        pos += nul + 1;
        if pos + 20 > data.len() {
            bail!("corrupt tree: truncated object id");
        }
        let oid = Oid::from_bytes(&data[pos..pos + 20])?;
        pos += 20;
        entries.push(TreeEntry { mode, name, oid });
    }
    Ok(entries)
}

pub fn serialize_tree(entries: &mut [TreeEntry]) -> Vec<u8> {
    entries.sort_by(tree_order);
    let mut out = Vec::new();
    for e in entries.iter() {
        out.extend_from_slice(format!("{:o} {}", e.mode, e.name).as_bytes());
        out.push(0);
        out.extend_from_slice(&e.oid.0);
    }
    out
}

// ---------------------------------------------------------------------------
// Signatures ("Name <email> 1700000000 +0530")
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub email: String,
    pub time: i64,
    /// Offset from UTC in minutes.
    pub tz: i32,
}

impl Signature {
    pub fn parse(s: &str) -> Result<Signature> {
        let lt = s.find('<').ok_or_else(|| anyhow!("bad signature {s:?}"))?;
        let gt = s.rfind('>').ok_or_else(|| anyhow!("bad signature {s:?}"))?;
        let name = s[..lt].trim_end().to_string();
        let email = s[lt + 1..gt].to_string();
        let mut rest = s[gt + 1..].split_whitespace();
        let time = rest.next().and_then(|t| t.parse().ok()).unwrap_or(0);
        let tz = rest.next().map(parse_tz).transpose()?.unwrap_or(0);
        Ok(Signature { name, email, time, tz })
    }

    pub fn tz_string(&self) -> String {
        format_tz(self.tz)
    }
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} <{}> {} {}", self.name, self.email, self.time, self.tz_string())
    }
}

pub fn parse_tz(s: &str) -> Result<i32> {
    let (sign, digits) = match s.as_bytes().first() {
        Some(b'+') => (1, &s[1..]),
        Some(b'-') => (-1, &s[1..]),
        _ => (1, s),
    };
    if digits.len() != 4 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        bail!("bad timezone {s:?}");
    }
    let hours: i32 = digits[..2].parse()?;
    let mins: i32 = digits[2..].parse()?;
    Ok(sign * (hours * 60 + mins))
}

pub fn format_tz(tz: i32) -> String {
    let sign = if tz < 0 { '-' } else { '+' };
    let a = tz.abs();
    format!("{sign}{:02}{:02}", a / 60, a % 60)
}

// ---------------------------------------------------------------------------
// Commits and tags: a header block, a blank line, then the message
// ---------------------------------------------------------------------------

/// Split "key value" header lines (with space-indented continuation lines) from the message.
fn parse_headers(data: &[u8]) -> (Vec<(String, String)>, String) {
    let text = String::from_utf8_lossy(data);
    let (head, message) = match text.find("\n\n") {
        Some(i) => (&text[..i], text[i + 2..].to_string()),
        None => (text.trim_end_matches('\n'), String::new()),
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head.split('\n') {
        if let Some(cont) = line.strip_prefix(' ') {
            if let Some(last) = headers.last_mut() {
                last.1.push('\n');
                last.1.push_str(cont);
            }
        } else if let Some((k, v)) = line.split_once(' ') {
            headers.push((k.to_string(), v.to_string()));
        } else if !line.is_empty() {
            headers.push((line.to_string(), String::new()));
        }
    }
    (headers, message)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Signature,
    pub committer: Signature,
    /// Headers we don't interpret (gpgsig, encoding, mergetag...), kept for display.
    pub extra: Vec<(String, String)>,
    pub message: String,
}

impl Commit {
    pub fn parse(data: &[u8]) -> Result<Commit> {
        let (headers, message) = parse_headers(data);
        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        let mut extra = Vec::new();
        for (k, v) in headers {
            match k.as_str() {
                "tree" => tree = Some(Oid::from_hex(&v)?),
                "parent" => parents.push(Oid::from_hex(&v)?),
                "author" => author = Some(Signature::parse(&v)?),
                "committer" => committer = Some(Signature::parse(&v)?),
                _ => extra.push((k, v)),
            }
        }
        let author = author.ok_or_else(|| anyhow!("commit has no author"))?;
        Ok(Commit {
            tree: tree.ok_or_else(|| anyhow!("commit has no tree"))?,
            parents,
            committer: committer.unwrap_or_else(|| author.clone()),
            author,
            extra,
            message,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut s = format!("tree {}\n", self.tree);
        for p in &self.parents {
            s.push_str(&format!("parent {p}\n"));
        }
        s.push_str(&format!("author {}\ncommitter {}\n", self.author, self.committer));
        for (k, v) in &self.extra {
            s.push_str(&format!("{k} {}\n", v.replace('\n', "\n ")));
        }
        s.push('\n');
        s.push_str(&self.message);
        s.into_bytes()
    }

    /// First line of the message.
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    pub object: Oid,
    pub kind: Kind,
    pub name: String,
    pub tagger: Option<Signature>,
    pub message: String,
}

impl Tag {
    pub fn parse(data: &[u8]) -> Result<Tag> {
        let (headers, message) = parse_headers(data);
        let get = |key: &str| headers.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        Ok(Tag {
            object: Oid::from_hex(get("object").ok_or_else(|| anyhow!("tag has no object"))?)?,
            kind: Kind::parse(get("type").unwrap_or("commit"))?,
            name: get("tag").unwrap_or_default().to_string(),
            tagger: get("tagger").map(Signature::parse).transpose()?,
            message,
        })
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut s = format!("object {}\ntype {}\ntag {}\n", self.object, self.kind, self.name);
        if let Some(t) = &self.tagger {
            s.push_str(&format!("tagger {t}\n"));
        }
        s.push('\n');
        s.push_str(&self.message);
        s.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_roundtrip_and_git_ordering() {
        let z = Oid::ZERO;
        let mut entries = vec![
            TreeEntry { mode: mode::FILE, name: "foo.c".into(), oid: z },
            TreeEntry { mode: mode::TREE, name: "foo".into(), oid: z },
            TreeEntry { mode: mode::FILE, name: "foo-bar".into(), oid: z },
            TreeEntry { mode: mode::EXEC, name: "a".into(), oid: z },
        ];
        let bytes = serialize_tree(&mut entries);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        // "foo/" sorts after "foo.c" and "foo-bar" because '/' > '.' and '-'
        assert_eq!(names, ["a", "foo-bar", "foo.c", "foo"]);
        assert_eq!(parse_tree(&bytes).unwrap(), entries);
        assert!(bytes.starts_with(b"100755 a\0"));
        assert!(String::from_utf8_lossy(&bytes).contains("40000 foo\0"));
    }

    #[test]
    fn commit_roundtrip_with_signature_header() {
        let raw = b"tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n\
parent e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\n\
author A U Thor <a@example.com> 1700000000 +0530\n\
committer C O Mitter <c@example.com> 1700000100 -0800\n\
gpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\
\n\
Subject line\n\nBody text\n";
        let c = Commit::parse(raw).unwrap();
        assert_eq!(c.parents.len(), 1);
        assert_eq!(c.author.name, "A U Thor");
        assert_eq!(c.author.tz, 330);
        assert_eq!(c.committer.tz, -480);
        assert_eq!(c.summary(), "Subject line");
        assert_eq!(c.extra[0].0, "gpgsig");
        assert_eq!(c.serialize(), raw.to_vec());
    }

    #[test]
    fn tag_roundtrip() {
        let raw = b"object e69de29bb2d1d6434b8b29ae775ad8c2e48c5391\ntype blob\ntag v1\ntagger T <t@x> 5 +0000\n\nrelease\n";
        let t = Tag::parse(raw).unwrap();
        assert_eq!(t.kind, Kind::Blob);
        assert_eq!(t.name, "v1");
        assert_eq!(t.serialize(), raw.to_vec());
    }

    #[test]
    fn timezones() {
        assert_eq!(parse_tz("+0000").unwrap(), 0);
        assert_eq!(parse_tz("-0130").unwrap(), -90);
        assert_eq!(format_tz(-90), "-0130");
        assert_eq!(format_tz(345), "+0545");
        assert!(parse_tz("+5").is_err());
    }
}
