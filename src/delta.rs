//! Git's binary delta format, used by packfiles to store an object as edits to another.
//!
//! A delta is two varints (base size, result size) followed by instructions:
//! `1xxxxxxx` copies a range of the base (offset/size bytes selected by the low
//! 7 bits), and `0nnnnnnn` inserts the next n literal bytes.

use anyhow::{Result, bail};

fn varint(data: &[u8], pos: &mut usize) -> Result<usize> {
    let mut value = 0usize;
    let mut shift = 0;
    loop {
        let Some(&byte) = data.get(*pos) else { bail!("truncated delta header") };
        *pos += 1;
        value |= ((byte & 0x7f) as usize) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
}

pub fn apply(base: &[u8], delta: &[u8]) -> Result<Vec<u8>> {
    let mut pos = 0;
    let base_size = varint(delta, &mut pos)?;
    let result_size = varint(delta, &mut pos)?;
    if base_size != base.len() {
        bail!("delta expects a {base_size}-byte base, got {}", base.len());
    }
    let mut out = Vec::with_capacity(result_size);
    while pos < delta.len() {
        let op = delta[pos];
        pos += 1;
        if op & 0x80 != 0 {
            let mut offset = 0usize;
            let mut size = 0usize;
            for i in 0..4 {
                if op & (1 << i) != 0 {
                    let Some(&b) = delta.get(pos) else { bail!("truncated copy instruction") };
                    offset |= (b as usize) << (8 * i);
                    pos += 1;
                }
            }
            for i in 0..3 {
                if op & (0x10 << i) != 0 {
                    let Some(&b) = delta.get(pos) else { bail!("truncated copy instruction") };
                    size |= (b as usize) << (8 * i);
                    pos += 1;
                }
            }
            if size == 0 {
                size = 0x10000;
            }
            let Some(chunk) = base.get(offset..offset + size) else { bail!("delta copy out of bounds") };
            out.extend_from_slice(chunk);
        } else if op != 0 {
            let n = op as usize;
            let Some(chunk) = delta.get(pos..pos + n) else { bail!("truncated insert instruction") };
            out.extend_from_slice(chunk);
            pos += n;
        } else {
            bail!("invalid delta opcode 0");
        }
    }
    if out.len() != result_size {
        bail!("delta produced {} bytes, header promised {result_size}", out.len());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_and_insert() {
        let base = b"hello, world";
        // base 12, result 13; copy offset 0 len 7 ("hello, "), insert "there", copy offset 7 len 1 ("w")
        let delta = [12, 13, 0x90, 7, 5, b't', b'h', b'e', b'r', b'e', 0x91, 7, 1];
        assert_eq!(apply(base, &delta).unwrap(), b"hello, therew");
    }

    #[test]
    fn rejects_bad_deltas() {
        assert!(apply(b"abc", &[4, 1, 1, b'x']).is_err()); // wrong base size
        assert!(apply(b"abc", &[3, 1, 0x91, 5, 1]).is_err()); // copy out of range
        assert!(apply(b"abc", &[3, 2, 1, b'x']).is_err()); // result size mismatch
    }

    #[test]
    fn size_zero_means_64k() {
        let base = vec![7u8; 0x10000];
        let delta = [0x80, 0x80, 0x04, 0x80, 0x80, 0x04, 0x80];
        assert_eq!(apply(&base, &delta).unwrap().len(), 0x10000);
    }
}
