//! Three-way merging: merge bases, line-level diff3, and whole-tree merges.

use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet};

use anyhow::Result;

use crate::diff::{self, Op};
use crate::object::{Kind, mode};
use crate::oid::Oid;
use crate::repo::{FlatTree, Repo};

// ---------------------------------------------------------------------------
// Commit graph
// ---------------------------------------------------------------------------

fn ancestors(repo: &Repo, start: Oid) -> Result<HashSet<Oid>> {
    let mut seen = HashSet::from([start]);
    let mut stack = vec![start];
    while let Some(c) = stack.pop() {
        for p in repo.odb.read_commit(&c)?.parents {
            if seen.insert(p) {
                stack.push(p);
            }
        }
    }
    Ok(seen)
}

pub fn is_ancestor(repo: &Repo, ancestor: Oid, of: Oid) -> Result<bool> {
    Ok(ancestors(repo, of)?.contains(&ancestor))
}

/// Best common ancestors of `a` and `b`: common ancestors that aren't ancestors of another one.
pub fn merge_bases(repo: &Repo, a: Oid, b: Oid) -> Result<Vec<Oid>> {
    let of_a = ancestors(repo, a)?;
    // Walk back from b newest-first; the first common commits on each path are candidates.
    let mut candidates = Vec::new();
    let mut seen = HashSet::from([b]);
    let mut queue = BinaryHeap::new();
    queue.push((repo.odb.read_commit(&b)?.committer.time, b));
    while let Some((_, c)) = queue.pop() {
        if of_a.contains(&c) {
            candidates.push(c);
            continue;
        }
        for p in repo.odb.read_commit(&c)?.parents {
            if seen.insert(p) {
                queue.push((repo.odb.read_commit(&p)?.committer.time, p));
            }
        }
    }
    let mut best = Vec::new();
    for &c in &candidates {
        let mut redundant = false;
        for &other in &candidates {
            if other != c && is_ancestor(repo, c, other)? {
                redundant = true;
                break;
            }
        }
        if !redundant && !best.contains(&c) {
            best.push(c);
        }
    }
    Ok(best)
}

// ---------------------------------------------------------------------------
// diff3 over lines
// ---------------------------------------------------------------------------

pub struct LineMerge {
    pub content: Vec<u8>,
    pub conflicts: usize,
}

enum Chunk<'a> {
    Clean(Vec<&'a [u8]>),
    Conflict(Vec<&'a [u8]>, Vec<&'a [u8]>),
}

/// 1-based "base line -> other line" map for the lines a diff keeps.
fn matches(base: &[&[u8]], other: &[&[u8]]) -> Vec<Option<usize>> {
    let mut m = vec![None; base.len() + 2];
    for e in diff::diff(base, other) {
        if e.op == Op::Equal {
            m[e.a + 1] = Some(e.b + 1);
        }
    }
    m
}

/// Merge two edits of `base`, in the style of diff3: regions only one side changed are taken
/// from that side, and regions both sides changed differently become conflicts.
pub fn merge_lines(base: &[u8], ours: &[u8], theirs: &[u8], ours_label: &str, theirs_label: &str) -> LineMerge {
    let o = diff::lines(base);
    let a = diff::lines(ours);
    let b = diff::lines(theirs);
    let ma = matches(&o, &a);
    let mb = matches(&o, &b);
    let (mut po, mut pa, mut pb) = (0usize, 0usize, 0usize);
    let mut chunks: Vec<Chunk> = Vec::new();

    fn write<'a>(chunks: &mut Vec<Chunk<'a>>, so: &[&'a [u8]], sa: &[&'a [u8]], sb: &[&'a [u8]]) {
        if sa == so || sa == sb {
            chunks.push(Chunk::Clean(sb.to_vec()));
        } else if sb == so {
            chunks.push(Chunk::Clean(sa.to_vec()));
        } else {
            chunks.push(Chunk::Conflict(sa.to_vec(), sb.to_vec()));
        }
    }

    loop {
        let in_bounds = |i: usize, po: usize, pa: usize, pb: usize| po + i <= o.len() || pa + i <= a.len() || pb + i <= b.len();
        let lockstep = |i: usize, po: usize, pa: usize, pb: usize| {
            ma.get(po + i).copied().flatten() == Some(pa + i) && mb.get(po + i).copied().flatten() == Some(pb + i)
        };
        let mut i = 1;
        while in_bounds(i, po, pa, pb) && lockstep(i, po, pa, pb) {
            i += 1;
        }
        if !in_bounds(i, po, pa, pb) {
            write(&mut chunks, &o[po..], &a[pa..], &b[pb..]);
            break;
        }
        if i > 1 {
            // i-1 lines that all three agree on
            write(&mut chunks, &o[po..po + i - 1], &a[pa..pa + i - 1], &b[pb..pb + i - 1]);
            po += i - 1;
            pa += i - 1;
            pb += i - 1;
            continue;
        }
        // find the next base line both sides kept
        let mut next = po + 1;
        while next <= o.len() && !(ma[next].is_some() && mb[next].is_some()) {
            next += 1;
        }
        if next > o.len() {
            write(&mut chunks, &o[po..], &a[pa..], &b[pb..]);
            break;
        }
        let (na, nb) = (ma[next].unwrap(), mb[next].unwrap());
        write(&mut chunks, &o[po..next - 1], &a[pa..na - 1], &b[pb..nb - 1]);
        po = next - 1;
        pa = na - 1;
        pb = nb - 1;
    }

    let mut content = Vec::new();
    let mut conflicts = 0;
    let push_lines = |content: &mut Vec<u8>, lines: &[&[u8]], terminate: bool| {
        for l in lines {
            content.extend_from_slice(l);
        }
        if terminate && !content.is_empty() && !content.ends_with(b"\n") {
            content.push(b'\n');
        }
    };
    for chunk in chunks {
        match chunk {
            Chunk::Clean(lines) => push_lines(&mut content, &lines, false),
            Chunk::Conflict(ours, theirs) => {
                conflicts += 1;
                if !content.is_empty() && !content.ends_with(b"\n") {
                    content.push(b'\n');
                }
                content.extend_from_slice(format!("<<<<<<< {ours_label}\n").as_bytes());
                push_lines(&mut content, &ours, true);
                content.extend_from_slice(b"=======\n");
                push_lines(&mut content, &theirs, true);
                content.extend_from_slice(format!(">>>>>>> {theirs_label}\n").as_bytes());
            }
        }
    }
    LineMerge { content, conflicts }
}

// ---------------------------------------------------------------------------
// Tree merge
// ---------------------------------------------------------------------------

pub type Side = Option<(u32, Oid)>;

pub struct Conflict {
    pub path: String,
    pub base: Side,
    pub ours: Side,
    pub theirs: Side,
    /// What to leave in the work tree: (mode, contents).
    pub worktree: Option<(u32, Vec<u8>)>,
}

pub struct TreeMerge {
    /// Cleanly merged files.
    pub clean: FlatTree,
    pub conflicts: Vec<Conflict>,
    /// Human-readable progress ("Auto-merging x", "CONFLICT (content): ...").
    pub messages: Vec<String>,
}

pub fn merge_trees(
    repo: &Repo,
    base: &FlatTree,
    ours: &FlatTree,
    theirs: &FlatTree,
    ours_label: &str,
    theirs_label: &str,
) -> Result<TreeMerge> {
    let mut paths: BTreeSet<&String> = base.keys().collect();
    paths.extend(ours.keys());
    paths.extend(theirs.keys());
    let mut out = TreeMerge { clean: FlatTree::new(), conflicts: Vec::new(), messages: Vec::new() };
    let regular = |s: &Side| s.is_some_and(|(m, _)| m == mode::FILE || m == mode::EXEC);

    for path in paths {
        let (b, o, t) = (base.get(path).copied(), ours.get(path).copied(), theirs.get(path).copied());
        let resolved = if o == t {
            Some(o)
        } else if b == o {
            Some(t)
        } else if b == t {
            Some(o)
        } else {
            None
        };
        if let Some(side) = resolved {
            if let Some(v) = side {
                out.clean.insert(path.clone(), v);
            }
            continue;
        }
        // Both sides changed this path, differently.
        if regular(&o) && regular(&t) && (b.is_none() || regular(&b)) {
            let read = |s: Side| -> Result<Vec<u8>> {
                Ok(match s {
                    Some((_, oid)) => repo.odb.read_kind(&oid, Kind::Blob)?,
                    None => Vec::new(),
                })
            };
            let (om, tm) = (o.unwrap().0, t.unwrap().0);
            let merged_mode = if om == tm || b.is_some_and(|(bm, _)| bm == tm) { om } else { tm };
            out.messages.push(format!("Auto-merging {path}"));
            let m = merge_lines(&read(b)?, &read(o)?, &read(t)?, ours_label, theirs_label);
            if m.conflicts == 0 {
                let oid = repo.odb.write(Kind::Blob, &m.content)?;
                out.clean.insert(path.clone(), (merged_mode, oid));
            } else {
                let what = if b.is_none() { "add/add" } else { "content" };
                out.messages.push(format!("CONFLICT ({what}): Merge conflict in {path}"));
                out.conflicts.push(Conflict { path: path.clone(), base: b, ours: o, theirs: t, worktree: Some((merged_mode, m.content)) });
            }
            continue;
        }
        let keep = match (o, t) {
            (Some(v), None) => {
                out.messages.push(format!(
                    "CONFLICT (modify/delete): {path} deleted in {theirs_label} and modified in {ours_label}. \
                     Version {ours_label} of {path} left in tree."
                ));
                Some(v)
            }
            (None, Some(v)) => {
                out.messages.push(format!(
                    "CONFLICT (modify/delete): {path} deleted in {ours_label} and modified in {theirs_label}. \
                     Version {theirs_label} of {path} left in tree."
                ));
                Some(v)
            }
            _ => {
                out.messages.push(format!("CONFLICT (type change): {path} changed type on both sides; keeping {ours_label}'s version"));
                o
            }
        };
        let worktree = match keep {
            Some((m, oid)) => Some((m, repo.odb.read_kind(&oid, Kind::Blob)?)),
            None => None,
        };
        out.conflicts.push(Conflict { path: path.clone(), base: b, ours: o, theirs: t, worktree });
    }
    Ok(out)
}

/// Commits reachable from `start`, newest first (by committer date), like `git log`.
pub fn walk_history(repo: &Repo, starts: &[Oid], mut visit: impl FnMut(Oid, &crate::object::Commit) -> Result<bool>) -> Result<()> {
    let mut seen: HashSet<Oid> = HashSet::new();
    // (time, -insertion counter) so equal times come out in insertion order, as in Git
    let mut heap: BinaryHeap<(i64, i64, Oid)> = BinaryHeap::new();
    let mut counter = 0i64;
    let mut cache: HashMap<Oid, crate::object::Commit> = HashMap::new();
    for &s in starts {
        if seen.insert(s) {
            let c = repo.odb.read_commit(&s)?;
            heap.push((c.committer.time, -counter, s));
            counter += 1;
            cache.insert(s, c);
        }
    }
    while let Some((_, _, oid)) = heap.pop() {
        let commit = cache.remove(&oid).unwrap();
        if !visit(oid, &commit)? {
            break;
        }
        for p in &commit.parents {
            if seen.insert(*p) {
                let pc = repo.odb.read_commit(p)?;
                heap.push((pc.committer.time, -counter, *p));
                counter += 1;
                cache.insert(*p, pc);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(base: &str, ours: &str, theirs: &str) -> (String, usize) {
        let r = merge_lines(base.as_bytes(), ours.as_bytes(), theirs.as_bytes(), "ours", "theirs");
        (String::from_utf8(r.content).unwrap(), r.conflicts)
    }

    #[test]
    fn non_overlapping_edits_merge_cleanly() {
        let base = "a\nb\nc\nd\ne\n";
        assert_eq!(m(base, "A\nb\nc\nd\ne\n", "a\nb\nc\nd\nE\n"), ("A\nb\nc\nd\nE\n".into(), 0));
        assert_eq!(m(base, "a\nb\nc\nd\ne\nf\n", "z\na\nb\nc\nd\ne\n"), ("z\na\nb\nc\nd\ne\nf\n".into(), 0));
        assert_eq!(m(base, "a\nc\nd\ne\n", "a\nb\nc\nd\ne\n"), ("a\nc\nd\ne\n".into(), 0));
    }

    #[test]
    fn same_change_on_both_sides_is_clean() {
        assert_eq!(m("a\nb\nc\n", "a\nX\nc\n", "a\nX\nc\n"), ("a\nX\nc\n".into(), 0));
    }

    #[test]
    fn overlapping_edits_conflict() {
        let (out, n) = m("a\nb\nc\n", "a\nours\nc\n", "a\ntheirs\nc\n");
        assert_eq!(n, 1);
        assert_eq!(out, "a\n<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\nc\n");
    }

    #[test]
    fn add_add_with_missing_newline() {
        let (out, n) = m("", "x", "y");
        assert_eq!(n, 1);
        assert_eq!(out, "<<<<<<< ours\nx\n=======\ny\n>>>>>>> theirs\n");
    }

    #[test]
    fn one_side_deletes_everything() {
        assert_eq!(m("a\nb\n", "", "a\nb\n"), ("".into(), 0));
    }
}
