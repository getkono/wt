//! The Git boundary (spec §4): `gix` for reads, the `git` CLI for mutations and
//! network operations. Submodules:
//!
//! - [`cli`] — the [`GitCli`] subprocess trait + [`RealGit`].
//! - `ops` — verb-named wrappers over [`GitCli`] for shared mutations.
//! - [`discover`] — repository discovery and identity via `gix`.
//! - [`porcelain`] — pure parsers for `git` porcelain output.
//! - [`submodule`] — submodule detection (`status`) and init (`update --init`).
//! - [`worktrees`] — worktree enumeration + missing detection.

pub mod aheadbehind;
pub mod cli;
pub mod commit;
pub mod discover;
pub(crate) mod ops;
pub mod porcelain;
pub mod refs;
pub mod status;
pub mod submodule;
pub mod worktrees;

pub(crate) use aheadbehind::ahead_behind;
pub use cli::{GitCli, GitOutput, RealGit};
pub(crate) use commit::{CommitInfo, abbrev_len, commit_info, recent_commits};
pub(crate) use refs::{
    Upstream, branch_ref, default_branch, is_ancestor, local_branches, resolve_hex, upstream_of,
    validate_branch_name,
};
// Only the command handlers reach for these; the core library does not.
// `default_base_ref` moved here from the TUI group when `wt issue` (a CLI
// command) started resolving its base from `origin/HEAD`.
#[cfg(feature = "cli")]
pub(crate) use refs::{all_branches, current_branch, default_base_ref, remote_branches};
// Only the TUI reaches for this.
#[cfg(feature = "tui")]
pub(crate) use refs::origin_head_branch;
pub(crate) use status::status_of;
pub(crate) use worktrees::enumerate;
