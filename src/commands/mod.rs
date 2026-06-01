//! Command handlers (spec §7). Each module implements one subcommand; this
//! module provides the shared repository session setup and query resolution.

pub mod complete;
pub mod completions;
pub mod list;
pub mod path;
pub mod root;
pub mod shell_init;
pub mod status_cmd;

use std::path::{Path, PathBuf};

use crate::config::{self, Config};
use crate::cx::Cx;
use crate::error::Result;
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::model::Worktree;
use crate::query::{self, Resolved};

/// A discovered repository plus its resolved configuration, set up once per
/// repo-scoped command.
pub struct Session {
    /// The discovered repository (gix handle).
    pub repo: Repo,
    /// The primary worktree root (or bare repo path).
    pub primary_root: PathBuf,
    /// The merged configuration.
    pub config: Config,
}

/// Discovers the repository from the context's working directory, resolves the
/// primary root via `git rev-parse`, and loads the merged configuration.
pub fn open_session(cx: &Cx, git: &dyn GitCli) -> Result<Session> {
    let repo = Repo::discover(&cx.cwd)?;
    let dir = repo.current_workdir().unwrap_or_else(|| repo.git_dir());
    let common = git.run(
        &dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common.trim());
    let primary_root = if repo.is_bare() {
        common
    } else {
        common.parent().map(Path::to_path_buf).unwrap_or(common)
    };
    let config = config::load(Some(&primary_root), &cx.env)?;
    Ok(Session {
        repo,
        primary_root,
        config,
    })
}

/// The outcome of resolving a query in a command handler.
pub enum Resolution {
    /// A unique worktree (its index).
    Found(usize),
    /// Ambiguous; candidates were listed to stderr (exit code `3`).
    Ambiguous,
    /// No match (the caller maps this to [`Error::NotFound`]).
    NotFound,
}

/// Resolves a query, reporting ambiguity (candidate list) to stderr. The picker
/// fallback for an interactive TTY is wired once the TUI exists (spec §7).
pub fn resolve_query(cx: &mut Cx, worktrees: &[Worktree], query: &str) -> Resolution {
    match query::resolve(worktrees, query) {
        Resolved::One(index) => Resolution::Found(index),
        Resolved::Ambiguous(indices) => {
            let _ = cx
                .err
                .line(&format!("query {query:?} is ambiguous; candidates:"));
            for index in indices {
                let _ = cx
                    .err
                    .line(&format!("  {}", candidate_label(&worktrees[index])));
            }
            Resolution::Ambiguous
        }
        Resolved::NotFound => Resolution::NotFound,
    }
}

/// A human label for a worktree in candidate/diagnostic lists.
pub fn candidate_label(worktree: &Worktree) -> String {
    match &worktree.branch {
        Some(branch) => branch.clone(),
        None => worktree.path.display().to_string(),
    }
}
