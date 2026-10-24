//! Object IDs: the 20-byte SHA-1 that names every Git object.

use std::fmt;

use anyhow::{Result, bail};

use crate::object::Kind;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Oid(pub [u8; 20]);

impl Oid {
    pub const ZERO: Oid = Oid([0; 20]);

    pub fn from_bytes(bytes: &[u8]) -> Result<Oid> {
        let arr: [u8; 20] = bytes.try_into().map_err(|_| anyhow::anyhow!("object id must be 20 bytes"))?;
        Ok(Oid(arr))
    }

    pub fn from_hex(hex: &str) -> Result<Oid> {
        let hex = hex.trim();
        if hex.len() != 40 {
            bail!("invalid object id {hex:?}: expected 40 hex digits");
        }
        let mut out = [0u8; 20];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            out[i] = (hex_val(chunk[0])? << 4) | hex_val(chunk[1])?;
        }
        Ok(Oid(out))
    }

    /// The id Git would give `data` stored as an object of type `kind`.
    pub fn hash_object(kind: Kind, data: &[u8]) -> Oid {
        let mut h = sha1_smol::Sha1::new();
        h.update(kind.as_str().as_bytes());
        h.update(b" ");
        h.update(data.len().to_string().as_bytes());
        h.update(b"\0");
        h.update(data);
        Oid(h.digest().bytes())
    }

    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(40);
        for b in self.0 {
            s.push(DIGITS[(b >> 4) as usize] as char);
            s.push(DIGITS[(b & 15) as usize] as char);
        }
        s
    }

    /// Abbreviated hex form, as shown by `log --oneline`.
    pub fn short(&self) -> String {
        self.to_hex()[..7].to_string()
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 20]
    }
}

fn hex_val(c: u8) -> Result<u8> {
    Ok(match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => bail!("invalid hex digit {:?}", c as char),
    })
}

pub fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_hexdigit())
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let hex = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        assert_eq!(Oid::from_hex(hex).unwrap().to_hex(), hex);
        assert!(Oid::from_hex("e69de").is_err());
        assert!(Oid::from_hex(&"g".repeat(40)).is_err());
    }

    #[test]
    fn known_hashes() {
        // `git hash-object -t blob /dev/null` and `git mktree < /dev/null`
        assert_eq!(Oid::hash_object(Kind::Blob, b"").to_hex(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(Oid::hash_object(Kind::Tree, b"").to_hex(), "4b825dc642cb6eb9a060e54bf8d69288fbee4904");
        assert_eq!(Oid::hash_object(Kind::Blob, b"hello\n").to_hex(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }
}
