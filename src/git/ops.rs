//! Thin wrappers over the [`GitCli`] seam for the state-mutating `git` operations
//! the commands share (spec §4). Each function owns the exact argv and the
//! `-d`/`-D` flag selection for its operation, so a command reads as a verb
//! (`worktree_remove(..)`) rather than a hand-built argument vector.
//!
//! These wrappers deliberately impose **no** error policy: they preserve the
//! underlying `run` vs `run_raw` semantics and hand the result straight back, so
//! each caller keeps its own interpretation — a failed `git branch -d` is a
//! sentinel the TUI re-prompts on (issue #53), a failed best-effort cleanup is
//! logged and ignored, a checked mutation propagates its [`Error`](crate::error).

use std::path::Path;

use crate::error::Result;
use crate::git::cli::{GitCli, GitOutput};

/// Adds a worktree for the existing `branch` at `target` (`git worktree add`).
pub(crate) fn worktree_add(
    git: &dyn GitCli,
    root: &Path,
    target: &str,
    branch: &str,
) -> Result<String> {
    git.run(root, &["worktree", "add", target, branch])
}

/// Adds a worktree, creating `branch` from `start` (`git worktree add -b`).
/// `no_track` adds `--no-track` so the new branch does not inherit `start` as its
/// upstream (issue #43).
pub(crate) fn worktree_add_branch(
    git: &dyn GitCli,
    root: &Path,
    branch: &str,
    target: &str,
    start: &str,
    no_track: bool,
) -> Result<String> {
    let mut argv = vec!["worktree", "add"];
    if no_track {
        argv.push("--no-track");
    }
    argv.extend(["-b", branch, target, start]);
    git.run(root, &argv)
}

/// Removes the worktree at `path` (`git worktree remove`), forcing past a dirty
/// or unpushed tree when `force`.
pub(crate) fn worktree_remove(
    git: &dyn GitCli,
    root: &Path,
    path: &str,
    force: bool,
) -> Result<String> {
    let mut argv = vec!["worktree", "remove"];
    if force {
        argv.push("--force");
    }
    argv.push(path);
    git.run(root, &argv)
}

/// Reconciles git's worktree admin metadata (`git worktree prune`).
pub(crate) fn worktree_prune(git: &dyn GitCli, root: &Path) -> Result<String> {
    git.run(root, &["worktree", "prune"])
}

/// Deletes the local `branch` (`git branch -d`, or `-D` when `force`). Returns the
/// raw [`GitOutput`] so the caller can interpret a refusal — a safe delete of an
/// unmerged branch fails with a "not fully merged" stderr the TUI keys on to offer
/// a force-delete (issue #53).
pub(crate) fn delete_branch(
    git: &dyn GitCli,
    root: &Path,
    branch: &str,
    force: bool,
) -> Result<GitOutput> {
    let flag = if force { "-D" } else { "-d" };
    git.run_raw(root, &["branch", flag, branch])
}

/// Points `branch` at `to` without checking it out (`git branch -f`).
pub(crate) fn set_branch_ref(
    git: &dyn GitCli,
    root: &Path,
    branch: &str,
    to: &str,
) -> Result<String> {
    git.run(root, &["branch", "-f", branch, to])
}

/// Sets `branch`'s upstream to `upstream` (`git branch -u`).
pub(crate) fn set_upstream(
    git: &dyn GitCli,
    root: &Path,
    branch: &str,
    upstream: &str,
) -> Result<String> {
    git.run(root, &["branch", "-u", upstream, branch])
}

/// Fetches `remote` (`git fetch <remote>`), returning the raw [`GitOutput`] so the
/// caller can treat a failure as non-fatal (an offline best-effort sync).
pub(crate) fn fetch(git: &dyn GitCli, dir: &Path, remote: &str) -> Result<GitOutput> {
    git.run_raw(dir, &["fetch", remote])
}

/// Fetches a single `refspec` from `remote` (`git fetch <remote> <refspec>`).
pub(crate) fn fetch_refspec(
    git: &dyn GitCli,
    dir: &Path,
    remote: &str,
    refspec: &str,
) -> Result<String> {
    git.run(dir, &["fetch", remote, refspec])
}

/// Fast-forwards the branch checked out at `dir` to `tracking_ref`
/// (`git merge --ff-only`). Errors if the merge is not a fast-forward.
pub(crate) fn merge_ff_only(git: &dyn GitCli, dir: &Path, tracking_ref: &str) -> Result<String> {
    git.run(dir, &["merge", "--ff-only", tracking_ref])
}

/// Pushes `branch` to `remote` (`git push <remote> <branch>`), returning the raw
/// [`GitOutput`] so the caller can interpret a rejected (non-fast-forward) push
/// as a sentinel rather than a hard error. Never force-pushes, and always pushes
/// the named branch explicitly (independent of `push.default`).
pub(crate) fn push(git: &dyn GitCli, dir: &Path, remote: &str, branch: &str) -> Result<GitOutput> {
    git.run_raw(dir, &["push", remote, branch])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    /// A [`GitCli`] that records every argv it is asked to run and returns a fixed
    /// outcome, so a test can assert the exact command a wrapper builds.
    struct RecordingGit {
        calls: RefCell<Vec<Vec<String>>>,
        output: GitOutput,
    }

    impl RecordingGit {
        fn new() -> Self {
            RecordingGit {
                calls: RefCell::new(Vec::new()),
                output: GitOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                },
            }
        }

        /// The argv of the most recent call.
        fn last(&self) -> Vec<String> {
            self.calls.borrow().last().cloned().unwrap_or_default()
        }
    }

    impl GitCli for RecordingGit {
        fn run_raw(&self, _repo: &Path, args: &[&str]) -> Result<GitOutput> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            Ok(self.output.clone())
        }
    }

    fn root() -> PathBuf {
        PathBuf::from("/r")
    }

    #[test]
    fn worktree_add_builds_plain_add() {
        let git = RecordingGit::new();
        worktree_add(&git, &root(), "/wt/x", "topic").unwrap();
        assert_eq!(git.last(), ["worktree", "add", "/wt/x", "topic"]);
    }

    #[test]
    fn worktree_add_branch_adds_no_track_flag_only_when_asked() {
        let git = RecordingGit::new();
        worktree_add_branch(&git, &root(), "topic", "/wt/x", "main", true).unwrap();
        assert_eq!(
            git.last(),
            [
                "worktree",
                "add",
                "--no-track",
                "-b",
                "topic",
                "/wt/x",
                "main"
            ]
        );
        worktree_add_branch(&git, &root(), "topic", "/wt/x", "FETCH_HEAD", false).unwrap();
        assert_eq!(
            git.last(),
            ["worktree", "add", "-b", "topic", "/wt/x", "FETCH_HEAD"]
        );
    }

    #[test]
    fn worktree_remove_adds_force_flag_only_when_asked() {
        let git = RecordingGit::new();
        worktree_remove(&git, &root(), "/wt/x", false).unwrap();
        assert_eq!(git.last(), ["worktree", "remove", "/wt/x"]);
        worktree_remove(&git, &root(), "/wt/x", true).unwrap();
        assert_eq!(git.last(), ["worktree", "remove", "--force", "/wt/x"]);
    }

    #[test]
    fn worktree_prune_builds_prune() {
        let git = RecordingGit::new();
        worktree_prune(&git, &root()).unwrap();
        assert_eq!(git.last(), ["worktree", "prune"]);
    }

    #[test]
    fn delete_branch_selects_capital_d_only_under_force() {
        let git = RecordingGit::new();
        delete_branch(&git, &root(), "topic", false).unwrap();
        assert_eq!(git.last(), ["branch", "-d", "topic"]);
        delete_branch(&git, &root(), "topic", true).unwrap();
        assert_eq!(git.last(), ["branch", "-D", "topic"]);
    }

    #[test]
    fn set_branch_ref_forces_the_named_branch() {
        let git = RecordingGit::new();
        set_branch_ref(&git, &root(), "main", "refs/remotes/origin/main").unwrap();
        assert_eq!(
            git.last(),
            ["branch", "-f", "main", "refs/remotes/origin/main"]
        );
    }

    #[test]
    fn set_upstream_passes_upstream_before_branch() {
        let git = RecordingGit::new();
        set_upstream(&git, &root(), "feat", "origin/main").unwrap();
        assert_eq!(git.last(), ["branch", "-u", "origin/main", "feat"]);
    }

    #[test]
    fn fetch_builds_bare_fetch() {
        let git = RecordingGit::new();
        fetch(&git, &root(), "origin").unwrap();
        assert_eq!(git.last(), ["fetch", "origin"]);
    }

    #[test]
    fn fetch_refspec_appends_the_refspec() {
        let git = RecordingGit::new();
        fetch_refspec(&git, &root(), "origin", "pull/7/head").unwrap();
        assert_eq!(git.last(), ["fetch", "origin", "pull/7/head"]);
    }

    #[test]
    fn merge_ff_only_builds_fast_forward_merge() {
        let git = RecordingGit::new();
        merge_ff_only(&git, &root(), "refs/remotes/origin/main").unwrap();
        assert_eq!(
            git.last(),
            ["merge", "--ff-only", "refs/remotes/origin/main"]
        );
    }

    #[test]
    fn push_builds_explicit_remote_branch_push() {
        let git = RecordingGit::new();
        push(&git, &root(), "origin", "feature/x").unwrap();
        assert_eq!(git.last(), ["push", "origin", "feature/x"]);
    }
}
