//! Ref and branch reads via `gix` (spec §4): local branch listing, upstream
//! resolution, ref resolution, and default-branch resolution.

use std::path::Path;

use crate::error::{Error, Result};
use crate::git::cli::GitCli;

/// The configured upstream of a local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    /// Display form, e.g. `origin/feature/x`.
    pub display: String,
    /// The remote-tracking ref, e.g. `refs/remotes/origin/feature/x`.
    pub tracking_ref: String,
    /// Whether the tracking ref is gone (configured but no longer present).
    pub is_gone: bool,
}

/// Lists local branch names (without the `refs/heads/` prefix).
pub fn local_branches(repo: &gix::Repository) -> Result<Vec<String>> {
    let platform = repo
        .references()
        .map_err(|e| Error::operation(format!("cannot read references: {e}")))?;
    let iter = platform
        .local_branches()
        .map_err(|e| Error::operation(format!("cannot list branches: {e}")))?;
    let mut names = Vec::new();
    for reference in iter {
        let reference =
            reference.map_err(|e| Error::operation(format!("cannot read branch: {e}")))?;
        names.push(reference.name().shorten().to_string());
    }
    names.sort();
    Ok(names)
}

/// Lists remote-tracking branch names (e.g. `origin/main`), skipping the
/// symbolic `<remote>/HEAD` pointers (which alias a real branch, not a fork
/// candidate). Names keep their remote prefix so they read unambiguously.
pub fn remote_branches(repo: &gix::Repository) -> Result<Vec<String>> {
    let platform = repo
        .references()
        .map_err(|e| Error::operation(format!("cannot read references: {e}")))?;
    let iter = platform
        .remote_branches()
        .map_err(|e| Error::operation(format!("cannot list remote branches: {e}")))?;
    let mut names = Vec::new();
    for reference in iter {
        let reference =
            reference.map_err(|e| Error::operation(format!("cannot read remote branch: {e}")))?;
        let name = reference.name().shorten().to_string();
        // `origin/HEAD` is a symbolic alias for the default branch, not a branch.
        if name.ends_with("/HEAD") {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

/// Lists every branch a new worktree can fork from or check out: local branches
/// first (sorted), then remote-tracking branches (sorted). Best-effort — used to
/// populate the TUI create-prompt options dropdown (issue #25).
pub fn all_branches(repo: &gix::Repository) -> Result<Vec<String>> {
    let mut names = local_branches(repo)?;
    names.extend(remote_branches(repo)?);
    Ok(names)
}

/// The fully-qualified ref of a local branch, i.e. `refs/heads/<branch>`.
///
/// Centralizes the single spelling of this path that the rest of the crate
/// builds whenever it needs a branch's full ref — for `gix` rev-parsing,
/// `git merge-base`, `git branch -f`, and so on.
pub fn branch_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

/// Validates a user-entered branch name as a legal git branch ref
/// (`git check-ref-format --branch` semantics), returning a human-readable
/// reason on failure. The name is validated as the full ref `refs/heads/<name>`,
/// so single-component lowercase names like `feature` are accepted while
/// illegal forms (`feat..x`, `a b`, `*x`, `.hidden`, `x.lock`, `HEAD`, …) are
/// rejected.
pub fn validate_branch_name(name: &str) -> std::result::Result<(), String> {
    use gix::bstr::ByteSlice;
    let full = branch_ref(name);
    gix::validate::reference::branch_name(full.as_bytes().as_bstr())
        .map(|_| ())
        .map_err(|e| format!("invalid branch name: {e}"))
}

/// Resolves a revspec to an object id (hex), or `None` if it does not resolve.
pub fn resolve_hex(repo: &gix::Repository, spec: &str) -> Option<String> {
    repo.rev_parse_single(spec)
        .ok()
        .map(|id| id.detach().to_string())
}

/// Resolves the configured upstream of `branch`, or `None` if none is set.
pub fn upstream_of(repo: &gix::Repository, branch: &str) -> Option<Upstream> {
    let config = repo.config_snapshot();
    let remote = config.string(format!("branch.{branch}.remote").as_str())?;
    let merge = config.string(format!("branch.{branch}.merge").as_str())?;
    let remote = remote.to_string();
    let merge = merge.to_string();
    let merge_branch = merge.strip_prefix("refs/heads/").unwrap_or(&merge);
    let display = format!("{remote}/{merge_branch}");
    let tracking_ref = format!("refs/remotes/{remote}/{merge_branch}");
    let is_gone = resolve_hex(repo, &tracking_ref).is_none();
    Some(Upstream {
        display,
        tracking_ref,
        is_gone,
    })
}

/// Whether commit-ish `a` is an ancestor of `b` (i.e. `a` is fully merged into
/// `b`), determined offline via `git merge-base --is-ancestor`. Returns `false`
/// if a ref is missing or the command errors.
pub fn is_ancestor(git: &dyn GitCli, root: &Path, a: &str, b: &str) -> bool {
    git.run_raw(root, &["merge-base", "--is-ancestor", a, b])
        .map(|o| o.success)
        .unwrap_or(false)
}

/// Resolves the repository's default branch (spec §7): the `origin/HEAD` target,
/// falling back to the current branch. (`init.defaultBranch` is deliberately not
/// consulted — it governs *new* repositories, not an existing repo's default.)
pub fn default_branch(repo: &gix::Repository) -> Option<String> {
    origin_head_branch(repo).or_else(|| current_branch(repo))
}

/// The current branch name, or `None` for a detached HEAD or unborn branch.
pub fn current_branch(repo: &gix::Repository) -> Option<String> {
    let head = repo.head().ok()?;
    head.referent_name().map(|name| name.shorten().to_string())
}

/// The branch that `refs/remotes/origin/HEAD` points to, if any.
fn origin_head_branch(repo: &gix::Repository) -> Option<String> {
    let reference = repo.find_reference("refs/remotes/origin/HEAD").ok()?;
    match reference.target() {
        gix::refs::TargetRef::Symbolic(name) => {
            // e.g. refs/remotes/origin/main -> main (handles slashes in names).
            let full = name.as_bstr().to_string();
            let rest = full.strip_prefix("refs/remotes/")?;
            rest.split_once('/').map(|(_, branch)| branch.to_string())
        }
        gix::refs::TargetRef::Object(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::discover::Repo;
    use crate::testutil::TestRepo;

    #[test]
    fn lists_local_branches_sorted() {
        let repo = TestRepo::init();
        repo.git(&["branch", "zeta"]);
        repo.git(&["branch", "alpha"]);
        let r = Repo::discover(repo.root()).unwrap();
        let branches = local_branches(r.gix()).unwrap();
        assert_eq!(branches, vec!["alpha", "main", "zeta"]);
    }

    #[test]
    fn lists_remote_branches_skipping_head() {
        let repo = TestRepo::init();
        let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &head]);
        repo.git(&["update-ref", "refs/remotes/origin/feature/x", &head]);
        // The symbolic origin/HEAD alias must be excluded.
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        let r = Repo::discover(repo.root()).unwrap();
        let remotes = remote_branches(r.gix()).unwrap();
        assert_eq!(remotes, vec!["origin/feature/x", "origin/main"]);
    }

    #[test]
    fn all_branches_lists_locals_then_remotes() {
        let repo = TestRepo::init();
        repo.git(&["branch", "zeta"]);
        let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &head]);
        let r = Repo::discover(repo.root()).unwrap();
        let all = all_branches(r.gix()).unwrap();
        // Local branches (sorted) first, then remote-tracking branches (sorted).
        assert_eq!(all, vec!["main", "zeta", "origin/main"]);
    }

    #[test]
    fn validates_branch_names() {
        // Legal branch names, including slashes, dashes, underscores, digits.
        for ok in ["feature", "feature/x", "fix-bug_123", "release/v1.2"] {
            assert!(
                validate_branch_name(ok).is_ok(),
                "expected {ok:?} to be valid"
            );
        }
        // Illegal forms are rejected with a reason.
        for bad in [
            "feat..x", "a b", "*x", ".hidden", "feature/", "x.lock", "HEAD", "",
        ] {
            let err = validate_branch_name(bad).unwrap_err();
            assert!(
                err.starts_with("invalid branch name:"),
                "expected {bad:?} to be rejected, got {err:?}"
            );
        }
    }

    #[test]
    fn branch_ref_prefixes_refs_heads() {
        assert_eq!(branch_ref("main"), "refs/heads/main");
        // Slashes in the branch name are preserved, not escaped.
        assert_eq!(branch_ref("feature/login"), "refs/heads/feature/login");
    }

    #[test]
    fn resolves_refs() {
        let repo = TestRepo::init();
        let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        let r = Repo::discover(repo.root()).unwrap();
        assert_eq!(resolve_hex(r.gix(), "HEAD").as_deref(), Some(head.as_str()));
        assert_eq!(
            resolve_hex(r.gix(), "refs/heads/main").as_deref(),
            Some(head.as_str())
        );
        assert!(resolve_hex(r.gix(), "refs/heads/nope").is_none());
    }

    #[test]
    fn upstream_present_absent_and_gone() {
        let repo = TestRepo::init();
        let r = Repo::discover(repo.root()).unwrap();
        // No upstream configured.
        assert!(upstream_of(r.gix(), "main").is_none());

        // Configure an upstream with a present tracking ref.
        let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &head]);
        repo.git(&["config", "branch.main.remote", "origin"]);
        repo.git(&["config", "branch.main.merge", "refs/heads/main"]);
        let r = Repo::discover(repo.root()).unwrap();
        let up = upstream_of(r.gix(), "main").unwrap();
        assert_eq!(up.display, "origin/main");
        assert_eq!(up.tracking_ref, "refs/remotes/origin/main");
        assert!(!up.is_gone);

        // Delete the tracking ref -> gone.
        repo.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
        let r = Repo::discover(repo.root()).unwrap();
        let up = upstream_of(r.gix(), "main").unwrap();
        assert!(up.is_gone);
    }

    #[test]
    fn default_branch_prefers_origin_head() {
        let repo = TestRepo::init();
        let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &head]);
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        let r = Repo::discover(repo.root()).unwrap();
        assert_eq!(default_branch(r.gix()).as_deref(), Some("main"));
    }

    #[test]
    fn default_branch_falls_back_to_current() {
        let repo = TestRepo::init();
        let r = Repo::discover(repo.root()).unwrap();
        assert_eq!(default_branch(r.gix()).as_deref(), Some("main"));
        assert_eq!(current_branch(r.gix()).as_deref(), Some("main"));
    }

    #[test]
    fn is_ancestor_true_when_merged_false_when_divergent() {
        use crate::git::cli::RealGit;
        let repo = TestRepo::init();
        // `topic` branches off main with no extra commits: an ancestor of main.
        repo.git(&["branch", "topic"]);
        assert!(is_ancestor(
            &RealGit,
            repo.root(),
            "refs/heads/topic",
            "refs/heads/main"
        ));
        // Add a commit on `topic` so it diverges: no longer an ancestor of main.
        repo.git(&["checkout", "topic"]);
        repo.write("t.txt", "1\n");
        repo.commit_all("topic work");
        assert!(!is_ancestor(
            &RealGit,
            repo.root(),
            "refs/heads/topic",
            "refs/heads/main"
        ));
        // ...but main is still an ancestor of topic.
        assert!(is_ancestor(
            &RealGit,
            repo.root(),
            "refs/heads/main",
            "refs/heads/topic"
        ));
    }

    #[test]
    fn is_ancestor_false_for_missing_ref() {
        use crate::git::cli::RealGit;
        let repo = TestRepo::init();
        assert!(!is_ancestor(
            &RealGit,
            repo.root(),
            "refs/heads/nope",
            "refs/heads/main"
        ));
    }
}
