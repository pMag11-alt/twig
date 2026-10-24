//! twig and real Git, side by side: every test checks twig's output or repository state
//! against what `git` itself says.

mod common;

use common::{Rng, Sandbox};

fn lines(s: &str) -> Vec<&str> {
    s.lines().collect()
}

/// Build the same small history in `dir` with either tool.
fn build_history(sb: &Sandbox, dir: &str, tool: &str) {
    let run = |args: &[&str]| if tool == "git" { sb.git(dir, args) } else { sb.twig(dir, args) };
    sb.mkdir(dir);
    run(&["init", "-q"]);
    sb.write(&format!("{dir}/README.md"), "# demo\n\nhello\n");
    sb.write(&format!("{dir}/src/main.rs"), "fn main() {\n    println!(\"hi\");\n}\n");
    sb.write(&format!("{dir}/src/util/mod.rs"), "pub fn f() -> u32 { 1 }\n");
    sb.write(&format!("{dir}/run.sh"), "#!/bin/sh\necho run\n");
    sb.chmod_x(&format!("{dir}/run.sh"));
    std::os::unix::fs::symlink("README.md", sb.path(&format!("{dir}/link"))).unwrap();
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "initial commit"]);
    sb.write(&format!("{dir}/src/main.rs"), "fn main() {\n    println!(\"hello\");\n}\n");
    std::fs::remove_file(sb.path(&format!("{dir}/src/util/mod.rs"))).unwrap();
    sb.write(&format!("{dir}/docs/guide.txt"), "step 1\nstep 2\n");
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "second commit\n\nwith a body"]);
}

#[test]
fn commits_are_byte_identical_to_gits() {
    let sb = Sandbox::new();
    build_history(&sb, "with_git", "git");
    build_history(&sb, "with_twig", "twig");
    let git_head = sb.git("with_git", &["rev-parse", "HEAD"]);
    let twig_head = sb.git("with_twig", &["rev-parse", "HEAD"]);
    assert_eq!(git_head, twig_head, "same content, same author/date => same commit id");
    assert_eq!(sb.twig("with_twig", &["rev-parse", "HEAD"]), git_head);
}

#[test]
fn git_accepts_everything_twig_writes() {
    let sb = Sandbox::new();
    build_history(&sb, "r", "twig");
    sb.twig("r", &["branch", "feature"]);
    sb.twig("r", &["tag", "v1"]);
    sb.twig("r", &["tag", "-a", "v2", "-m", "release two"]);
    sb.git("r", &["fsck", "--strict", "--no-dangling"]);
    assert_eq!(sb.git("r", &["status", "--porcelain"]), "");
    assert_eq!(sb.git("r", &["cat-file", "-t", "v2"]), "tag\n");
    assert_eq!(sb.git("r", &["rev-parse", "v2^{}"]), sb.git("r", &["rev-parse", "HEAD"]));
    let refs = sb.git("r", &["for-each-ref", "--format=%(refname)"]);
    assert_eq!(lines(&refs), ["refs/heads/feature", "refs/heads/main", "refs/tags/v1", "refs/tags/v2"]);
    // the reflog twig writes is readable by git
    assert_eq!(sb.git("r", &["reflog", "--format=%gs"]).lines().count(), 2);
}

#[test]
fn reads_packed_repositories_with_deltas() {
    let sb = Sandbox::new();
    sb.mkdir("p");
    sb.git("p", &["init", "-q"]);
    let mut big = String::new();
    for i in 0..400 {
        big.push_str(&format!("line {i}: the quick brown fox jumps over the lazy dog\n"));
    }
    for round in 0..12 {
        big = big.replacen(&format!("line {}:", round * 7), &format!("LINE {round}:"), 1);
        sb.write("p/big.txt", &big);
        sb.write(&format!("p/notes/{}.txt", round % 4), &format!("round {round}\n"));
        sb.git("p", &["add", "-A"]);
        sb.git("p", &["commit", "-q", "-m", &format!("round {round}")]);
        if round == 5 {
            sb.git("p", &["tag", "-a", "mid", "-m", "halfway"]);
        }
    }
    sb.git("p", &["gc", "-q", "--aggressive"]);
    assert!(!sb.git("p", &["count-objects", "-v"]).contains("in-pack: 0"));
    assert!(sb.git("p", &["verify-pack", "-v", &glob_one(&sb, "p/.git/objects/pack", ".idx")]).contains("chain length"));

    assert_eq!(sb.twig("p", &["log", "--format=%H %P %s"]), sb.git("p", &["log", "--format=%H %P %s"]));
    let objects = sb.git("p", &["rev-list", "--objects", "--all"]);
    for line in objects.lines() {
        let oid = line.split(' ').next().unwrap();
        assert_eq!(sb.twig("p", &["cat-file", "-t", oid]), sb.git("p", &["cat-file", "-t", oid]));
        assert_eq!(sb.twig("p", &["cat-file", "-s", oid]), sb.git("p", &["cat-file", "-s", oid]));
        assert_eq!(sb.twig_bytes("p", &["cat-file", "-p", oid]), sb.git_bytes("p", &["cat-file", "-p", oid]), "object {oid}");
    }
    assert_eq!(sb.twig("p", &["rev-parse", "mid^{}"]), sb.git("p", &["rev-parse", "mid^{}"]));
    assert_eq!(sb.twig("p", &["status", "--porcelain"]), "");
}

fn glob_one(sb: &Sandbox, dir: &str, ext: &str) -> String {
    let entry = std::fs::read_dir(sb.path(dir)).unwrap().flatten().find(|e| e.path().to_string_lossy().ends_with(ext)).unwrap();
    entry.path().to_string_lossy().into_owned()
}

#[test]
fn hash_object_and_rev_parse_agree_with_git() {
    let sb = Sandbox::new();
    build_history(&sb, "r", "git");
    sb.git("r", &["checkout", "-q", "-b", "side", "HEAD~1"]);
    sb.write("r/side.txt", "side\n");
    sb.git("r", &["add", "side.txt"]);
    sb.git("r", &["commit", "-q", "-m", "side"]);
    sb.git("r", &["checkout", "-q", "main"]);
    sb.git("r", &["merge", "-q", "--no-edit", "side"]);
    sb.git("r", &["tag", "-a", "v1", "-m", "v1", "HEAD~1"]);
    let short = sb.git("r", &["rev-parse", "--short=6", "HEAD~2"]);
    for spec in [
        "HEAD",
        "@",
        "main",
        "HEAD~1",
        "HEAD^2",
        "HEAD^1~1",
        "HEAD^{tree}",
        "v1",
        "v1^{}",
        "v1^{commit}",
        "HEAD:src/main.rs",
        "HEAD~1:src",
        short.trim(),
    ] {
        assert_eq!(sb.twig("r", &["rev-parse", spec]), sb.git("r", &["rev-parse", spec]), "rev-parse {spec}");
    }
    assert_eq!(sb.twig("r", &["rev-parse", "--abbrev-ref", "HEAD"]), "main\n");
    for f in ["README.md", "run.sh", "src/main.rs"] {
        assert_eq!(sb.twig("r", &["hash-object", f]), sb.git("r", &["hash-object", f]));
    }
    assert_eq!(sb.twig("r", &["ls-tree", "-r", "HEAD"]), sb.git("r", &["ls-tree", "-r", "HEAD"]));
    assert_eq!(sb.twig("r", &["ls-tree", "HEAD"]), sb.git("r", &["ls-tree", "HEAD"]));
    assert_eq!(sb.twig("r", &["ls-files", "-s"]), sb.git("r", &["ls-files", "-s"]));
    assert_eq!(sb.twig("r", &["merge-base", "main", "side"]), sb.git("r", &["merge-base", "main", "side"]));
    assert_eq!(sb.twig("r", &["write-tree"]), sb.git("r", &["write-tree"]));
}

#[test]
fn log_formats_match_git() {
    let sb = Sandbox::new();
    build_history(&sb, "r", "git");
    sb.git("r", &["checkout", "-q", "-b", "side", "HEAD~1"]);
    sb.write("r/x.txt", "x\n");
    sb.git("r", &["add", "x.txt"]);
    sb.git("r", &["commit", "-q", "-m", "on side"]);
    sb.git("r", &["checkout", "-q", "main"]);
    sb.git("r", &["merge", "-q", "--no-edit", "side"]);
    for fmt in ["%H %P", "%h %an <%ae> %ad", "%s%n%b", "%T %cn %ce %ct %at", "%ai | %ci"] {
        let f = format!("--format={fmt}");
        assert_eq!(sb.twig("r", &["log", &f]), sb.git("r", &["log", &f]), "format {fmt}");
    }
    assert_eq!(sb.twig("r", &["log", "--no-decorate"]), sb.git("r", &["log", "--no-decorate"]));
    assert_eq!(sb.twig("r", &["log", "--oneline", "-n", "2"]), sb.git("r", &["log", "--oneline", "--no-decorate", "-n", "2"]));
    assert_eq!(sb.twig("r", &["log", "-2", "--format=%s"]), sb.git("r", &["log", "-2", "--format=%s"]));
    assert_eq!(sb.twig("r", &["log", "--format=%s", "main~1..main"]), sb.git("r", &["log", "--format=%s", "main~1..main"]));
}

/// After every step of a random sequence of edits, `twig status --porcelain` must equal Git's.
#[test]
fn status_matches_git_through_random_edits() {
    let sb = Sandbox::new();
    sb.mkdir("r");
    sb.twig("r", &["init", "-q"]);
    sb.write("r/.gitignore", "*.log\nbuild/\n!keep.log\n");
    let paths = ["a.txt", "b.txt", "dir/c.txt", "dir/sub/d.txt", "e/f.txt", "x.log", "keep.log", "build/out.o", "new/deep/g.txt"];
    let mut rng = Rng::new(7);
    for step in 0..120 {
        let path = *rng.pick(&paths);
        let full = format!("r/{path}");
        match rng.below(9) {
            0..=2 => sb.write(&full, &rng.text(6)),
            3 => {
                let _ = std::fs::remove_file(sb.path(&full));
            }
            4 => {
                if sb.path(&full).exists() {
                    use std::os::unix::fs::PermissionsExt;
                    let mode = if rng.below(2) == 0 { 0o755 } else { 0o644 };
                    std::fs::set_permissions(sb.path(&full), std::fs::Permissions::from_mode(mode)).unwrap();
                }
            }
            5 => {
                let target = if rng.below(3) == 0 { "." } else { path };
                if sb.path(&full).exists() || target == "." {
                    sb.twig("r", &["add", target]);
                }
            }
            6 => {
                let tracked = sb.twig("r", &["ls-files"]);
                if let Some(p) = tracked.lines().nth(rng.below(10) as usize) {
                    sb.twig("r", &["rm", "-q", "--cached", p]);
                }
            }
            7 => {
                let status = sb.twig("r", &["status", "--porcelain"]);
                if status.lines().any(|l| !l.starts_with(' ') && !l.starts_with('?')) {
                    sb.twig("r", &["commit", "-q", "-m", &format!("step {step}")]);
                }
            }
            _ => {
                let status = sb.twig("r", &["status", "--porcelain"]);
                if status.lines().any(|l| l.starts_with(" M") || l.starts_with(" D"))
                    && sb.twig("r", &["log", "-1", "--format=%h"]).len() > 1
                {
                    sb.twig("r", &["commit", "-q", "-a", "-m", &format!("step {step} -a")]);
                }
            }
        }
        let ours = sb.twig("r", &["status", "--porcelain"]);
        let theirs = sb.git("r", &["-c", "status.renames=false", "status", "--porcelain"]);
        assert_eq!(ours, theirs, "status differs after step {step}");
    }
    sb.git("r", &["fsck", "--strict"]);
}

#[test]
fn twig_reads_an_index_git_wrote_and_vice_versa() {
    let sb = Sandbox::new();
    build_history(&sb, "r", "git");
    sb.write("r/README.md", "changed\n");
    sb.write("r/new.txt", "new\n");
    sb.git("r", &["add", "README.md", "new.txt"]);
    sb.git("r", &["rm", "-q", "--cached", "run.sh"]);
    sb.write("r/src/main.rs", "fn main() {}\n");
    assert_eq!(sb.twig("r", &["status", "--porcelain"]), sb.git("r", &["status", "--porcelain"]));
    sb.twig("r", &["add", "src/main.rs", "run.sh"]);
    assert_eq!(sb.git("r", &["diff", "--cached", "--name-status"]), "M\tREADME.md\nA\tnew.txt\nM\tsrc/main.rs\n");
    assert_eq!(sb.git("r", &["diff", "--name-only"]), "");
}

/// The patch twig prints must apply cleanly with `git apply` and reproduce the edits.
#[test]
fn diffs_apply_with_git_apply() {
    let sb = Sandbox::new();
    sb.mkdir("r");
    sb.twig("r", &["init", "-q"]);
    let mut rng = Rng::new(11);
    let files = ["one.txt", "two.txt", "dir/three.txt", "dir/four.txt", "five.txt"];
    for f in files {
        sb.write(&format!("r/{f}"), &rng.text(40));
    }
    sb.twig("r", &["add", "."]);
    sb.twig("r", &["commit", "-q", "-m", "base"]);
    for round in 0..6 {
        sb.copy_dir("r", &format!("before{round}"));
        sb.git(&format!("before{round}"), &["status", "--short"]); // refresh stat data invalidated by the copy
        for f in files {
            match rng.below(4) {
                0 => {}
                1 => {
                    let _ = std::fs::remove_file(sb.path(&format!("r/{f}")));
                }
                _ => sb.write(&format!("r/{f}"), &rng.text(40)),
            }
        }
        sb.write(&format!("r/added{round}.txt"), &rng.text(10));
        if round % 2 == 0 {
            sb.chmod_x("r/added0.txt");
        }
        // unstaged diff for tracked files, then everything via --cached
        let unstaged = sb.twig_bytes("r", &["diff"]);
        let check = sb.git_stdin(&format!("before{round}"), &["apply", "--check", "-"], &unstaged);
        assert!(check.status.success(), "round {round}: {}", String::from_utf8_lossy(&check.stderr));
        sb.twig("r", &["add", "-A"]);
        let patch = sb.twig_bytes("r", &["diff", "--cached"]);
        let applied = sb.git_stdin(&format!("before{round}"), &["apply", "--index", "-"], &patch);
        assert!(
            applied.status.success(),
            "round {round}: {}\n{}",
            String::from_utf8_lossy(&applied.stderr),
            String::from_utf8_lossy(&patch)
        );
        assert_eq!(sb.git(&format!("before{round}"), &["write-tree"]), sb.twig("r", &["write-tree"]), "round {round}");
        sb.twig("r", &["commit", "-q", "-m", &format!("round {round}")]);
        // commit-to-commit diffs apply too
        let patch = sb.twig_bytes("r", &["diff", "HEAD~1", "HEAD"]);
        assert!(sb.git_stdin(&format!("before{round}"), &["apply", "--check", "-R", "-"], &patch).status.success());
    }
}

#[test]
fn switching_branches_updates_files_and_keeps_git_happy() {
    let sb = Sandbox::new();
    build_history(&sb, "r", "twig");
    sb.twig("r", &["switch", "-c", "feature", "HEAD~1"]);
    assert!(sb.path("r/src/util/mod.rs").exists(), "file deleted on main is back");
    assert!(!sb.path("r/docs").exists(), "directory added on main is gone");
    assert_eq!(sb.read("r/src/main.rs"), "fn main() {\n    println!(\"hi\");\n}\n");
    assert_eq!(sb.git("r", &["status", "--porcelain"]), "");
    assert_eq!(sb.git("r", &["rev-parse", "--abbrev-ref", "HEAD"]), "feature\n");

    // local edits that don't conflict are carried across
    sb.write("r/README.md", "local edit\n");
    sb.twig("r", &["switch", "main"]);
    assert_eq!(sb.read("r/README.md"), "local edit\n");
    assert_eq!(sb.twig("r", &["status", "--porcelain"]), " M README.md\n");

    // ...but edits to files the switch must change stop it
    sb.write("r/src/main.rs", "dirty\n");
    let (_, err) = sb.twig_fail("r", &["switch", "feature"]);
    assert!(err.contains("would be overwritten"), "{err}");
    assert_eq!(sb.read("r/src/main.rs"), "dirty\n");

    // detached HEAD, then back
    sb.twig("r", &["checkout", "-f", "HEAD~1"]);
    assert_eq!(sb.git("r", &["status", "--porcelain"]), "");
    assert_eq!(sb.git("r", &["rev-parse", "HEAD"]), sb.git("r", &["rev-parse", "main~1"]));
    assert!(sb.twig("r", &["branch"]).starts_with("* (HEAD detached at"));
    sb.twig("r", &["switch", "main"]);
    assert_eq!(sb.git("r", &["rev-parse", "--abbrev-ref", "HEAD"]), "main\n");
}

/// Same history, merged once by twig and once by Git: the resulting trees must be identical.
#[test]
fn clean_merges_produce_the_same_tree_as_git() {
    let sb = Sandbox::new();
    sb.mkdir("r");
    sb.git("r", &["init", "-q"]);
    let body: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    sb.write("r/shared.txt", &body);
    sb.write("r/gone.txt", "delete me on main\n");
    sb.write("r/keep.txt", "keep\n");
    sb.git("r", &["add", "."]);
    sb.git("r", &["commit", "-q", "-m", "base"]);
    sb.git("r", &["checkout", "-q", "-b", "feature"]);
    sb.write("r/shared.txt", &body.replace("line 3\n", "line three (feature)\n"));
    sb.write("r/feature-only.txt", "from feature\n");
    sb.git("r", &["add", "."]);
    sb.git("r", &["commit", "-q", "-m", "feature work"]);
    sb.git("r", &["checkout", "-q", "main"]);
    sb.write("r/shared.txt", &body.replace("line 27\n", "line twenty-seven (main)\n"));
    std::fs::remove_file(sb.path("r/gone.txt")).unwrap();
    sb.git("r", &["add", "-A"]);
    sb.git("r", &["commit", "-q", "-m", "main work"]);
    sb.copy_dir("r", "by_git");
    sb.copy_dir("r", "by_twig");

    sb.git("by_git", &["merge", "-q", "--no-edit", "feature"]);
    let out = sb.twig("by_twig", &["merge", "feature"]);
    assert!(out.contains("Auto-merging shared.txt"), "{out}");
    assert_eq!(sb.git("by_twig", &["rev-parse", "HEAD^{tree}"]), sb.git("by_git", &["rev-parse", "HEAD^{tree}"]));
    assert_eq!(sb.git("by_twig", &["log", "-1", "--format=%P %s"]), sb.git("by_git", &["log", "-1", "--format=%P %s"]));
    assert_eq!(sb.git("by_twig", &["status", "--porcelain"]), "");
    sb.git("by_twig", &["fsck", "--strict"]);
}

#[test]
fn fast_forward_merge() {
    let sb = Sandbox::new();
    build_history(&sb, "r", "twig");
    sb.twig("r", &["switch", "-q", "-c", "old", "HEAD~1"]);
    let out = sb.twig("r", &["merge", "main"]);
    assert!(out.contains("Fast-forward"), "{out}");
    assert_eq!(sb.twig("r", &["rev-parse", "old"]), sb.twig("r", &["rev-parse", "main"]));
    assert_eq!(sb.git("r", &["status", "--porcelain"]), "");
    assert_eq!(sb.twig("r", &["merge", "main"]), "Already up to date.\n");
}

#[test]
fn conflicts_look_like_gits_and_can_be_resolved() {
    let sb = Sandbox::new();
    sb.mkdir("r");
    sb.git("r", &["init", "-q"]);
    sb.write("r/f.txt", "a\nb\nc\n");
    sb.write("r/g.txt", "will be modified and deleted\n");
    sb.git("r", &["add", "."]);
    sb.git("r", &["commit", "-q", "-m", "base"]);
    sb.git("r", &["checkout", "-q", "-b", "feature"]);
    sb.write("r/f.txt", "a\nfeature\nc\n");
    sb.git("r", &["rm", "-q", "g.txt"]);
    sb.git("r", &["commit", "-q", "-am", "feature"]);
    sb.git("r", &["checkout", "-q", "main"]);
    sb.write("r/f.txt", "a\nmain\nc\n");
    sb.write("r/g.txt", "modified on main\n");
    sb.git("r", &["commit", "-q", "-am", "main"]);
    sb.copy_dir("r", "by_git");
    sb.copy_dir("r", "by_twig");

    let _ = sb.git_stdin("by_git", &["merge", "--no-edit", "feature"], b"");
    let (code, out) = sb.twig_fail("by_twig", &["merge", "feature"]);
    assert_eq!(code, 1);
    assert!(out.contains("CONFLICT (content): Merge conflict in f.txt"), "{out}");
    assert!(out.contains("CONFLICT (modify/delete)"), "{out}");
    assert_eq!(sb.read("by_twig/f.txt"), sb.read("by_git/f.txt"));
    assert_eq!(sb.read("by_twig/f.txt"), "a\n<<<<<<< HEAD\nmain\n=======\nfeature\n>>>>>>> feature\nc\n");
    // Git reads twig's conflict stages
    assert_eq!(sb.git("by_twig", &["status", "--porcelain"]), sb.git("by_git", &["status", "--porcelain"]));
    assert_eq!(sb.git("by_twig", &["ls-files", "-s"]), sb.git("by_git", &["ls-files", "-s"]));
    let (_, err) = sb.twig_fail("by_twig", &["commit", "-m", "too early"]);
    assert!(err.contains("unmerged"), "{err}");

    sb.write("by_twig/f.txt", "a\nmain and feature\nc\n");
    sb.twig("by_twig", &["add", "f.txt", "g.txt"]);
    sb.twig("by_twig", &["commit", "-q", "-m", "merged by hand"]);
    assert_eq!(sb.git("by_twig", &["log", "-1", "--format=%P"]).split(' ').count(), 2);
    assert_eq!(sb.git("by_twig", &["status", "--porcelain"]), "");
    sb.git("by_twig", &["fsck", "--strict"]);
}

#[test]
fn merge_abort_restores_the_previous_state() {
    let sb = Sandbox::new();
    sb.mkdir("r");
    sb.twig("r", &["init", "-q"]);
    sb.write("r/f.txt", "base\n");
    sb.twig("r", &["add", "f.txt"]);
    sb.twig("r", &["commit", "-q", "-m", "base"]);
    sb.twig("r", &["branch", "other"]);
    sb.write("r/f.txt", "ours\n");
    sb.twig("r", &["commit", "-q", "-am", "ours"]);
    sb.twig("r", &["switch", "other"]);
    sb.write("r/f.txt", "theirs\n");
    sb.twig("r", &["commit", "-q", "-am", "theirs"]);
    sb.twig("r", &["switch", "main"]);
    sb.twig_fail("r", &["merge", "other"]);
    assert!(sb.twig("r", &["status"]).contains("You have unmerged paths"));
    sb.twig("r", &["merge", "--abort"]);
    assert_eq!(sb.read("r/f.txt"), "ours\n");
    assert_eq!(sb.git("r", &["status", "--porcelain"]), "");
    assert!(!sb.path("r/.git/MERGE_HEAD").exists());
}

#[test]
fn reset_restore_and_rm() {
    let sb = Sandbox::new();
    build_history(&sb, "r", "twig");
    let head = sb.twig("r", &["rev-parse", "HEAD"]);

    sb.twig("r", &["reset", "--soft", "HEAD~1"]);
    assert_eq!(sb.twig("r", &["status", "--porcelain"]), sb.git("r", &["status", "--porcelain"]));
    assert!(sb.twig("r", &["status", "--porcelain"]).contains("A  docs/guide.txt"));
    assert_eq!(sb.read("r/.git/ORIG_HEAD"), head);

    sb.twig("r", &["reset", "HEAD"]); // mixed: unstage everything
    assert_eq!(sb.twig("r", &["status", "--porcelain"]), sb.git("r", &["status", "--porcelain"]));
    assert!(sb.twig("r", &["status", "--porcelain"]).contains("?? docs/"));

    sb.twig("r", &["reset", "--hard", head.trim()]);
    assert_eq!(sb.git("r", &["status", "--porcelain"]), "");

    sb.write("r/README.md", "oops\n");
    sb.twig("r", &["add", "README.md"]);
    sb.twig("r", &["restore", "--staged", "README.md"]);
    assert_eq!(sb.twig("r", &["status", "--porcelain"]), " M README.md\n");
    sb.twig("r", &["restore", "README.md"]);
    assert_eq!(sb.git("r", &["status", "--porcelain"]), "");

    sb.twig("r", &["rm", "-q", "run.sh"]);
    assert!(!sb.path("r/run.sh").exists());
    sb.twig("r", &["rm", "-q", "-r", "--cached", "src"]);
    assert!(sb.path("r/src/main.rs").exists());
    assert_eq!(sb.twig("r", &["status", "--porcelain"]), sb.git("r", &["-c", "status.renames=false", "status", "--porcelain"]));
}

#[test]
fn clone_fetch_and_pull_from_a_local_repository() {
    let sb = Sandbox::new();
    build_history(&sb, "origin", "git");
    sb.git("origin", &["tag", "v1"]);
    sb.twig("", &["clone", "-q", "origin", "copy"]);
    assert_eq!(sb.git("copy", &["rev-parse", "HEAD"]), sb.git("origin", &["rev-parse", "HEAD"]));
    assert_eq!(sb.git("copy", &["status", "--porcelain"]), "");
    assert_eq!(sb.git("copy", &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/main\n");
    assert!(sb.git("copy", &["tag"]).contains("v1"));
    sb.git("copy", &["fsck", "--strict"]);

    // new commit upstream, then pull (fast-forward)
    sb.write("origin/later.txt", "later\n");
    sb.git("origin", &["add", "later.txt"]);
    sb.git("origin", &["commit", "-q", "-m", "later"]);
    assert!(sb.twig("copy", &["status"]).contains("up to date with 'origin/main'"));
    let out = sb.twig("copy", &["pull"]);
    assert!(out.contains("Fast-forward"), "{out}");
    assert_eq!(sb.read("copy/later.txt"), "later\n");
    assert_eq!(sb.git("copy", &["rev-parse", "HEAD"]), sb.git("origin", &["rev-parse", "HEAD"]));

    // diverge, fetch, and check ahead/behind accounting
    sb.write("copy/mine.txt", "mine\n");
    sb.twig("copy", &["add", "mine.txt"]);
    sb.twig("copy", &["commit", "-q", "-m", "mine"]);
    sb.write("origin/theirs.txt", "theirs\n");
    sb.git("origin", &["add", "theirs.txt"]);
    sb.git("origin", &["commit", "-q", "-m", "theirs"]);
    sb.twig("copy", &["fetch", "-q"]);
    assert!(sb.twig("copy", &["status"]).contains("have 1 and 1 different commits each"));
    sb.twig("copy", &["pull", "-q"]);
    assert!(sb.path("copy/theirs.txt").exists() && sb.path("copy/mine.txt").exists());
    sb.git("copy", &["fsck", "--strict"]);
}

#[test]
fn clone_over_https() {
    if std::env::var("TWIG_NETWORK_TESTS").is_err() {
        eprintln!("skipped: set TWIG_NETWORK_TESTS=1 to clone from GitHub");
        return;
    }
    let sb = Sandbox::new();
    let url = "https://github.com/octocat/Hello-World.git";
    sb.twig("", &["clone", "-q", url, "hello"]);
    let remote_head = sb.git("", &["ls-remote", url, "HEAD"]);
    assert_eq!(sb.git("hello", &["rev-parse", "HEAD"]).trim(), remote_head.split('\t').next().unwrap());
    assert!(sb.path("hello/README").exists());
    assert_eq!(sb.git("hello", &["status", "--porcelain"]), "");
    sb.git("hello", &["fsck", "--strict"]);
    assert_eq!(sb.twig("hello", &["log", "--format=%H"]), sb.git("hello", &["log", "--format=%H"]));
}

#[test]
fn ignored_files_need_force() {
    let sb = Sandbox::new();
    sb.mkdir("r");
    sb.twig("r", &["init", "-q"]);
    sb.write("r/.gitignore", "*.secret\n");
    sb.write("r/a.secret", "x\n");
    let (_, err) = sb.twig_fail("r", &["add", "a.secret"]);
    assert!(err.contains("ignored"), "{err}");
    sb.twig("r", &["add", "-f", "a.secret"]);
    assert_eq!(sb.git("r", &["ls-files"]), "a.secret\n");
}

#[test]
fn errors_are_reported_like_git() {
    let sb = Sandbox::new();
    sb.mkdir("empty");
    let (code, err) = sb.twig_fail("empty", &["status"]);
    assert_eq!(code, 128);
    assert!(err.contains("not a git repository"), "{err}");
    sb.twig("empty", &["init", "-q"]);
    let (_, err) = sb.twig_fail("empty", &["log"]);
    assert!(err.contains("does not have any commits yet"), "{err}");
    let (_, err) = sb.twig_fail("empty", &["commit", "-m", "nothing"]);
    assert!(err.contains("nothing to commit"), "{err}");
    let (_, err) = sb.twig_fail("empty", &["add", "missing.txt"]);
    assert!(err.contains("did not match any files"), "{err}");
    let (_, err) = sb.twig_fail("empty", &["branch", "bad..name"]);
    assert!(err.contains("not a valid branch name") || err.contains("does not have any commits"), "{err}");
}
