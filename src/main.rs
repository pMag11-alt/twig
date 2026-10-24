use std::io::{BufWriter, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use twig::commands::branching::{self, BranchOptions, ResetMode, SwitchOptions, TagOptions};
use twig::commands::history::{self, Format, LogOptions};
use twig::commands::plumbing::{self, CatMode};
use twig::commands::staging::{self, CommitOptions};
use twig::commands::{DiffOptions, remote};
use twig::object::Kind;
use twig::repo::Repo;
use twig::{Exit, color, revparse};

#[derive(Parser)]
#[command(name = "twig", version, about = "A Git implementation in Rust that reads and writes real Git repositories")]
struct Cli {
    /// Run as if twig was started in <path>
    #[arg(short = 'C', global = true, value_name = "path")]
    chdir: Option<PathBuf>,
    /// Never use colours
    #[arg(long, global = true)]
    no_color: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create an empty repository
    Init {
        path: Option<PathBuf>,
        #[arg(short = 'b', long = "initial-branch")]
        branch: Option<String>,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Clone a repository over HTTPS (smart HTTP protocol) or from a local path
    Clone {
        url: String,
        dir: Option<PathBuf>,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Download objects and refs from a remote
    Fetch {
        remote: Option<String>,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Fetch and merge the current branch's upstream
    Pull {
        remote: Option<String>,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Add file contents to the index
    Add {
        paths: Vec<PathBuf>,
        /// Stage all changes, including deletions and new files
        #[arg(short = 'A', long)]
        all: bool,
        /// Allow adding ignored files
        #[arg(short, long)]
        force: bool,
    },
    /// Remove files from the work tree and the index
    Rm {
        paths: Vec<PathBuf>,
        #[arg(short, long)]
        quiet: bool,
        /// Only remove from the index
        #[arg(long)]
        cached: bool,
        #[arg(short = 'r')]
        recursive: bool,
        #[arg(short, long)]
        force: bool,
    },
    /// Show the working tree status
    Status {
        #[arg(short, long)]
        short: bool,
        #[arg(long)]
        porcelain: bool,
    },
    /// Record changes to the repository
    Commit {
        #[arg(short = 'm', long = "message")]
        message: Vec<String>,
        /// Take the message from a file ("-" for stdin)
        #[arg(short = 'F', long = "file")]
        file: Option<String>,
        /// Stage modified and deleted tracked files first
        #[arg(short = 'a', long)]
        all: bool,
        #[arg(long)]
        amend: bool,
        #[arg(long)]
        allow_empty: bool,
        #[arg(short, long)]
        quiet: bool,
    },
    /// Show commit history
    Log {
        revs: Vec<String>,
        #[arg(long)]
        oneline: bool,
        #[arg(short = 'n', long = "max-count")]
        max_count: Option<usize>,
        /// oneline, medium, or a template such as "%h %an %s" ("format:" prefix optional)
        #[arg(long, alias = "pretty")]
        format: Option<String>,
        #[arg(long)]
        all: bool,
        /// Show a patch for each commit
        #[arg(short = 'p', long)]
        patch: bool,
        #[arg(long)]
        stat: bool,
        #[arg(long)]
        decorate: bool,
        #[arg(long)]
        no_decorate: bool,
    },
    /// Show a commit (with its diff), tag, tree or blob
    Show {
        rev: Option<String>,
        #[arg(long)]
        stat: bool,
    },
    /// Show changes between the work tree, the index and commits
    Diff {
        revs: Vec<String>,
        #[arg(long, visible_alias = "staged")]
        cached: bool,
        #[arg(long)]
        stat: bool,
        #[arg(long)]
        name_only: bool,
        #[arg(long)]
        name_status: bool,
        /// Lines of context
        #[arg(short = 'U', long = "unified", default_value_t = 3)]
        context: usize,
    },
    /// List, create, rename or delete branches
    Branch {
        name: Option<String>,
        start: Option<String>,
        #[arg(short = 'd', long)]
        delete: bool,
        #[arg(short = 'D')]
        force_delete: bool,
        #[arg(short = 'm', long = "move")]
        rename: bool,
        #[arg(short, long)]
        verbose: bool,
        #[arg(short, long)]
        all: bool,
    },
    /// Switch branches
    Switch {
        target: Option<String>,
        #[arg(short, long)]
        quiet: bool,
        /// Create a new branch (at <target>, or HEAD) and switch to it
        #[arg(short = 'c', long = "create")]
        create: Option<String>,
        #[arg(long)]
        detach: bool,
        /// Throw away local changes
        #[arg(short, long)]
        force: bool,
    },
    /// Switch branches, detach HEAD at a commit, or restore files
    Checkout {
        target: Option<String>,
        #[arg(short, long)]
        quiet: bool,
        #[arg(short = 'b')]
        new_branch: Option<String>,
        #[arg(short, long)]
        force: bool,
        #[arg(last = true)]
        paths: Vec<PathBuf>,
    },
    /// Restore work tree files, or unstage them with --staged
    Restore {
        paths: Vec<PathBuf>,
        #[arg(short = 'S', long)]
        staged: bool,
        #[arg(short = 'W', long)]
        worktree: bool,
        #[arg(short, long)]
        source: Option<String>,
    },
    /// Move HEAD (and optionally the index and work tree) to a commit, or unstage paths
    Reset {
        args: Vec<String>,
        #[arg(short, long)]
        quiet: bool,
        #[arg(long)]
        soft: bool,
        #[arg(long)]
        mixed: bool,
        #[arg(long)]
        hard: bool,
    },
    /// Join another branch into the current one
    Merge {
        rev: Option<String>,
        #[arg(short, long)]
        quiet: bool,
        #[arg(short = 'm')]
        message: Option<String>,
        #[arg(long)]
        no_ff: bool,
        #[arg(long)]
        abort: bool,
    },
    /// List, create or delete tags
    Tag {
        name: Option<String>,
        target: Option<String>,
        #[arg(short = 'a')]
        annotate: bool,
        #[arg(short = 'm')]
        message: Option<String>,
        #[arg(short = 'd', long)]
        delete: bool,
        #[arg(short, long)]
        force: bool,
    },
    /// Get or set configuration
    Config {
        key: Option<String>,
        value: Option<String>,
        #[arg(long)]
        global: bool,
        #[arg(short, long)]
        list: bool,
    },
    /// Compute an object id, optionally writing a blob
    HashObject {
        files: Vec<PathBuf>,
        #[arg(short = 'w')]
        write: bool,
        #[arg(short = 't', default_value = "blob")]
        kind: String,
        #[arg(long)]
        stdin: bool,
    },
    /// Show an object's type, size or contents
    CatFile {
        /// <object>, or <type> <object>
        args: Vec<String>,
        #[arg(short = 't')]
        kind: bool,
        #[arg(short = 's')]
        size: bool,
        #[arg(short = 'p')]
        pretty: bool,
        #[arg(short = 'e')]
        exists: bool,
    },
    /// List a tree's contents
    LsTree {
        treeish: String,
        #[arg(short = 'r')]
        recursive: bool,
        #[arg(short = 't')]
        show_trees: bool,
        #[arg(long)]
        name_only: bool,
    },
    /// Show what's in the index
    LsFiles {
        #[arg(short = 's', long)]
        stage: bool,
    },
    /// Write the index as a tree object
    WriteTree,
    /// Create a commit object from a tree
    CommitTree {
        tree: String,
        #[arg(short = 'p')]
        parents: Vec<String>,
        #[arg(short = 'm')]
        message: Vec<String>,
    },
    /// Resolve revisions to object ids
    RevParse {
        revs: Vec<String>,
        #[arg(long)]
        short: bool,
        #[arg(long)]
        abbrev_ref: bool,
        #[arg(long)]
        show_toplevel: bool,
        #[arg(long)]
        git_dir: bool,
    },
    /// Find the best common ancestor of two commits
    MergeBase {
        a: String,
        b: String,
        #[arg(long)]
        is_ancestor: bool,
    },
}

/// `log -3` means `log -n 3`; clap needs the long form.
fn expand_numeric_flags(args: Vec<String>) -> Vec<String> {
    let is_log = args.iter().skip(1).find(|a| !a.starts_with('-')).is_some_and(|a| a == "log");
    if !is_log {
        return args;
    }
    args.into_iter()
        .flat_map(|a| match a.strip_prefix('-') {
            Some(n) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => vec!["-n".to_string(), n.to_string()],
            _ => vec![a],
        })
        .collect()
}

fn main() -> ExitCode {
    let cli = Cli::parse_from(expand_numeric_flags(std::env::args().collect()));
    let stdout = std::io::stdout();
    let tty = stdout.is_terminal();
    color::set_enabled(tty && !cli.no_color && std::env::var_os("NO_COLOR").is_none());
    let mut out = BufWriter::new(stdout.lock());
    let result = run(cli, tty, &mut out);
    let flushed = out.flush();
    match result.and(flushed.map_err(Into::into)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            if let Some(Exit(code)) = e.downcast_ref::<Exit>() {
                return ExitCode::from(*code as u8);
            }
            if e.chain().any(|c| c.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)) {
                return ExitCode::SUCCESS; // e.g. `twig log | head`
            }
            eprintln!("fatal: {e:#}");
            ExitCode::from(128)
        }
    }
}

fn run(cli: Cli, tty: bool, out: &mut dyn Write) -> Result<()> {
    if let Some(dir) = &cli.chdir {
        std::env::set_current_dir(dir)?;
    }
    let cwd = std::env::current_dir()?;
    let open = || Repo::discover(&cwd);
    match cli.cmd {
        Cmd::Init { path, branch, quiet } => staging::init(&path.unwrap_or(cwd.clone()), branch.as_deref(), quiet, out),
        Cmd::Clone { url, dir, quiet } => remote::clone(&url, dir.as_deref(), quiet, out),
        Cmd::Fetch { remote: r, quiet } => remote::fetch(open()?, r.as_deref().unwrap_or("origin"), quiet, out).map(|_| ()),
        Cmd::Pull { remote: r, quiet } => remote::pull(open()?, r.as_deref(), quiet, out),
        Cmd::Add { paths, all, force } => staging::add(&open()?, &paths, all, force, out),
        Cmd::Rm { paths, quiet, cached, recursive, force } => {
            let mut sink = std::io::sink();
            staging::rm(&open()?, &paths, cached, recursive, force, if quiet { &mut sink } else { out })
        }
        Cmd::Status { short, porcelain } => staging::status(&open()?, short, porcelain, out),
        Cmd::Commit { message, file, all, amend, allow_empty, quiet } => {
            staging::commit(&open()?, CommitOptions { messages: message, file, all, amend, allow_empty, quiet }, out)
        }
        Cmd::Log { revs, oneline, max_count, format, all, patch, stat, decorate, no_decorate } => {
            let format = match format.as_deref() {
                None if oneline => Format::Oneline,
                None | Some("medium") => Format::Medium,
                Some("oneline") => Format::Oneline,
                Some(f) => Format::Template(f.trim_start_matches("format:").trim_start_matches("tformat:").to_string()),
            };
            let decorate = (decorate || tty) && !no_decorate;
            history::log(&open()?, LogOptions { format, max_count, all, patch, stat, decorate, revs }, out)
        }
        Cmd::Show { rev, stat } => {
            history::show(&open()?, rev.as_deref().unwrap_or("HEAD"), DiffOptions { stat, context: 3, ..Default::default() }, out)
        }
        Cmd::Diff { revs, cached, stat, name_only, name_status, context } => {
            history::diff(&open()?, cached, &revs, DiffOptions { stat, name_only, name_status, context }, out)
        }
        Cmd::Branch { name, start, delete, force_delete, rename, verbose, all } => {
            branching::branch(&open()?, BranchOptions { name, start, delete, force_delete, rename, verbose, all }, out)
        }
        Cmd::Switch { target, quiet, create, detach, force } => {
            let mut sink = std::io::sink();
            let out: &mut dyn Write = if quiet { &mut sink } else { out };
            branching::switch(&open()?, SwitchOptions { target, create, detach, force, implicit_detach: false }, out)
        }
        Cmd::Checkout { target, quiet, new_branch, force, paths } => {
            let mut sink = std::io::sink();
            let out: &mut dyn Write = if quiet { &mut sink } else { out };
            checkout(&open()?, target, new_branch, force, paths, out)
        }
        Cmd::Restore { paths, staged, worktree, source } => branching::restore(&open()?, &paths, staged, worktree, source.as_deref(), out),
        Cmd::Reset { args, quiet, soft, mixed, hard } => {
            let mut sink = std::io::sink();
            let out: &mut dyn Write = if quiet { &mut sink } else { out };
            let repo = open()?;
            let mode = match (soft, mixed, hard) {
                (true, false, false) => ResetMode::Soft,
                (false, _, false) => ResetMode::Mixed,
                (false, false, true) => ResetMode::Hard,
                _ => bail!("--soft, --mixed and --hard are mutually exclusive"),
            };
            // `reset <commit> [paths]` or `reset <paths>`
            let (target, paths): (Option<&str>, Vec<PathBuf>) = match args.split_first() {
                Some((first, rest)) if revparse::commit(&repo, first).is_ok() => (Some(first), rest.iter().map(PathBuf::from).collect()),
                _ => (None, args.iter().map(PathBuf::from).collect()),
            };
            if !paths.is_empty() && mode != ResetMode::Mixed {
                bail!("Cannot do soft or hard reset with paths.");
            }
            branching::reset(&repo, mode, target, &paths, out)
        }
        Cmd::Merge { rev, quiet, message, no_ff, abort } => {
            let mut sink = std::io::sink();
            let out: &mut dyn Write = if quiet { &mut sink } else { out };
            let repo = open()?;
            if abort {
                return branching::merge_abort(&repo, out);
            }
            let Some(rev) = rev else { bail!("specify a branch or commit to merge") };
            branching::merge_cmd(&repo, &rev, message, no_ff, out)
        }
        Cmd::Tag { name, target, annotate, message, delete, force } => {
            branching::tag(&open()?, TagOptions { name, target, annotate, message, delete, force }, out)
        }
        Cmd::Config { key, value, global, list } => {
            let repo = open().ok();
            plumbing::config_cmd(repo.as_ref(), global, list, key.as_deref(), value.as_deref(), out)
        }
        Cmd::HashObject { files, write, kind, stdin } => {
            let repo = open().ok();
            plumbing::hash_object(repo.as_ref(), &kind, write, stdin, &files, out)
        }
        Cmd::CatFile { args, kind, size, pretty, exists } => {
            let repo = open()?;
            let (mode, obj) = match (kind, size, pretty, exists, args.as_slice()) {
                (true, false, false, false, [o]) => (CatMode::Type, o),
                (false, true, false, false, [o]) => (CatMode::Size, o),
                (false, false, true, false, [o]) => (CatMode::Pretty, o),
                (false, false, false, true, [o]) => (CatMode::Exists, o),
                (false, false, false, false, [t, o]) => (CatMode::Raw(Kind::parse(t)?), o),
                _ => bail!("usage: twig cat-file (-t | -s | -p | -e | <type>) <object>"),
            };
            plumbing::cat_file(&repo, mode, obj, out)
        }
        Cmd::LsTree { treeish, recursive, show_trees, name_only } => {
            plumbing::ls_tree(&open()?, &treeish, recursive, show_trees, name_only, out)
        }
        Cmd::LsFiles { stage } => plumbing::ls_files(&open()?, stage, out),
        Cmd::WriteTree => plumbing::write_tree(&open()?, out),
        Cmd::CommitTree { tree, parents, message } => plumbing::commit_tree(&open()?, &tree, &parents, &message, out),
        Cmd::RevParse { revs, short, abbrev_ref, show_toplevel, git_dir } => {
            let repo = open()?;
            if show_toplevel {
                writeln!(out, "{}", repo.work_tree.display())?;
            }
            if git_dir {
                writeln!(out, "{}", repo.git_dir.display())?;
            }
            plumbing::rev_parse(&repo, &revs, short, abbrev_ref, out)
        }
        Cmd::MergeBase { a, b, is_ancestor } => plumbing::merge_base(&open()?, &a, &b, is_ancestor, out),
    }
}

fn checkout(
    repo: &Repo,
    target: Option<String>,
    new_branch: Option<String>,
    force: bool,
    paths: Vec<PathBuf>,
    out: &mut dyn Write,
) -> Result<()> {
    if !paths.is_empty() {
        // checkout [<rev>] -- <paths>: from the index, or from <rev> into index and work tree
        return match target {
            Some(rev) => branching::restore(repo, &paths, true, true, Some(&rev), out),
            None => branching::restore(repo, &paths, false, true, None, out),
        };
    }
    if new_branch.is_some() {
        return branching::switch(repo, SwitchOptions { target, create: new_branch, detach: false, force, implicit_detach: true }, out);
    }
    let Some(t) = target else { bail!("you must specify a branch, commit or path") };
    let is_rev = revparse::commit(repo, &t).is_ok() || repo.refs.exists(&format!("refs/remotes/origin/{t}"))?;
    if !is_rev && repo.abs(&repo.rel_path(std::path::Path::new(&t))?).exists() {
        return branching::restore(repo, &[PathBuf::from(&t)], false, true, None, out);
    }
    branching::switch(repo, SwitchOptions { target: Some(t), create: None, detach: false, force, implicit_detach: true }, out)
}
