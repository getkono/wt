//! Worktree enumeration via `git worktree list --porcelain` (spec §4 sanctioned
//! subprocess read) plus missing-directory detection.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::git::cli::GitCli;
use crate::git::porcelain::{RawWorktree, parse_worktree_list};

/// Enumerates the repository's worktrees from any directory inside it, marking
/// any whose directory has been deleted externally as missing (spec §3). The
/// main worktree is listed first.
pub fn enumerate(git: &dyn GitCli, dir: &Path) -> Result<Vec<RawWorktree>> {
    let output = git.run(dir, &["worktree", "list", "--porcelain"])?;
    let mut worktrees = parse_worktree_list(&output);
    for wt in &mut worktrees {
        // A worktree is "missing" when its admin record exists but the directory
        // is gone. The bare entry has no working directory and is never missing.
        wt.is_missing = !wt.is_bare && !wt.path.exists();
    }
    Ok(worktrees)
}

/// The primary worktree root (or bare repo path): the path of the main entry
/// from [`enumerate`] (spec §3).
pub fn primary_root(worktrees: &[RawWorktree]) -> Option<PathBuf> {
    worktrees.iter().find(|w| w.is_main).map(|w| w.path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    #[test]
    fn enumerates_main_and_linked() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        repo.add_worktree("feature/y", "../wt-y");
        let wts = enumerate(&RealGit, repo.root()).unwrap();
        assert_eq!(wts.len(), 3);
        assert!(wts[0].is_main);
        let branches: Vec<_> = wts.iter().filter_map(|w| w.branch.clone()).collect();
        assert!(branches.contains(&"feature/x".to_string()));
        assert!(branches.contains(&"feature/y".to_string()));
        assert!(wts.iter().all(|w| !w.is_missing));
    }

    #[test]
    fn detects_missing_worktree() {
        let repo = TestRepo::init();
        repo.add_worktree("gone", "../wt-gone");
        let linked = repo.root().parent().unwrap().join("wt-gone");
        std::fs::remove_dir_all(&linked).unwrap();
        let wts = enumerate(&RealGit, repo.root()).unwrap();
        let missing = wts
            .iter()
            .find(|w| w.branch.as_deref() == Some("gone"))
            .unwrap();
        assert!(missing.is_missing);
    }

    #[test]
    fn single_worktree_repo() {
        let repo = TestRepo::init();
        let wts = enumerate(&RealGit, repo.root()).unwrap();
        assert_eq!(wts.len(), 1);
        assert!(wts[0].is_main);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn primary_root_is_main_worktree_even_from_linked() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let linked = repo.root().parent().unwrap().join("wt-x");
        // Enumerating from inside the linked worktree still reports the main
        // worktree as the primary root.
        let wts = enumerate(&RealGit, &linked).unwrap();
        let root = primary_root(&wts).unwrap();
        assert_eq!(canon(&root), canon(repo.root()));
    }

    fn canon(p: &std::path::Path) -> std::path::PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    }
}
