//! The Git boundary (spec §4): `gix` for reads, the `git` CLI for mutations and
//! network operations. Submodules:
//!
//! - [`cli`] — the [`GitCli`](cli::GitCli) subprocess trait + [`RealGit`](cli::RealGit).
//! - [`discover`] — repository discovery and identity via `gix`.
//! - [`porcelain`] — pure parsers for `git` porcelain output.
//! - [`worktrees`] — worktree enumeration + missing detection.

pub mod cli;
pub mod discover;
pub mod porcelain;
pub mod worktrees;

pub use cli::{GitCli, GitOutput, RealGit};
pub use discover::Repo;
pub use porcelain::{RawWorktree, parse_worktree_list};
pub use worktrees::{enumerate, primary_root};
