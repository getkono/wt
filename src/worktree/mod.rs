//! The worktree operation layer (issue #95).
//!
//! [`service`] is the public, stateless surface: a [`Workspace`] discovered
//! from a directory can enumerate, create, and remove worktrees and read their
//! `wt.*` metadata with **no prompting, no terminal, and no
//! [`Cx`](crate::cx::Cx)** — outcomes and warnings are returned as data and the
//! caller decides how (or whether) to present them. The `wt` CLI and TUI are
//! thin interactive wrappers over this module; embedders (karet) call it
//! directly and must resolve worktree paths through it rather than
//! reimplementing the `.wt.toml` layout rules.
//!
//! [`rows`] is the crate-internal row assembly on top: enriched listing rows,
//! worktree-less branch rows (issue #47), sorting, and the remove/prune guard
//! evaluation shared by the CLI and TUI.

pub(crate) mod rows;
mod service;

pub(crate) use rows::{
    build_rows, build_worktrees, enumerate_rows, enumerate_worktrees, guard_status, sort_worktrees,
    sort_worktrees_base_first,
};
pub use service::{
    CreateOptions, CreatedWorktree, HookOutcome, RemoveOptions, RemovedWorktree, SubmodulesOutcome,
    Workspace,
};
pub(crate) use service::{
    WorkspaceParts, create_in, remove_in, resolve_base, resolve_target, rollback_worktree,
    run_best_effort, same_path,
};
