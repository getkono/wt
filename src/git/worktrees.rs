//! Worktree enumeration via `git worktree list --porcelain` (spec §4 sanctioned
//! subprocess read) plus missing-directory detection.

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};
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
/// being rebased (`refs/heads/<name>`), or the one a bisect started from —
/// each paired with what is happening to that branch.
const IN_PROGRESS_BRANCH_FILES: [(&str, &str); 3] = [
    ("rebase-merge/head-name", "being rebased"),
    ("rebase-apply/head-name", "being rebased"),
    ("BISECT_START", "being bisected"),
];

/// A local branch an in-progress rebase or bisect will return to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeldBranch {
    /// The branch's short name.
    pub(crate) name: String,
    /// What is happening to it: `being rebased` or `being bisected`.
    pub(crate) activity: &'static str,
}

/// The absolute admin directory (`$GIT_DIR`) of the worktree at `worktree`.
/// A `missing` worktree's directory is gone, so git cannot be asked from inside
/// it; its admin directory is found from the repository at `repo_dir` instead
/// (see [`missing_admin_dir`]).
fn admin_dir(git: &dyn GitCli, repo_dir: &Path, worktree: &Path, missing: bool) -> Result<PathBuf> {
    if missing {
        return missing_admin_dir(git, repo_dir, worktree);
    }
    let git_dir = git.run(worktree, &["rev-parse", "--absolute-git-dir"])?;
    Ok(PathBuf::from(git_dir.trim()))
}

/// The admin directory of a linked worktree whose directory is gone: the entry
/// under `<common-dir>/worktrees/` whose `gitdir` file points at
/// `<worktree>/.git`. Errors when no entry can be matched or one cannot be
/// read, so a caller can fail safe.
fn missing_admin_dir(git: &dyn GitCli, repo_dir: &Path, worktree: &Path) -> Result<PathBuf> {
    let common = git.run(repo_dir, &["rev-parse", "--git-common-dir"])?;
    let entries = repo_dir.join(common.trim()).join("worktrees");
    let target = worktree.join(".git");
    for entry in std::fs::read_dir(&entries)? {
        let admin = entry?.path();
        let recorded = match std::fs::read_to_string(admin.join("gitdir")) {
            Ok(recorded) => recorded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        // `worktree.useRelativePaths` records the path relative to the entry.
        if normalize(&admin.join(recorded.trim())) == target {
            return Ok(admin);
        }
    }
    Err(Error::operation(format!(
        "no worktree admin directory records {}",
        worktree.display()
    )))
}

/// `path` with `.` and `..` components resolved lexically (the path may not
/// exist, so it cannot be canonicalized).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The operation in progress in the worktree at `worktree` (a rebase, merge,
/// cherry-pick, revert, or bisect), or `None` when it is idle. A `missing`
/// worktree's state is read through the repository at `repo_dir`. Errors when
/// the worktree's admin directory cannot be resolved, so a caller can fail safe.
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn in_progress_op(
    git: &dyn GitCli,
    repo_dir: &Path,
    worktree: &Path,
    missing: bool,
) -> Result<Option<&'static str>> {
    let git_dir = admin_dir(git, repo_dir, worktree, missing)?;
    Ok(IN_PROGRESS_MARKERS
        .iter()
        .find(|(marker, _)| git_dir.join(marker).exists())
        .map(|(_, op)| *op))
}

/// The local branch an in-progress rebase or bisect in the worktree at
/// `worktree` will return to, if any. Mid-operation the worktree is detached,
/// so `git worktree list` no longer names that branch — yet it is still in use.
/// An absent state file means no such operation; any other read failure (a
/// permission error, a file that is not UTF-8) is an error, so a caller can
/// fail safe rather than take the branch for unused. A `missing` worktree's
/// state is read through the repository at `repo_dir`.
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn in_progress_branch(
    git: &dyn GitCli,
    repo_dir: &Path,
    worktree: &Path,
    missing: bool,
) -> Result<Option<HeldBranch>> {
    let git_dir = admin_dir(git, repo_dir, worktree, missing)?;
    for (file, activity) in IN_PROGRESS_BRANCH_FILES {
        let contents = match std::fs::read_to_string(git_dir.join(file)) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let name = contents.trim();
        let name = name.strip_prefix("refs/heads/").unwrap_or(name);
        if !name.is_empty() {
            return Ok(Some(HeldBranch {
                name: name.to_string(),
                activity,
            }));
        }
    }
    Ok(None)
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
        assert_eq!(
            in_progress_op(&RealGit, repo.root(), &linked, false).unwrap(),
            None
        );
        let admin = RealGit
            .run(&linked, &["rev-parse", "--absolute-git-dir"])
            .unwrap();
        let admin = Path::new(admin.trim());
        for (marker, op) in IN_PROGRESS_MARKERS {
            let path = admin.join(marker);
            std::fs::write(&path, "").unwrap();
            assert_eq!(
                in_progress_op(&RealGit, repo.root(), &linked, false).unwrap(),
                Some(op)
            );
            std::fs::remove_file(&path).unwrap();
        }
        // The primary worktree's own state is separate.
        assert_eq!(
            in_progress_op(&RealGit, repo.root(), repo.root(), false).unwrap(),
            None
        );
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
            in_progress_op(&RealGit, repo.root(), repo.root(), false).unwrap(),
            Some("rebase")
        );
        // The rebased branch is still in use although HEAD is detached.
        assert_eq!(
            in_progress_branch(&RealGit, repo.root(), repo.root(), false)
                .unwrap()
                .map(|held| (held.name, held.activity)),
            Some(("topic".to_string(), "being rebased"))
        );
    }

    #[test]
    fn in_progress_branch_names_the_branch_a_bisect_started_from() {
        let repo = TestRepo::init();
        repo.write("a.txt", "1\n");
        repo.commit_all("c1");
        assert_eq!(
            in_progress_branch(&RealGit, repo.root(), repo.root(), false).unwrap(),
            None
        );
        repo.git(&["bisect", "start", "HEAD", "HEAD~1"]);
        assert_eq!(
            in_progress_branch(&RealGit, repo.root(), repo.root(), false)
                .unwrap()
                .map(|held| (held.name, held.activity)),
            Some(("main".to_string(), "being bisected"))
        );
        assert!(
            in_progress_branch(
                &RealGit,
                Path::new("/nonexistent/wt"),
                Path::new("/nonexistent/wt"),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn in_progress_branch_errors_when_a_state_file_cannot_be_read() {
        let repo = TestRepo::init();
        let admin = RealGit
            .run(repo.root(), &["rev-parse", "--absolute-git-dir"])
            .unwrap();
        let rebase = Path::new(admin.trim()).join("rebase-merge");
        std::fs::create_dir(&rebase).unwrap();
        // No `head-name` yet: nothing is held.
        assert_eq!(
            in_progress_branch(&RealGit, repo.root(), repo.root(), false).unwrap(),
            None
        );
        // A directory where the file should be cannot be read as one.
        std::fs::create_dir(rebase.join("head-name")).unwrap();
        assert!(in_progress_branch(&RealGit, repo.root(), repo.root(), false).is_err());
        std::fs::remove_dir(rebase.join("head-name")).unwrap();
        // Nor can a name that is not UTF-8.
        std::fs::write(rebase.join("head-name"), b"refs/heads/\xff\xfe").unwrap();
        assert!(in_progress_branch(&RealGit, repo.root(), repo.root(), false).is_err());
    }

    #[test]
    fn a_missing_worktree_state_is_read_through_the_repository() {
        let repo = TestRepo::init();
        repo.add_worktree("gone", "../wt-gone");
        repo.add_worktree("other", "../wt-other");
        let linked = repo.root().parent().unwrap().join("wt-gone");
        let admin = RealGit
            .run(&linked, &["rev-parse", "--absolute-git-dir"])
            .unwrap();
        let admin = PathBuf::from(admin.trim());
        std::fs::remove_dir_all(&linked).unwrap();
        // Git cannot be asked from inside a directory that is gone.
        assert!(in_progress_op(&RealGit, repo.root(), &linked, false).is_err());
        assert_eq!(
            in_progress_op(&RealGit, repo.root(), &linked, true).unwrap(),
            None
        );
        std::fs::create_dir(admin.join("rebase-merge")).unwrap();
        std::fs::write(admin.join("rebase-merge/head-name"), "refs/heads/gone\n").unwrap();
        assert_eq!(
            in_progress_op(&RealGit, repo.root(), &linked, true).unwrap(),
            Some("rebase")
        );
        assert_eq!(
            in_progress_branch(&RealGit, repo.root(), &linked, true)
                .unwrap()
                .map(|held| held.name)
                .as_deref(),
            Some("gone")
        );
        // A path no admin entry records cannot be resolved.
        let stranger = repo.root().parent().unwrap().join("wt-stranger");
        assert!(in_progress_op(&RealGit, repo.root(), &stranger, true).is_err());
    }

    #[test]
    fn normalize_resolves_dot_components_lexically() {
        assert_eq!(
            normalize(Path::new("/r/.git/worktrees/x/../../../wt/./.git")),
            PathBuf::from("/r/wt/.git")
        );
    }

    #[test]
    fn in_progress_op_errors_outside_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        assert!(in_progress_op(&RealGit, dir.path(), dir.path(), false).is_err());
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
