//! Content-level merge detection: whether every change on a commit is already in
//! a target branch even when its SHAs are not — a squash merge, a rebase merge,
//! work split across several pull requests, or a branch of merge commits whose
//! parents all landed. `wt prune` falls back to this when ancestry says "not
//! merged".
//!
//! The check fails safe: an error, a conflict, or an older `git` reads as "not
//! merged", so a failed check can never be the reason work is deleted.

use std::path::Path;

use crate::git::cli::GitCli;

/// Whether every change `tip` introduces is already in `target`, run in `dir`:
/// merging `tip` into `target` would leave `target`'s tree unchanged
/// (`git merge-tree --write-tree`, git ≥ 2.38). Meant for a `tip` that is *not*
/// an ancestor of `target` — ancestry is the cheaper, stronger test and callers
/// try it first.
///
/// This is a tree test, not a patch test, on purpose: a patch-equivalence check
/// (`git cherry`) still matches a change the target later reverted, which would
/// make reverted work look safe to delete. The cost is that a change the target
/// has since edited on the same lines conflicts and reads as "not merged" — the
/// safe direction to be wrong in.
///
/// A branch whose final tree equals its merge base's — nothing but empty
/// commits, or work it added and then backed out itself — also merges as a
/// no-op, yet its commits may hold work found nowhere else. It reads as "not
/// merged", so only a branch that actually changes something can be judged
/// merged by content.
///
/// `merge-tree` writes the merged tree to the object store; it creates no ref,
/// so the object is garbage-collected like any other unreachable one. A conflict,
/// unrelated histories, or a `git` without `merge-tree --write-tree` returns
/// `false`.
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn is_content_merged(git: &dyn GitCli, dir: &Path, tip: &str, target: &str) -> bool {
    let target_tree = format!("{target}^{{tree}}");
    let Ok(expected) = git.run(dir, &["rev-parse", "--verify", "--quiet", &target_tree]) else {
        return false;
    };
    if !changes_anything(git, dir, tip, target) {
        return false;
    }
    match git.run_raw(
        dir,
        &["merge-tree", "--write-tree", "--no-messages", target, tip],
    ) {
        Ok(out) if out.success => out.stdout.lines().next().map(str::trim) == Some(expected.trim()),
        Ok(out) => {
            // Exit 1 is a conflict; anything else (an old git, unrelated
            // histories, an unknown ref) is logged so a surprising "not merged"
            // is traceable.
            tracing::debug!(tip, target, stderr = %out.stderr.trim(), "merge-tree: not a clean no-op");
            false
        }
        Err(error) => {
            tracing::debug!(tip, target, %error, "merge-tree could not run");
            false
        }
    }
}

/// Whether `tip`'s tree differs from the tree of its merge base with `target`
/// — i.e. the branch's net effect is not empty. Any failure (no merge base
/// included) reads as `false`, which the caller treats as "not merged".
fn changes_anything(git: &dyn GitCli, dir: &Path, tip: &str, target: &str) -> bool {
    let Ok(base) = git.run(dir, &["merge-base", target, tip]) else {
        return false;
    };
    let tree_of = |rev: &str| {
        git.run(
            dir,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{rev}^{{tree}}"),
            ],
        )
        .ok()
        .map(|t| t.trim().to_string())
    };
    match (tree_of(base.trim()), tree_of(tip)) {
        (Some(base_tree), Some(tip_tree)) => base_tree != tip_tree,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    fn merged(repo: &TestRepo, tip: &str) -> bool {
        is_content_merged(&RealGit, repo.root(), tip, "refs/heads/main")
    }

    /// Creates `name` off the current `main`, commits `files` on it (one commit
    /// per file), and returns to `main`.
    fn topic(repo: &TestRepo, name: &str, files: &[(&str, &str)]) {
        repo.git(&["checkout", "-q", "-b", name]);
        for (path, content) in files {
            repo.write(path, content);
            repo.commit_all(&format!("add {path}"));
        }
        repo.git(&["checkout", "-q", "main"]);
    }

    /// Squash-merges `name` into `main` as one new commit.
    fn squash(repo: &TestRepo, name: &str) {
        repo.git(&["merge", "-q", "--squash", name]);
        repo.git(&["commit", "-q", "-m", &format!("squash {name}")]);
    }

    #[test]
    fn a_squash_merged_branch_is_merged() {
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        assert!(!merged(&repo, "feat"));
        squash(&repo, "feat");
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn a_rebase_merged_branch_is_merged() {
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        repo.git(&["cherry-pick", "main..feat"]);
        assert!(merged(&repo, "feat"));
    }

    #[test]
    fn work_split_across_several_squashes_is_merged() {
        // The branch's two changes reach main through two separate "PRs".
        let repo = TestRepo::init();
        topic(&repo, "combo", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        topic(&repo, "part-a", &[("a.txt", "a\n")]);
        squash(&repo, "part-a");
        assert!(!merged(&repo, "combo"));
        topic(&repo, "part-b", &[("b.txt", "b\n")]);
        squash(&repo, "part-b");
        assert!(merged(&repo, "combo"));
    }

    #[test]
    fn a_merge_only_branch_whose_parents_landed_is_merged() {
        // An integration branch: nothing but merges of branches main later took.
        let repo = TestRepo::init();
        topic(&repo, "x", &[("x.txt", "x\n")]);
        topic(&repo, "y", &[("y.txt", "y\n")]);
        repo.git(&["checkout", "-q", "-b", "integration"]);
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge x", "x"]);
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge y", "y"]);
        repo.git(&["checkout", "-q", "main"]);
        assert!(!merged(&repo, "integration"));
        squash(&repo, "x");
        squash(&repo, "y");
        assert!(merged(&repo, "integration"));
    }

    #[test]
    fn a_change_main_reverted_is_not_merged() {
        // The patch is still in main's history, but its effect is not.
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n")]);
        squash(&repo, "feat");
        repo.git(&["revert", "--no-edit", "HEAD"]);
        assert!(!merged(&repo, "feat"));
    }

    #[test]
    fn a_change_main_edited_since_is_not_merged() {
        // Conservative by design: main took the change, then rewrote the line.
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "one\n")]);
        squash(&repo, "feat");
        repo.write("a.txt", "two\n");
        repo.commit_all("main edits a");
        assert!(!merged(&repo, "feat"));
    }

    #[test]
    fn a_branch_with_unlanded_work_is_not_merged() {
        let repo = TestRepo::init();
        topic(&repo, "feat", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        repo.git(&["cherry-pick", "feat~1"]); // only the first commit lands
        assert!(!merged(&repo, "feat"));
    }

    #[test]
    fn a_branch_that_backed_out_its_own_work_is_not_merged() {
        // Its net effect is empty, so merging it changes nothing — but the
        // added file lives only in its history.
        let repo = TestRepo::init();
        topic(&repo, "exp", &[("s.txt", "big work\n")]);
        repo.git(&["checkout", "-q", "exp"]);
        repo.git(&["rm", "-q", "s.txt"]);
        repo.git(&["commit", "-q", "-m", "move away"]);
        repo.git(&["checkout", "-q", "main"]);
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        assert!(!merged(&repo, "exp"));
    }

    #[test]
    fn a_branch_of_empty_commits_is_not_merged() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "empty"]);
        repo.git(&["commit", "-q", "--allow-empty", "-m", "note to self"]);
        repo.git(&["checkout", "-q", "main"]);
        repo.write("other.txt", "o\n");
        repo.commit_all("main moves on");
        assert!(!merged(&repo, "empty"));
    }

    #[test]
    fn unrelated_histories_are_not_merged() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "--orphan", "stray"]);
        repo.write("stray.txt", "s\n");
        repo.commit_all("unrelated root");
        repo.git(&["checkout", "-q", "main"]);
        assert!(!merged(&repo, "stray"));
    }

    #[test]
    fn an_unknown_ref_is_not_merged() {
        let repo = TestRepo::init();
        assert!(!merged(&repo, "no-such-branch"));
        assert!(!is_content_merged(
            &RealGit,
            repo.root(),
            "main",
            "refs/heads/no-such-target"
        ));
    }
}
