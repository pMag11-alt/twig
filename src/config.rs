//! Git config files: `[section "subsection"]` headers and `key = value` lines.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

#[derive(Clone, Debug, Default)]
pub struct Config {
    /// (canonical "section.subsection.key", value) in file order; later entries win.
    entries: Vec<(String, String)>,
}

impl Config {
    /// Global config (~/.gitconfig, $XDG_CONFIG_HOME/git/config) followed by the repository's.
    pub fn load(git_dir: Option<&Path>) -> Config {
        let mut cfg = Config::default();
        for path in global_paths() {
            if let Ok(text) = fs::read_to_string(&path) {
                cfg.entries.extend(parse(&text));
            }
        }
        if let Some(dir) = git_dir
            && let Ok(text) = fs::read_to_string(dir.join("config"))
        {
            cfg.entries.extend(parse(&text));
        }
        cfg
    }

    pub fn parse_str(text: &str) -> Config {
        Config { entries: parse(text) }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        let key = canonical(key);
        self.entries.iter().rev().find(|(k, _)| *k == key).map(|(_, v)| v.as_str())
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key).map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "on" | "1" | ""))
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }
}

fn global_paths() -> Vec<PathBuf> {
    if let Ok(p) = std::env::var("GIT_CONFIG_GLOBAL") {
        return vec![PathBuf::from(p)];
    }
    let mut out = Vec::new();
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    if let Some(x) = xdg {
        out.push(x.join("git/config"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        out.push(PathBuf::from(home).join(".gitconfig"));
    }
    out
}

/// Section and key names are case-insensitive; subsection names are not.
fn canonical(key: &str) -> String {
    match (key.find('.'), key.rfind('.')) {
        (Some(first), Some(last)) if first != last => {
            format!("{}.{}.{}", key[..first].to_ascii_lowercase(), &key[first + 1..last], key[last + 1..].to_ascii_lowercase())
        }
        _ => key.to_ascii_lowercase(),
    }
}

fn parse(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            let Some(end) = line.find(']') else { continue };
            let inner = &line[1..end];
            section = match inner.split_once(' ') {
                Some((name, sub)) => format!("{}.{}", name.to_ascii_lowercase(), sub.trim().trim_matches('"')),
                None => inner.to_ascii_lowercase(),
            };
            let rest = line[end + 1..].trim();
            if rest.is_empty() || rest.starts_with('#') || rest.starts_with(';') {
                continue;
            }
            // `[core] bare = false` on one line
            push_kv(&mut out, &section, rest);
            continue;
        }
        push_kv(&mut out, &section, line);
    }
    out
}

fn push_kv(out: &mut Vec<(String, String)>, section: &str, line: &str) {
    let (key, value) = match line.split_once('=') {
        Some((k, v)) => (k.trim(), unquote(v.trim())),
        None => (line, String::new()), // bare key means "true"
    };
    out.push((format!("{section}.{}", key.to_ascii_lowercase()), value));
}

fn unquote(v: &str) -> String {
    let mut out = String::new();
    let mut in_quotes = false;
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            },
            '#' | ';' if !in_quotes => break,
            _ => out.push(c),
        }
    }
    out.trim_end().to_string()
}

fn quote(v: &str) -> String {
    let needs = v.starts_with(' ') || v.ends_with(' ') || v.contains(['#', ';', '"', '\\', '\n']);
    if !needs {
        return v.to_string();
    }
    let escaped = v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
    format!("\"{escaped}\"")
}

/// Set `key` (e.g. "remote.origin.url") in the config file at `path`, editing it in place.
pub fn set(path: &Path, key: &str, value: &str) -> Result<()> {
    let (section, sub, name) = split_key(key)?;
    let text = fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    let header_matches = |line: &str| -> bool {
        let t = line.trim();
        if !t.starts_with('[') {
            return false;
        }
        let Some(end) = t.find(']') else { return false };
        let inner = &t[1..end];
        match (inner.split_once(' '), &sub) {
            (Some((s, ss)), Some(want)) => s.eq_ignore_ascii_case(&section) && ss.trim().trim_matches('"') == want,
            (None, None) => inner.eq_ignore_ascii_case(&section),
            _ => false,
        }
    };
    let new_line = format!("\t{name} = {}", quote(value));
    let mut in_section = false;
    let mut last_in_section = None;
    let mut replaced = false;
    for (i, line) in lines.iter_mut().enumerate() {
        let t = line.trim().to_string();
        if t.starts_with('[') {
            in_section = header_matches(line);
            if in_section {
                last_in_section = Some(i);
            }
            continue;
        }
        if in_section {
            last_in_section = Some(i);
            let k = t.split('=').next().unwrap_or("").trim();
            if k.eq_ignore_ascii_case(&name) {
                *line = new_line.clone();
                replaced = true;
            }
        }
    }
    if !replaced {
        match last_in_section {
            Some(i) => lines.insert(i + 1, new_line),
            None => {
                lines.push(match &sub {
                    Some(s) => format!("[{section} \"{s}\"]"),
                    None => format!("[{section}]"),
                });
                lines.push(new_line);
            }
        }
    }
    fs::write(path, lines.join("\n") + "\n")?;
    Ok(())
}

fn split_key(key: &str) -> Result<(String, Option<String>, String)> {
    let first = key.find('.');
    let last = key.rfind('.');
    match (first, last) {
        (Some(f), Some(l)) if f == l => Ok((key[..f].to_string(), None, key[l + 1..].to_string())),
        (Some(f), Some(l)) => Ok((key[..f].to_string(), Some(key[f + 1..l].to_string()), key[l + 1..].to_string())),
        _ => bail!("key does not contain a section: {key}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_subsections_and_quotes() {
        let cfg = Config::parse_str(
            "[core]\n\tbare = false\n\tfilemode\n[User]\n  Name = \"A \\\"Q\\\" Person\" # comment\n\
             [remote \"Origin\"]\n\turl = https://x/y.git\n[user] name = Override\n",
        );
        assert_eq!(cfg.get("core.bare"), Some("false"));
        assert_eq!(cfg.get_bool("core.filemode"), Some(true));
        assert_eq!(cfg.get("user.name"), Some("Override"));
        assert_eq!(cfg.get("USER.NAME"), Some("Override"));
        assert_eq!(cfg.get("remote.Origin.url"), Some("https://x/y.git"));
        assert_eq!(cfg.get("remote.origin.url"), None); // subsections are case-sensitive
    }

    #[test]
    fn set_edits_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config");
        fs::write(&p, "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = old\n").unwrap();
        set(&p, "remote.origin.url", "new").unwrap();
        set(&p, "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*").unwrap();
        set(&p, "user.name", "has # hash").unwrap();
        set(&p, "core.bare", "true").unwrap();
        let text = fs::read_to_string(&p).unwrap();
        let cfg = Config::parse_str(&text);
        assert_eq!(cfg.get("remote.origin.url"), Some("new"));
        assert_eq!(cfg.get("remote.origin.fetch"), Some("+refs/heads/*:refs/remotes/origin/*"));
        assert_eq!(cfg.get("user.name"), Some("has # hash"));
        assert_eq!(cfg.get("core.bare"), Some("true"));
        assert_eq!(text.matches("[remote").count(), 1);
    }
}
