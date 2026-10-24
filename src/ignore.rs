//! `.gitignore` rules and Git's `wildmatch` glob matcher.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
struct Pattern {
    glob: String,
    negated: bool,
    dir_only: bool,
    /// Pattern contains a slash, so it matches the path relative to its base, not just the basename.
    anchored: bool,
    /// Directory (relative to the work tree, "" for the root) of the file that defined it.
    base: String,
}

impl Pattern {
    fn parse(line: &str, base: &str) -> Option<Pattern> {
        let mut line = line.trim_end_matches(['\n', '\r']);
        // trailing spaces are ignored unless escaped
        while line.ends_with(' ') && !line.ends_with("\\ ") {
            line = &line[..line.len() - 1];
        }
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (negated, rest) = match line.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        // "\!" and "\#" escape a literal leading '!' or '#'
        let mut glob = if rest.starts_with("\\!") || rest.starts_with("\\#") { &rest[1..] } else { rest }.to_string();
        let dir_only = glob.ends_with('/');
        if dir_only {
            glob.pop();
        }
        let anchored = glob.contains('/');
        let glob = glob.strip_prefix('/').map(String::from).unwrap_or(glob);
        if glob.is_empty() {
            return None;
        }
        Some(Pattern { glob, negated, dir_only, anchored, base: base.to_string() })
    }

    fn matches(&self, path: &str, is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let rel = if self.base.is_empty() {
            path
        } else {
            match path.strip_prefix(&self.base).and_then(|r| r.strip_prefix('/')) {
                Some(r) => r,
                None => return false,
            }
        };
        if self.anchored {
            wildmatch(self.glob.as_bytes(), rel.as_bytes(), true)
        } else {
            let name = rel.rsplit('/').next().unwrap_or(rel);
            wildmatch(self.glob.as_bytes(), name.as_bytes(), true)
        }
    }
}

/// Ignore rules for a work tree: info/exclude, then each directory's .gitignore.
pub struct Ignore {
    work_tree: PathBuf,
    global: Vec<Pattern>,
    per_dir: HashMap<String, Vec<Pattern>>,
}

impl Ignore {
    pub fn new(work_tree: &Path, git_dir: &Path) -> Ignore {
        let mut global = Vec::new();
        if let Ok(text) = fs::read_to_string(git_dir.join("info/exclude")) {
            global.extend(text.lines().filter_map(|l| Pattern::parse(l, "")));
        }
        Ignore { work_tree: work_tree.to_path_buf(), global, per_dir: HashMap::new() }
    }

    fn dir_patterns(&mut self, dir: &str) -> &[Pattern] {
        if !self.per_dir.contains_key(dir) {
            let file = if dir.is_empty() { self.work_tree.join(".gitignore") } else { self.work_tree.join(dir).join(".gitignore") };
            let pats = fs::read_to_string(file).map(|t| t.lines().filter_map(|l| Pattern::parse(l, dir)).collect()).unwrap_or_default();
            self.per_dir.insert(dir.to_string(), pats);
        }
        &self.per_dir[dir]
    }

    /// Is `path` (relative to the work tree, '/'-separated) ignored? Callers walk top-down and
    /// don't descend into ignored directories, which gives Git's "can't re-include a file
    /// inside an excluded directory" rule.
    pub fn is_ignored(&mut self, path: &str, is_dir: bool) -> bool {
        let mut result = None;
        for p in &self.global {
            if p.matches(path, is_dir) {
                result = Some(!p.negated);
            }
        }
        // root .gitignore first, deeper files later so they take precedence
        let mut dirs = vec![String::new()];
        let mut acc = String::new();
        let parts: Vec<&str> = path.split('/').collect();
        for part in &parts[..parts.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            dirs.push(acc.clone());
        }
        for d in dirs {
            for p in self.dir_patterns(&d).to_vec() {
                if p.matches(path, is_dir) {
                    result = Some(!p.negated);
                }
            }
        }
        result.unwrap_or(false)
    }
}

/// Glob match in the style of Git's wildmatch. With `pathname`, `*` and `?` don't match '/',
/// while `**` (as a whole path component) matches any number of directories.
pub fn wildmatch(pat: &[u8], text: &[u8], pathname: bool) -> bool {
    matches!(dowild(pat, text, pathname), Match::Yes)
}

#[derive(PartialEq)]
enum Match {
    Yes,
    No,
    /// No match, and no later position for an enclosing `*` can help either.
    AbortAll,
    /// No match; an enclosing `*` may only retry while staying within the current component.
    AbortToStarStar,
}

fn dowild(p: &[u8], t: &[u8], pathname: bool) -> Match {
    let (mut pi, mut ti) = (0, 0);
    while pi < p.len() {
        let pc = p[pi];
        if ti >= t.len() && pc != b'*' {
            return Match::AbortAll;
        }
        match pc {
            b'\\' => {
                pi += 1;
                if pi >= p.len() || p[pi] != t[ti] {
                    return Match::No;
                }
            }
            b'?' => {
                if pathname && t[ti] == b'/' {
                    return Match::No;
                }
            }
            b'*' => {
                pi += 1;
                let match_slash;
                if pi < p.len() && p[pi] == b'*' {
                    let prev_ok = pi < 2 || p[pi - 2] == b'/';
                    while pi < p.len() && p[pi] == b'*' {
                        pi += 1;
                    }
                    if !pathname {
                        match_slash = true;
                    } else if prev_ok && (pi >= p.len() || p[pi] == b'/') {
                        // "**/" may match zero directories
                        if pi < p.len() && p[pi] == b'/' && dowild(&p[pi + 1..], &t[ti..], pathname) == Match::Yes {
                            return Match::Yes;
                        }
                        match_slash = true;
                    } else {
                        match_slash = false; // "**" not a whole component acts like "*"
                    }
                } else {
                    match_slash = !pathname;
                }
                if pi >= p.len() {
                    // trailing star: matches the rest unless it would cross a '/'
                    if !match_slash && t[ti..].contains(&b'/') {
                        return Match::AbortToStarStar;
                    }
                    return Match::Yes;
                }
                loop {
                    if ti > t.len() {
                        return Match::AbortAll;
                    }
                    let r = dowild(&p[pi..], &t[ti..], pathname);
                    if r != Match::No && (!match_slash || r != Match::AbortToStarStar) {
                        return r;
                    }
                    if ti >= t.len() || (!match_slash && t[ti] == b'/') {
                        return Match::AbortToStarStar;
                    }
                    ti += 1;
                }
            }
            b'[' => {
                pi += 1;
                let negate = pi < p.len() && (p[pi] == b'!' || p[pi] == b'^');
                if negate {
                    pi += 1;
                }
                let c = t[ti];
                let mut matched = false;
                let mut first = true;
                loop {
                    if pi >= p.len() {
                        return Match::AbortAll; // unterminated class
                    }
                    let mut pc = p[pi];
                    if pc == b']' && !first {
                        break;
                    }
                    first = false;
                    if pc == b'\\' {
                        pi += 1;
                        if pi >= p.len() {
                            return Match::AbortAll;
                        }
                        pc = p[pi];
                        if pc == c {
                            matched = true;
                        }
                    } else if pc == b'[' && p.get(pi + 1) == Some(&b':') {
                        let Some(end) = p[pi + 2..].windows(2).position(|w| w == b":]") else { return Match::AbortAll };
                        let class = &p[pi + 2..pi + 2 + end];
                        if char_class(class, c) {
                            matched = true;
                        }
                        pi += 2 + end + 1;
                    } else if p.get(pi + 1) == Some(&b'-') && p.get(pi + 2).is_some_and(|&x| x != b']') {
                        let hi = p[pi + 2];
                        if pc <= c && c <= hi {
                            matched = true;
                        }
                        pi += 2;
                    } else if pc == c {
                        matched = true;
                    }
                    pi += 1;
                }
                if matched == negate || (pathname && c == b'/') {
                    return Match::No;
                }
            }
            _ => {
                if pc != t[ti] {
                    return Match::No;
                }
            }
        }
        pi += 1;
        ti += 1;
    }
    if ti == t.len() { Match::Yes } else { Match::No }
}

fn char_class(class: &[u8], c: u8) -> bool {
    match class {
        b"alnum" => c.is_ascii_alphanumeric(),
        b"alpha" => c.is_ascii_alphabetic(),
        b"blank" => c == b' ' || c == b'\t',
        b"cntrl" => c.is_ascii_control(),
        b"digit" => c.is_ascii_digit(),
        b"graph" => c.is_ascii_graphic(),
        b"lower" => c.is_ascii_lowercase(),
        b"print" => c.is_ascii_graphic() || c == b' ',
        b"punct" => c.is_ascii_punctuation(),
        b"space" => c.is_ascii_whitespace(),
        b"upper" => c.is_ascii_uppercase(),
        b"xdigit" => c.is_ascii_hexdigit(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cases adapted from Git's t3070-wildmatch.sh (pathname mode).
    #[test]
    fn wildmatch_cases() {
        let cases: &[(bool, &str, &str)] = &[
            (true, "foo", "foo"),
            (false, "bar", "foo"),
            (true, "", ""),
            (true, "???", "foo"),
            (false, "??", "foo"),
            (true, "*", "foo"),
            (true, "f*", "foo"),
            (false, "*f", "foo"),
            (true, "*foo*", "foo"),
            (true, "*ob*a*r*", "foobar"),
            (true, "*ab", "aaaaaaabababab"),
            (true, "foo\\*", "foo*"),
            (false, "foo\\*bar", "foobar"),
            (true, "f\\\\oo", "f\\oo"),
            (true, "*[al]?", "ball"),
            (false, "[ten]", "ten"),
            (true, "**[!te]", "ten"),
            (false, "**[!ten]", "ten"),
            (true, "t[a-g]n", "ten"),
            (false, "t[!a-g]n", "ten"),
            (true, "t[!a-g]n", "ton"),
            (true, "t[^a-g]n", "ton"),
            (true, "a[]]b", "a]b"),
            (true, "a[]-]b", "a-b"),
            (true, "a[]a-]b", "aab"),
            (true, "]", "]"),
            (false, "foo*bar", "foo/baz/bar"),
            (false, "foo**bar", "foo/baz/bar"),
            (true, "foo**bar", "foobazbar"),
            (true, "foo/**/bar", "foo/baz/bar"),
            (true, "foo/**/**/bar", "foo/baz/bar"),
            (true, "foo/**/bar", "foo/b/a/z/bar"),
            (true, "foo/**/bar", "foo/bar"),
            (false, "foo?bar", "foo/bar"),
            (false, "foo[/]bar", "foo/bar"),
            (false, "f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo/bar"),
            (true, "f[^eiu][^eiu][^eiu][^eiu][^eiu]r", "foo-bar"),
            (true, "**/foo", "foo"),
            (true, "**/foo", "XXX/foo"),
            (true, "**/foo", "bar/baz/foo"),
            (false, "*/foo", "bar/baz/foo"),
            (false, "**/bar*", "foo/bar/baz"),
            (true, "**/bar/*", "deep/foo/bar/baz"),
            (false, "**/bar/*", "deep/foo/bar/baz/"),
            (true, "**/bar/**", "deep/foo/bar/baz/"),
            (false, "**/bar/*", "deep/foo/bar"),
            (true, "**/bar/**", "deep/foo/bar/"),
            (true, "*/bar/**", "foo/bar/baz/x"),
            (false, "*/bar/**", "deep/foo/bar/baz/x"),
            (true, "**/bar/*/*", "deep/foo/bar/baz/x"),
            (false, "a[c-c]st", "acrt"),
            (true, "a[c-c]rt", "acrt"),
            (false, "[!]-]", "]"),
            (true, "[!]-]", "a"),
            (true, "[[:alpha:]][[:digit:]][[:upper:]]", "a1B"),
            (false, "[[:digit:][:upper:][:space:]]", "a"),
            (true, "[[:digit:][:upper:][:space:]]", "A"),
            (true, "[[:digit:][:upper:][:space:]]", " "),
            (true, "[a-c[:digit:]x-z]", "5"),
            (true, "[a-c[:digit:]x-z]", "y"),
            (false, "[a-c[:digit:]x-z]", "q"),
            (true, "-*-*-*-*-*-*-12-*-*-*-m-*-*-*", "-adobe-courier-bold-o-normal--12-120-75-75-m-70-iso8859-1"),
            (false, "-*-*-*-*-*-*-12-*-*-*-m-*-*-*", "-adobe-courier-bold-o-normal--12-120-75-75-X-70-iso8859-1"),
            (true, "XXX/*/*/*/*/*/*/12/*/*/*/m/*/*/*", "XXX/adobe/courier/bold/o/normal//12/120/75/75/m/70/iso8859/1"),
            (true, "**/*a*b*g*n*t", "abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txt"),
            (false, "**/*a*b*g*n*t", "abcd/abcdefg/abcdefghijk/abcdefghijklmnop.txtz"),
            (true, "*.c", "main.c"),
            (false, "*.c", "src/main.c"),
        ];
        for &(want, pat, text) in cases {
            assert_eq!(wildmatch(pat.as_bytes(), text.as_bytes(), true), want, "pattern {pat:?} on {text:?}");
        }
    }

    #[test]
    fn gitignore_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::create_dir_all(root.join("sub/deeper")).unwrap();
        fs::write(root.join(".git/info/exclude"), "*.swp\n").unwrap();
        fs::write(root.join(".gitignore"), "# comment\n*.log\n!keep.log\n/build\ntmp/\ndocs/*.html\n").unwrap();
        fs::write(root.join("sub/.gitignore"), "*.txt\n!important.txt\n/local.cfg\n").unwrap();
        let mut ig = Ignore::new(root, &root.join(".git"));
        let check = |ig: &mut Ignore, p: &str, d: bool| ig.is_ignored(p, d);
        assert!(check(&mut ig, "a.log", false));
        assert!(check(&mut ig, "sub/deeper/b.log", false));
        assert!(!check(&mut ig, "keep.log", false));
        assert!(check(&mut ig, "build", true));
        assert!(!check(&mut ig, "sub/build", true)); // anchored to the root
        assert!(check(&mut ig, "tmp", true));
        assert!(!check(&mut ig, "tmp", false)); // dir-only pattern
        assert!(check(&mut ig, "docs/x.html", false));
        assert!(!check(&mut ig, "docs/api/x.html", false));
        assert!(check(&mut ig, "sub/notes.txt", false));
        assert!(check(&mut ig, "sub/deeper/notes.txt", false));
        assert!(!check(&mut ig, "sub/important.txt", false));
        assert!(!check(&mut ig, "notes.txt", false)); // sub/.gitignore doesn't apply at root
        assert!(check(&mut ig, "sub/local.cfg", false));
        assert!(!check(&mut ig, "sub/deeper/local.cfg", false));
        assert!(check(&mut ig, "x.swp", false));
    }
}
