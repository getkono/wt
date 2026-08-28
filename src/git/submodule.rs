//! Git submodule detection and initialization (issue #50).
//!
//! `git submodule status` is a sanctioned subprocess read (spec §4); the parsers
//! live in [`porcelain`](super::porcelain). Initialization is a mutating/network
//! operation, so it goes through the `git` CLI. Both run against a worktree
//! directory: a freshly created worktree (or one switched to a branch that adds
//! submodules) reports its submodules as uninitialized until `update --init`
//! populates them.
//!
//! # Why a new worktree re-clones everything
//!
//! A linked worktree does **not** share the superproject's submodule object
//! stores. Git resolves a linked worktree's submodule gitdir to
//! `$GIT_COMMON_DIR/worktrees/<id>/modules/<name>`, not the shared
//! `$GIT_COMMON_DIR/modules/<name>`, so a plain `update --init --recursive` in a
//! new worktree performs a full network clone of every submodule even though
//! byte-identical objects already sit on the same disk.
//!
//! Two consequences show up here: enumeration has to recurse to be honest about
//! nested submodules, and `git worktree remove` refuses any worktree that has a
//! populated submodule.

use std::path::Path;

use crate::error::Result;
use crate::git::cli::GitCli;
use crate::git::porcelain::parse_submodule_status;

/// Returns the paths of submodules that are defined but not yet initialized in
/// `worktree_dir` (the `-` marker of `git submodule status`). Best-effort: a repo
/// with no submodules, or a directory where the command cannot run, yields an
/// empty list rather than an error, so callers can treat "no submodules" and
/// "could not tell" alike.
///
/// Recursive, so nested submodules count too. Note that `--recursive` can only
/// descend into submodules that are already initialized: an uninitialized parent
/// hides its children until it is populated. The count is therefore honest about
/// what is knowable now, not a prediction of the eventual total.
pub fn uninitialized(git: &dyn GitCli, worktree_dir: &Path) -> Result<Vec<String>> {
    let output = git.run_raw(worktree_dir, &["submodule", "status", "--recursive"])?;
    if !output.success {
        return Ok(Vec::new());
    }
    Ok(parse_submodule_status(&output.stdout)
        .into_iter()
        .filter(|s| s.is_uninitialized())
        .map(|s| s.path)
        .collect())
}

/// Whether `worktree_dir` currently has at least one *populated* submodule.
///
/// This is the condition `git worktree remove` refuses on
/// (`fatal: working trees containing submodules cannot be moved or removed`),
/// so removal consults it to decide whether git needs forcing. Best-effort, like
/// [`uninitialized`]: "could not tell" reports `false`.
pub(crate) fn any_initialized(git: &dyn GitCli, worktree_dir: &Path) -> Result<bool> {
    let output = git.run_raw(worktree_dir, &["submodule", "status", "--recursive"])?;
    if !output.success {
        return Ok(false);
    }
    Ok(parse_submodule_status(&output.stdout)
        .into_iter()
        .any(|s| !s.is_uninitialized()))
}

/// Initializes and updates all submodules in `worktree_dir`, recursively
/// (`git submodule update --init --recursive`). Propagates a subprocess error;
/// callers decide whether that is fatal.
pub fn update_init(git: &dyn GitCli, worktree_dir: &Path) -> Result<()> {
    git.run(
        worktree_dir,
        &["submodule", "update", "--init", "--recursive"],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    #[test]
    fn no_submodules_yields_empty() {
        let repo = TestRepo::init();
        assert!(uninitialized(&RealGit, repo.root()).unwrap().is_empty());
    }

    #[test]
    fn reports_uninitialized_submodule() {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        // After `add` the submodule is initialized; deinit makes it report `-`.
        repo.deinit_submodule("libs/sub");
        let pending = uninitialized(&RealGit, repo.root()).unwrap();
        assert_eq!(pending, vec!["libs/sub".to_string()]);
    }

    #[test]
    fn update_init_populates_submodule() {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        repo.deinit_submodule("libs/sub");
        // Sanity: empty before, populated after (reuses .git/modules, no clone).
        assert!(!repo.root().join("libs/sub/sub.txt").exists());
        update_init(&RealGit, repo.root()).unwrap();
        assert!(repo.root().join("libs/sub/sub.txt").exists());
        assert!(uninitialized(&RealGit, repo.root()).unwrap().is_empty());
    }

    #[test]
    fn uninitialized_is_empty_outside_a_repo() {
        // `git submodule status` fails (non-success) in a non-repo dir; the
        // best-effort contract returns an empty list rather than erroring.
        let dir = tempfile::tempdir().unwrap();
        assert!(uninitialized(&RealGit, dir.path()).unwrap().is_empty());
    }

    #[test]
    fn uninitialized_recurses_into_nested_submodules() {
        let repo = TestRepo::init();
        repo.add_nested_submodule("libs/sub", "deep");
        // Everything is populated after the fixture builds it.
        assert!(uninitialized(&RealGit, repo.root()).unwrap().is_empty());
        // Deinit only the *nested* one. A non-recursive `submodule status`
        // reports nothing here, which is the undercount this recursion fixes.
        repo.git(&["-C", "libs/sub", "submodule", "deinit", "-q", "-f", "deep"]);
        let pending = uninitialized(&RealGit, repo.root()).unwrap();
        assert_eq!(pending, vec!["libs/sub/deep".to_string()]);
    }

    #[test]
    fn any_initialized_tracks_population() {
        let repo = TestRepo::init();
        assert!(!any_initialized(&RealGit, repo.root()).unwrap());
        repo.add_submodule("libs/sub");
        assert!(any_initialized(&RealGit, repo.root()).unwrap());
        repo.deinit_submodule("libs/sub");
        assert!(!any_initialized(&RealGit, repo.root()).unwrap());
    }
}
