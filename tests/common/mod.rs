//! Test harness: run twig and real git side by side in throwaway repositories.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub struct Sandbox {
    pub root: tempfile::TempDir,
    home: tempfile::TempDir,
}

pub const AUTHOR_DATE: &str = "1700000000 +0530";

impl Sandbox {
    pub fn new() -> Sandbox {
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join("gitconfig"),
            "[user]\n\tname = Test User\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n\
             [core]\n\tautocrlf = false\n[advice]\n\tdetachedHead = false\n[merge]\n\tconflictStyle = merge\n",
        )
        .unwrap();
        Sandbox { root: tempfile::tempdir().unwrap(), home }
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn command(&self, program: &str, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(program);
        c.args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("HOME", self.home.path())
            .env("GIT_CONFIG_GLOBAL", self.home.path().join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "A U Thor")
            .env("GIT_AUTHOR_EMAIL", "author@example.com")
            .env("GIT_AUTHOR_DATE", AUTHOR_DATE)
            .env("GIT_COMMITTER_NAME", "C O Mitter")
            .env("GIT_COMMITTER_EMAIL", "committer@example.com")
            .env("GIT_COMMITTER_DATE", AUTHOR_DATE)
            .env("TZ", "UTC")
            .env("LC_ALL", "C");
        c
    }

    fn run(&self, program: &str, dir: &str, args: &[&str], expect_ok: bool) -> Output {
        let out = self.command(program, &self.path(dir), args).output().unwrap();
        if expect_ok && !out.status.success() {
            panic!(
                "`{program} {}` in {dir} failed ({}):\nstdout:\n{}\nstderr:\n{}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        out
    }

    /// Run twig in `dir` (relative to the sandbox root) and return stdout; panics on failure.
    pub fn twig(&self, dir: &str, args: &[&str]) -> String {
        String::from_utf8(self.run(env!("CARGO_BIN_EXE_twig"), dir, args, true).stdout).unwrap()
    }

    /// Run twig expecting failure; returns (exit code, stdout + stderr).
    pub fn twig_fail(&self, dir: &str, args: &[&str]) -> (i32, String) {
        let out = self.run(env!("CARGO_BIN_EXE_twig"), dir, args, false);
        assert!(!out.status.success(), "`twig {}` unexpectedly succeeded", args.join(" "));
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.code().unwrap_or(-1), text)
    }

    pub fn git(&self, dir: &str, args: &[&str]) -> String {
        String::from_utf8(self.run("git", dir, args, true).stdout).unwrap()
    }

    pub fn git_bytes(&self, dir: &str, args: &[&str]) -> Vec<u8> {
        self.run("git", dir, args, true).stdout
    }

    pub fn twig_bytes(&self, dir: &str, args: &[&str]) -> Vec<u8> {
        self.run(env!("CARGO_BIN_EXE_twig"), dir, args, true).stdout
    }

    /// Run git with stdin.
    pub fn git_stdin(&self, dir: &str, args: &[&str], input: &[u8]) -> Output {
        use std::io::Write;
        let mut child = self
            .command("git", &self.path(dir), args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    pub fn write(&self, rel: &str, content: &str) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap()
    }

    pub fn mkdir(&self, rel: &str) {
        fs::create_dir_all(self.path(rel)).unwrap();
    }

    pub fn copy_dir(&self, from: &str, to: &str) {
        let status = Command::new("cp").arg("-R").arg(self.path(from)).arg(self.path(to)).status().unwrap();
        assert!(status.success());
    }

    pub fn chmod_x(&self, rel: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(self.path(rel), fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A tiny deterministic PRNG so randomized tests are reproducible.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// Some lines of text, reusing a small vocabulary so diffs have common lines.
    pub fn text(&mut self, max_lines: u64) -> String {
        let words = ["alpha", "beta", "gamma", "delta", "fn main() {", "}", "", "return x;", "let y = 2;", "// note"];
        let n = self.below(max_lines + 1);
        let mut s = String::new();
        for _ in 0..n {
            s.push_str(self.pick(&words));
            s.push('\n');
        }
        if self.below(5) == 0 && !s.is_empty() {
            s.pop(); // sometimes no trailing newline
        }
        s
    }
}
