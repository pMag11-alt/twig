//! Line diffs: Myers' O(ND) algorithm (linear-space, divide and conquer) and unified output.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Equal,
    Delete,
    Insert,
}

/// One line of an edit script. `a`/`b` are line indexes into the old/new sides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub op: Op,
    pub a: usize,
    pub b: usize,
}

/// Split into lines, keeping each line's terminator so "no newline at end" survives.
pub fn lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, &c) in data.iter().enumerate() {
        if c == b'\n' {
            out.push(&data[start..=i]);
            start = i + 1;
        }
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

/// A shortest edit script turning `a` into `b`.
pub fn diff<T: Eq + std::hash::Hash>(a: &[T], b: &[T]) -> Vec<Edit> {
    // Intern lines as integers so the inner loop compares u32s.
    let mut ids: HashMap<&T, u32> = HashMap::new();
    let mut intern = |x| {
        let n = ids.len() as u32;
        *ids.entry(x).or_insert(n)
    };
    let a_ids: Vec<u32> = a.iter().map(&mut intern).collect();
    let b_ids: Vec<u32> = b.iter().map(&mut intern).collect();
    let mut out = Vec::with_capacity(a.len() + b.len());
    diff_range(&a_ids, &b_ids, 0, 0, &mut out);
    out
}

fn diff_range(a: &[u32], b: &[u32], a_off: usize, b_off: usize, out: &mut Vec<Edit>) {
    // Common prefix and suffix are always part of some shortest script.
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    for i in 0..prefix {
        out.push(Edit { op: Op::Equal, a: a_off + i, b: b_off + i });
    }
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let (am, bm) = (a_off + prefix, b_off + prefix);
    if a_mid.is_empty() {
        out.extend((0..b_mid.len()).map(|j| Edit { op: Op::Insert, a: am, b: bm + j }));
    } else if b_mid.is_empty() {
        out.extend((0..a_mid.len()).map(|i| Edit { op: Op::Delete, a: am + i, b: bm }));
    } else {
        match bisect(a_mid, b_mid) {
            Some((x, y)) => {
                diff_range(&a_mid[..x], &b_mid[..y], am, bm, out);
                diff_range(&a_mid[x..], &b_mid[y..], am + x, bm + y, out);
            }
            None => {
                out.extend((0..a_mid.len()).map(|i| Edit { op: Op::Delete, a: am + i, b: bm }));
                out.extend((0..b_mid.len()).map(|j| Edit { op: Op::Insert, a: am + a_mid.len(), b: bm + j }));
            }
        }
    }
    let (a_end, b_end) = (a_off + a.len() - suffix, b_off + b.len() - suffix);
    for i in 0..suffix {
        out.push(Edit { op: Op::Equal, a: a_end + i, b: b_end + i });
    }
}

/// Find a point on a shortest edit path by searching forwards from the start and backwards
/// from the end at the same time until the two frontiers overlap (Myers' "middle snake").
fn bisect(a: &[u32], b: &[u32]) -> Option<(usize, usize)> {
    let (n, m) = (a.len() as isize, b.len() as isize);
    let max_d = (n + m + 1) / 2;
    let offset = max_d + 1;
    let len = (2 * max_d + 3) as usize;
    let mut v1 = vec![-1isize; len];
    let mut v2 = vec![-1isize; len];
    v1[(offset + 1) as usize] = 0;
    v2[(offset + 1) as usize] = 0;
    let delta = n - m;
    let front = delta % 2 != 0;
    let (mut k1start, mut k1end, mut k2start, mut k2end) = (0, 0, 0, 0);
    for d in 0..max_d {
        let mut k1 = -d + k1start;
        while k1 <= d - k1end {
            let i = (offset + k1) as usize;
            let mut x1 = if k1 == -d || (k1 != d && v1[i - 1] < v1[i + 1]) { v1[i + 1] } else { v1[i - 1] + 1 };
            let mut y1 = x1 - k1;
            while x1 < n && y1 < m && a[x1 as usize] == b[y1 as usize] {
                x1 += 1;
                y1 += 1;
            }
            v1[i] = x1;
            if x1 > n {
                k1end += 2;
            } else if y1 > m {
                k1start += 2;
            } else if front {
                let j = offset + delta - k1;
                if j >= 0 && (j as usize) < len && v2[j as usize] != -1 {
                    let x2 = n - v2[j as usize];
                    if x1 >= x2 {
                        return Some((x1 as usize, y1 as usize));
                    }
                }
            }
            k1 += 2;
        }
        let mut k2 = -d + k2start;
        while k2 <= d - k2end {
            let i = (offset + k2) as usize;
            let mut x2 = if k2 == -d || (k2 != d && v2[i - 1] < v2[i + 1]) { v2[i + 1] } else { v2[i - 1] + 1 };
            let mut y2 = x2 - k2;
            while x2 < n && y2 < m && a[(n - x2 - 1) as usize] == b[(m - y2 - 1) as usize] {
                x2 += 1;
                y2 += 1;
            }
            v2[i] = x2;
            if x2 > n {
                k2end += 2;
            } else if y2 > m {
                k2start += 2;
            } else if !front {
                let j = offset + delta - k2;
                if j >= 0 && (j as usize) < len && v1[j as usize] != -1 {
                    let x1 = v1[j as usize];
                    let y1 = offset + x1 - j;
                    if x1 >= n - x2 {
                        return Some((x1 as usize, y1 as usize));
                    }
                }
            }
            k2 += 2;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Unified diff output
// ---------------------------------------------------------------------------

pub struct Hunk {
    pub a_start: usize,
    pub a_len: usize,
    pub b_start: usize,
    pub b_len: usize,
    pub edits: Vec<Edit>,
}

/// Group an edit script into hunks with `context` unchanged lines around each change.
pub fn hunks(edits: &[Edit], context: usize) -> Vec<Hunk> {
    let changes: Vec<usize> = edits.iter().enumerate().filter(|(_, e)| e.op != Op::Equal).map(|(i, _)| i).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < changes.len() {
        let start = changes[i].saturating_sub(context);
        let mut end = changes[i];
        // extend while the next change is close enough that contexts would touch
        while i + 1 < changes.len() && changes[i + 1] - end <= 2 * context + 1 {
            i += 1;
            end = changes[i];
        }
        let end = (end + context + 1).min(edits.len());
        let slice = &edits[start..end];
        let a_len = slice.iter().filter(|e| e.op != Op::Insert).count();
        let b_len = slice.iter().filter(|e| e.op != Op::Delete).count();
        out.push(Hunk { a_start: slice[0].a, a_len, b_start: slice[0].b, b_len, edits: slice.to_vec() });
        i += 1;
    }
    out
}

pub fn is_binary(data: &[u8]) -> bool {
    data[..data.len().min(8000)].contains(&0)
}

/// Lines in Git's unified format, without the `diff --git` header. Each item is
/// (kind, text) where kind is one of ' ', '+', '-', '@' or '\\' for "No newline".
pub fn unified(old: &[u8], new: &[u8], context: usize) -> Vec<(char, String)> {
    let a = lines(old);
    let b = lines(new);
    let edits = diff(&a, &b);
    let mut out = Vec::new();
    for h in hunks(&edits, context) {
        let range = |start: usize, len: usize| match len {
            0 => format!("{start},0"),
            1 => format!("{}", start + 1),
            _ => format!("{},{len}", start + 1),
        };
        let mut header = format!("@@ -{} +{} @@", range(h.a_start, h.a_len), range(h.b_start, h.b_len));
        if let Some(func) = function_context(&a, h.a_start) {
            header.push(' ');
            header.push_str(&func);
        }
        out.push(('@', header));
        for e in &h.edits {
            let (kind, line) = match e.op {
                Op::Equal => (' ', a[e.a]),
                Op::Delete => ('-', a[e.a]),
                Op::Insert => ('+', b[e.b]),
            };
            let text = String::from_utf8_lossy(line);
            match text.strip_suffix('\n') {
                Some(t) => out.push((kind, t.to_string())),
                None => {
                    out.push((kind, text.into_owned()));
                    out.push(('\\', "\\ No newline at end of file".to_string()));
                }
            }
        }
    }
    out
}

/// Git's default hunk header context: the nearest preceding line that starts with a letter,
/// '_' or '$' (a function or section header in most languages).
fn function_context(a: &[&[u8]], before: usize) -> Option<String> {
    a[..before.min(a.len())].iter().rev().find_map(|line| {
        let first = *line.first()?;
        if first.is_ascii_alphabetic() || first == b'_' || first == b'$' {
            let text = String::from_utf8_lossy(line);
            let text = text.trim_end();
            Some(text.chars().take(80).collect::<String>().trim_end().to_string())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcs_len(a: &[u8], b: &[u8]) -> usize {
        let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for i in (0..a.len()).rev() {
            for j in (0..b.len()).rev() {
                dp[i][j] = if a[i] == b[j] { dp[i + 1][j + 1] + 1 } else { dp[i + 1][j].max(dp[i][j + 1]) };
            }
        }
        dp[0][0]
    }

    fn apply(a: &[u8], b: &[u8], edits: &[Edit]) -> Vec<u8> {
        let mut out = Vec::new();
        let (mut ai, mut bi) = (0, 0);
        for e in edits {
            match e.op {
                Op::Equal => {
                    assert_eq!(a[e.a], b[e.b]);
                    assert_eq!((e.a, e.b), (ai, bi), "edits out of order");
                    out.push(a[e.a]);
                    ai += 1;
                    bi += 1;
                }
                Op::Delete => {
                    assert_eq!(e.a, ai);
                    ai += 1;
                }
                Op::Insert => {
                    assert_eq!(e.b, bi);
                    out.push(b[e.b]);
                    bi += 1;
                }
            }
        }
        assert_eq!((ai, bi), (a.len(), b.len()));
        out
    }

    /// Property test: on random inputs the script is valid and minimal (matches LCS length).
    #[test]
    fn random_diffs_are_valid_and_minimal() {
        let mut seed = 0x2545F4914F6CDD1Du64;
        let mut rand = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..3000 {
            let alphabet = 1 + rand(5) as u8;
            let a: Vec<u8> = (0..rand(25)).map(|_| rand(alphabet as u64) as u8).collect();
            let b: Vec<u8> = (0..rand(25)).map(|_| rand(alphabet as u64) as u8).collect();
            let edits = diff(&a, &b);
            assert_eq!(apply(&a, &b, &edits), b);
            let equal = edits.iter().filter(|e| e.op == Op::Equal).count();
            assert_eq!(equal, lcs_len(&a, &b), "not minimal for {a:?} -> {b:?}");
        }
    }

    #[test]
    fn unified_format() {
        let old = b"fn main() {\n    a();\n    b();\n    c();\n}\n";
        let new = b"fn main() {\n    a();\n    B();\n    c();\n}";
        let out = unified(old, new, 3);
        let text: Vec<String> = out.iter().map(|(k, l)| if *k == '@' || *k == '\\' { l.clone() } else { format!("{k}{l}") }).collect();
        assert_eq!(
            text,
            [
                "@@ -1,5 +1,5 @@",
                " fn main() {",
                "     a();",
                "-    b();",
                "+    B();",
                "     c();",
                "-}",
                "+}",
                "\\ No newline at end of file",
            ]
        );
    }

    #[test]
    fn hunks_split_and_merge_by_context() {
        let a: Vec<String> = (0..30).map(|i| format!("{i}\n")).collect();
        let mut b = a.clone();
        b[2] = "x\n".into();
        b[25] = "y\n".into();
        let old = a.concat();
        let new = b.concat();
        let headers: Vec<String> =
            unified(old.as_bytes(), new.as_bytes(), 3).into_iter().filter(|(k, _)| *k == '@').map(|(_, l)| l).collect();
        assert_eq!(headers, ["@@ -1,6 +1,6 @@", "@@ -23,7 +23,7 @@"]);
        b[9] = "z\n".into();
        let new = b.concat();
        let n = unified(old.as_bytes(), new.as_bytes(), 3).iter().filter(|(k, _)| *k == '@').count();
        assert_eq!(n, 2, "changes 7 lines apart share a hunk");
    }

    #[test]
    fn insert_into_empty_and_delete_all() {
        let out = unified(b"", b"a\nb\n", 3);
        assert_eq!(out[0].1, "@@ -0,0 +1,2 @@");
        let out = unified(b"a\n", b"", 3);
        assert_eq!(out[0].1, "@@ -1 +0,0 @@");
    }
}
