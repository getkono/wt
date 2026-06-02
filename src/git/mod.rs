//! The Git boundary (spec §4): `gix` for reads, the `git` CLI for mutations and
//! network operations. Submodules:
//!
//! - [`cli`] — the [`GitCli`](cli::GitCli) subprocess trait + [`RealGit`](cli::RealGit).
//! - [`discover`] — repository discovery and identity via `gix`.
//! - [`porcelain`] — pure parsers for `git` porcelain output.
//! - [`worktrees`] — worktree enumeration + missing detection.

pub mod aheadbehind;
pub mod cli;
pub mod commit;
pub mod discover;
pub mod porcelain;
pub mod refs;
pub mod status;
pub mod worktrees;

pub use aheadbehind::ahead_behind;
pub use cli::{GitCli, GitOutput, RealGit};
pub use commit::{CommitInfo, abbrev_len, commit_info, recent_commits};
pub use discover::Repo;
pub use porcelain::{RawWorktree, parse_worktree_list};
pub use refs::{Upstream, default_branch, is_ancestor, local_branches, resolve_hex, upstream_of};
pub use status::{StatusSummary, status_of};
pub use worktrees::{enumerate, primary_root};
