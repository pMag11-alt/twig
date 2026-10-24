//! twig: a Git implementation that reads and writes real Git repositories.
//!
//! The library is organised bottom-up: object ids and formats ([`oid`], [`object`]),
//! storage ([`odb`], [`pack`], [`delta`]), [`refs`], the [`index`], then algorithms
//! ([`diff`], [`merge`], [`revparse`], [`ignore`]) and finally [`commands`].

pub mod color;
pub mod commands;
pub mod config;
pub mod date;
pub mod delta;
pub mod diff;
pub mod ignore;
pub mod index;
pub mod merge;
pub mod object;
pub mod odb;
pub mod oid;
pub mod pack;
pub mod refs;
pub mod remote;
pub mod repo;
pub mod revparse;
pub mod worktree;

/// Returned by commands that need a specific exit status without printing an error
/// (e.g. `cat-file -e` on a missing object, or a merge that stopped on conflicts).
#[derive(Debug)]
pub struct Exit(pub i32);

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exit status {}", self.0)
    }
}

impl std::error::Error for Exit {}
