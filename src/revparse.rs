//! Revision syntax: names, abbreviated ids, `~n`, `^n`, `^{type}` and `rev:path`.

use anyhow::{Result, anyhow, bail};

use crate::object::Kind;
use crate::oid::{Oid, is_hex};
use crate::repo::Repo;

/// The order Git tries when a short name could be a branch, tag, etc. (gitrevisions(7)).
pub fn expand_ref(repo: &Repo, name: &str) -> Result<Option<(String, Oid)>> {
    let candidates = [
        name.to_string(),
        format!("refs/{name}"),
        format!("refs/tags/{name}"),
        format!("refs/heads/{name}"),
        format!("refs/remotes/{name}"),
        format!("refs/remotes/{name}/HEAD"),
    ];
    for c in candidates {
        if c != "HEAD" && !c.starts_with("refs/") {
            continue; // only HEAD-like names live outside refs/
        }
        if let Some(oid) = repo.refs.resolve(&c)? {
            return Ok(Some((c, oid)));
        }
    }
    if ["ORIG_HEAD", "MERGE_HEAD", "FETCH_HEAD"].contains(&name)
        && let Some(oid) = repo.refs.resolve(name)?
    {
        return Ok(Some((name.to_string(), oid)));
    }
    Ok(None)
}

/// Resolve a revision to an object id.
pub fn resolve(repo: &Repo, spec: &str) -> Result<Oid> {
    if let Some((rev, path)) = spec.split_once(':') {
        let tree = peel(repo, resolve(repo, if rev.is_empty() { "HEAD" } else { rev })?, Kind::Tree)?;
        return lookup_path(repo, tree, path).ok_or_else(|| anyhow!("path '{path}' does not exist in '{rev}'"));
    }
    // split "base" from trailing ~n / ^n / ^{type} operators
    let ops_start = spec.find(['~', '^']).unwrap_or(spec.len());
    let base = &spec[..ops_start];
    let mut oid = resolve_base(repo, base)?;
    let ops = &spec.as_bytes()[ops_start..];
    let mut i = 0;
    while i < ops.len() {
        let op = ops[i];
        i += 1;
        if op == b'^' && ops.get(i) == Some(&b'{') {
            let end = spec[ops_start + i..].find('}').ok_or_else(|| anyhow!("bad revision {spec}"))?;
            let ty = &spec[ops_start + i + 1..ops_start + i + end];
            i += end + 1;
            oid = match ty {
                "" => peel_tags(repo, oid)?,
                t => peel(repo, oid, Kind::parse(t)?)?,
            };
            continue;
        }
        let digits_start = i;
        while i < ops.len() && ops[i].is_ascii_digit() {
            i += 1;
        }
        let n: usize = if i > digits_start { spec[ops_start + digits_start..ops_start + i].parse()? } else { 1 };
        match op {
            b'~' => {
                for _ in 0..n {
                    let c = repo.odb.read_commit(&peel(repo, oid, Kind::Commit)?)?;
                    oid = *c.parents.first().ok_or_else(|| anyhow!("{spec}: commit has no parent"))?;
                }
            }
            b'^' => {
                let commit = peel(repo, oid, Kind::Commit)?;
                if n == 0 {
                    oid = commit;
                } else {
                    let c = repo.odb.read_commit(&commit)?;
                    oid = *c.parents.get(n - 1).ok_or_else(|| anyhow!("{spec}: no parent number {n}"))?;
                }
            }
            _ => bail!("bad revision {spec}"),
        }
    }
    Ok(oid)
}

fn resolve_base(repo: &Repo, base: &str) -> Result<Oid> {
    let base = if base.is_empty() || base == "@" { "HEAD" } else { base };
    if let Some((_, oid)) = expand_ref(repo, base)? {
        return Ok(oid);
    }
    if is_hex(base) && base.len() >= 4 && base.len() <= 40 {
        if base.len() == 40 {
            let oid = Oid::from_hex(base)?;
            if repo.odb.contains(&oid) {
                return Ok(oid);
            }
        }
        let found = repo.odb.find_prefix(base)?;
        match found.len() {
            0 => {}
            1 => return Ok(found[0]),
            _ => bail!("short object ID {base} is ambiguous"),
        }
    }
    if base == "HEAD" {
        bail!("ambiguous argument 'HEAD': unknown revision (the current branch has no commits yet)");
    }
    bail!("ambiguous argument '{base}': unknown revision or path not in the working tree")
}

/// Follow annotated tags to the object they point at.
pub fn peel_tags(repo: &Repo, mut oid: Oid) -> Result<Oid> {
    for _ in 0..32 {
        let (kind, data) = repo.odb.read(&oid)?;
        if kind != Kind::Tag {
            return Ok(oid);
        }
        oid = crate::object::Tag::parse(&data)?.object;
    }
    bail!("tag chain too long")
}

/// Peel tags (and commits, to reach a tree) until we get an object of type `want`.
pub fn peel(repo: &Repo, oid: Oid, want: Kind) -> Result<Oid> {
    let oid = peel_tags(repo, oid)?;
    let (kind, data) = repo.odb.read(&oid)?;
    if kind == want {
        return Ok(oid);
    }
    if kind == Kind::Commit && want == Kind::Tree {
        return Ok(crate::object::Commit::parse(&data)?.tree);
    }
    bail!("{} is a {kind}, not a {want}", oid.short())
}

fn lookup_path(repo: &Repo, tree: Oid, path: &str) -> Option<Oid> {
    let mut cur = tree;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        let entries = repo.odb.read_tree(&cur).ok()?;
        cur = entries.into_iter().find(|e| e.name == part)?.oid;
    }
    Some(cur)
}

/// Resolve a revision that must name a commit.
pub fn commit(repo: &Repo, spec: &str) -> Result<Oid> {
    peel(repo, resolve(repo, spec)?, Kind::Commit)
}
