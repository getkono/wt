//! Worktree enumeration via `git worktree list --porcelain` (spec §4 sanctioned
//! subprocess read) plus missing-directory detection.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::git::cli::GitCli;
use crate::git::porcelain::{RawWorktree, parse_worktree_list};

/// Enumerates the repository's worktrees from any directory inside it, marking
/// any whose directory has been deleted externally as missing (spec §3). The
/// main worktree is listed first.
pub(crate) fn enumerate(git: &dyn GitCli, dir: &Path) -> Result<Vec<RawWorktree>> {
    let output = git.run(dir, &["worktree", "list", "--porcelain"])?;
    let mut worktrees = parse_worktree_list(&output);
    for wt in &mut worktrees {
        // A worktree is "missing" when its admin record exists but the directory
        // is gone. The bare entry has no working directory and is never missing.
        wt.is_missing = !wt.is_bare && !wt.path.exists();
    }
    Ok(worktrees)
}

/// Marker files in a worktree's admin directory that mean an operation is
/// mid-flight, paired with its name. A worktree mid-rebase or mid-bisect is
/// usually detached, so without this it would look like idle, finished work.
const IN_PROGRESS_MARKERS: [(&str, &str); 7] = [
    ("rebase-merge", "rebase"),
    ("rebase-apply", "rebase"),
    ("MERGE_HEAD", "merge"),
    ("CHERRY_PICK_HEAD", "cherry-pick"),
    ("REVERT_HEAD", "revert"),
    // A multi-commit cherry-pick or revert stopped between commits.
    ("sequencer", "cherry-pick or revert"),
    ("BISECT_LOG", "bisect"),
];

/// Files naming the branch an in-progress operation will return to: the branch
/// being rebased (`refs/heads/<name>`), or the one a bisect started from.
const IN_PROGRESS_BRANCH_FILES: [&str; 3] = [
    "rebase-merge/head-name",
    "rebase-apply/head-name",
    "BISECT_START",
];

/// The absolute admin directory (`$GIT_DIR`) of the worktree at `worktree`.
fn admin_dir(git: &dyn GitCli, worktree: &Path) -> Result<PathBuf> {
    let git_dir = git.run(worktree, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(git_dir.trim()))
}

/// The operation in progress in the worktree at `worktree` (a rebase, merge,
/// cherry-pick, revert, or bisect), or `None` when it is idle. Errors when the
/// worktree's admin directory cannot be resolved, so a caller can fail safe.
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn in_progress_op(git: &dyn GitCli, worktree: &Path) -> Result<Option<&'static str>> {
    let git_dir = admin_dir(git, worktree)?;
    Ok(IN_PROGRESS_MARKERS
        .iter()
        .find(|(marker, _)| git_dir.join(marker).exists())
        .map(|(_, op)| *op))
}

/// The local branch an in-progress rebase or bisect in the worktree at
/// `worktree` will return to, if any. Mid-operation the worktree is detached,
/// so `git worktree list` no longer names that branch — yet it is still in use.
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn in_progress_branch(git: &dyn GitCli, worktree: &Path) -> Result<Option<String>> {
    let git_dir = admin_dir(git, worktree)?;
    Ok(IN_PROGRESS_BRANCH_FILES.iter().find_map(|file| {
        let name = std::fs::read_to_string(git_dir.join(file)).ok()?;
        let name = name.trim();
        let name = name.strip_prefix("refs/heads/").unwrap_or(name);
        (!name.is_empty()).then(|| name.to_string())
    }))
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
    fn in_progress_op_names_each_marker_in_a_linked_worktree() {
        let repo = TestRepo::init();
        repo.add_worktree("busy", "../wt-busy");
        let linked = repo.root().parent().unwrap().join("wt-busy");
        assert_eq!(in_progress_op(&RealGit, &linked).unwrap(), None);
        let admin = RealGit
            .run(&linked, &["rev-parse", "--absolute-git-dir"])
            .unwrap();
        let admin = Path::new(admin.trim());
        for (marker, op) in IN_PROGRESS_MARKERS {
            let path = admin.join(marker);
            std::fs::write(&path, "").unwrap();
            assert_eq!(in_progress_op(&RealGit, &linked).unwrap(), Some(op));
            std::fs::remove_file(&path).unwrap();
        }
        // The primary worktree's own state is separate.
        assert_eq!(in_progress_op(&RealGit, repo.root()).unwrap(), None);
    }

    #[test]
    fn in_progress_op_detects_a_real_rebase_stop() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("a.txt", "topic\n");
        repo.commit_all("topic a");
        repo.git(&["checkout", "-q", "main"]);
        repo.write("a.txt", "main\n");
        repo.commit_all("main a");
        repo.git(&["checkout", "-q", "topic"]);
        // Conflicts, leaving the rebase stopped mid-flight.
        let out = RealGit.run_raw(repo.root(), &["rebase", "main"]).unwrap();
        assert!(!out.success);
        assert_eq!(
            in_progress_op(&RealGit, repo.root()).unwrap(),
            Some("rebase")
        );
        // The rebased branch is still in use although HEAD is detached.
        assert_eq!(
            in_progress_branch(&RealGit, repo.root())
                .unwrap()
                .as_deref(),
            Some("topic")
        );
    }

    #[test]
    fn in_progress_branch_names_the_branch_a_bisect_started_from() {
        let repo = TestRepo::init();
        repo.write("a.txt", "1\n");
        repo.commit_all("c1");
        assert_eq!(in_progress_branch(&RealGit, repo.root()).unwrap(), None);
        repo.git(&["bisect", "start", "HEAD", "HEAD~1"]);
        assert_eq!(
            in_progress_branch(&RealGit, repo.root())
                .unwrap()
                .as_deref(),
            Some("main")
        );
        assert!(in_progress_branch(&RealGit, Path::new("/nonexistent/wt")).is_err());
    }

    #[test]
    fn in_progress_op_errors_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(in_progress_op(&RealGit, dir.path()).is_err());
    }

    #[test]
    fn single_worktree_repo() {
        let repo = TestRepo::init();
        let wts = enumerate(&RealGit, repo.root()).unwrap();
        assert_eq!(wts.len(), 1);
        assert!(wts[0].is_main);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
    }
}
