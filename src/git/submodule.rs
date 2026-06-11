//! Git submodule detection and initialization (issue #50).
//!
//! `git submodule status` is a sanctioned subprocess read (spec §4); the parser
//! lives in [`porcelain`](super::porcelain). Initialization is a mutating/network
//! operation, so it goes through the `git` CLI. Both run against a worktree
//! directory: a freshly created worktree (or one switched to a branch that adds
//! submodules) reports its submodules as uninitialized until `update --init`
//! populates them.

use std::path::Path;

use crate::error::Result;
use crate::git::cli::GitCli;
use crate::git::porcelain::parse_submodule_status;

/// Returns the paths of submodules that are defined but not yet initialized in
/// `worktree_dir` (the `-` marker of `git submodule status`). Best-effort: a repo
/// with no submodules, or a directory where the command cannot run, yields an
/// empty list rather than an error, so callers can treat "no submodules" and
/// "could not tell" alike.
pub fn uninitialized(git: &dyn GitCli, worktree_dir: &Path) -> Result<Vec<String>> {
    let output = git.run_raw(worktree_dir, &["submodule", "status"])?;
    if !output.success {
        return Ok(Vec::new());
    }
    Ok(parse_submodule_status(&output.stdout)
        .into_iter()
        .filter(|s| s.is_uninitialized())
        .map(|s| s.path)
        .collect())
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
}
