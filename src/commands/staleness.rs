//! Pre-flight staleness check for `wt new` (issue #56): detect when the base a
//! new branch would fork from is behind its origin/upstream counterpart, and (on
//! the user's request) fast-forward the local base before forking.
//!
//! Both the CLI (`wt new`) and the TUI create flow use [`check_base_behind`] for
//! detection and [`fast_forward_base`] for the "update" action.

use std::path::Path;

use crate::commands::checkout::remote_configured;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{ahead_behind, branch_ref, is_ancestor, ops, resolve_hex, upstream_of};
use crate::worktree_service::enumerate_worktrees;

/// A base branch found to be behind its upstream (issue #56).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StaleBase {
    /// How many commits the base is behind its upstream.
    pub(crate) behind: u32,
    /// The remote-tracking ref, e.g. `refs/remotes/origin/main`.
    pub(crate) tracking_ref: String,
    /// The upstream display name, e.g. `origin/main`.
    pub(crate) upstream_display: String,
    /// Whether the base has no local-only commits, so it can be fast-forwarded.
    /// When false the base has diverged and "update" would fail.
    pub(crate) can_fast_forward: bool,
}

/// Whether `base` (a local branch) is behind its origin/upstream counterpart, for
/// the create pre-flight (issue #56). Offline-friendly: returns `Ok(None)` when
/// `base` is not a local branch, has no (or a gone) upstream, or its upstream is
/// not ahead. When a remote is configured it best-effort fetches first so the
/// comparison sees the latest; a failed fetch is non-fatal (the check proceeds
/// against the already-known tracking ref).
pub(crate) fn check_base_behind(
    cx: &mut Cx,
    git: &dyn GitCli,
    repo: &Repo,
    dir: &Path,
    base: &str,
) -> Result<Option<StaleBase>> {
    // Only a local branch has a base to fall behind; a raw ref like `origin/main`
    // or `HEAD` is not something this tracks or updates.
    if resolve_hex(repo.gix(), &branch_ref(base)).is_none() {
        return Ok(None);
    }
    let Some(up) = upstream_of(repo.gix(), base) else {
        return Ok(None);
    };
    if up.is_gone {
        return Ok(None);
    }
    // The remote name is the first segment of the display form (`<remote>/<branch>`).
    let remote = up.display.split('/').next().unwrap_or("origin").to_string();
    // Best-effort fetch so the comparison sees the latest origin (skipped when no
    // remote is configured / offline); a failure is non-fatal.
    if remote_configured(repo.gix(), &remote)
        && let Err(e) = ops::fetch(git, dir, &remote)
    {
        let _ = cx
            .err
            .line(&format!("warning: failed to fetch {remote}: {e}"));
    }
    let (ahead, behind) = ahead_behind(git, dir, &up.tracking_ref, &branch_ref(base))?;
    if behind == 0 {
        return Ok(None);
    }
    Ok(Some(StaleBase {
        behind,
        tracking_ref: up.tracking_ref,
        upstream_display: up.display,
        can_fast_forward: ahead == 0,
    }))
}

/// Fast-forwards the local `base` branch to `stale.tracking_ref` (issue #56, the
/// "update" action). Errors when the base has diverged (cannot fast-forward). If
/// the base is checked out in a worktree, runs `git merge --ff-only` there so the
/// working tree follows; otherwise moves the ref with `git branch -f`.
pub(crate) fn fast_forward_base(
    cx: &mut Cx,
    git: &dyn GitCli,
    repo: &Repo,
    root: &Path,
    base: &str,
    stale: &StaleBase,
) -> Result<()> {
    if !is_ancestor(git, root, &branch_ref(base), &stale.tracking_ref) {
        return Err(Error::operation(format!(
            "base {base:?} has diverged from {}; cannot fast-forward",
            stale.upstream_display
        )));
    }
    let checked_out = enumerate_worktrees(repo, git)?
        .into_iter()
        .find(|w| w.branch.as_deref() == Some(base))
        .map(|w| w.path);
    if let Some(path) = checked_out {
        ops::merge_ff_only(git, &path, &stale.tracking_ref)?;
    } else {
        ops::set_branch_ref(git, root, base, &stale.tracking_ref)?;
    }
    let _ = cx.err.line(&format!(
        "updated {base} to {} (fast-forward)",
        stale.upstream_display
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RealGit;
    use crate::git::discover::Repo;
    use crate::testutil::{TestRepo, test_cx};

    /// Runs `check_base_behind` against the repo's primary worktree.
    fn check(repo: &TestRepo, base: &str) -> Option<StaleBase> {
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let r = Repo::discover(repo.root()).unwrap();
        super::check_base_behind(&mut t.cx, &RealGit, &r, repo.root(), base).unwrap()
    }

    /// Records `origin/<base>` one commit ahead of local `<base>` and configures
    /// the upstream, without a fetchable remote (so the check's fetch is skipped).
    fn behind_with_upstream(repo: &TestRepo, base: &str) {
        let c1 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("a.txt", "1\n");
        repo.commit_all("c2");
        let c2 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", &format!("refs/remotes/origin/{base}"), &c2]);
        repo.git(&["reset", "-q", "--hard", &c1]);
        repo.git(&["config", &format!("branch.{base}.remote"), "origin"]);
        repo.git(&[
            "config",
            &format!("branch.{base}.merge"),
            &format!("refs/heads/{base}"),
        ]);
    }

    #[test]
    fn detects_base_behind_upstream() {
        let repo = TestRepo::init();
        behind_with_upstream(&repo, "main");
        let stale = check(&repo, "main").expect("base is behind");
        assert_eq!(stale.behind, 1);
        assert_eq!(stale.upstream_display, "origin/main");
        assert!(stale.can_fast_forward);
    }

    #[test]
    fn no_upstream_is_not_stale() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]); // a local branch with no upstream
        assert!(check(&repo, "topic").is_none());
    }

    #[test]
    fn non_local_base_is_not_stale() {
        let repo = TestRepo::init();
        // Neither a raw remote ref nor HEAD is a local branch with a base.
        assert!(check(&repo, "origin/main").is_none());
        assert!(check(&repo, "HEAD").is_none());
    }

    #[test]
    fn up_to_date_base_is_not_stale() {
        let repo = TestRepo::init();
        repo.git(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"]);
        repo.git(&["config", "branch.main.remote", "origin"]);
        repo.git(&["config", "branch.main.merge", "refs/heads/main"]);
        assert!(check(&repo, "main").is_none());
    }

    #[test]
    fn fetches_then_detects_behind() {
        // With a real remote configured, the check fetches first (covering that
        // branch) and still finds the base behind.
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        let c1 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("a.txt", "1\n");
        repo.commit_all("c2");
        repo.git(&["push", "-q", "origin", "main"]);
        repo.git(&["reset", "-q", "--hard", &c1]);
        let stale = check(&repo, "main").expect("base is behind after fetch");
        assert_eq!(stale.behind, 1);
    }

    #[test]
    fn fast_forward_checked_out_base_updates_working_tree() {
        let repo = TestRepo::init();
        behind_with_upstream(&repo, "main");
        let c2 = repo
            .git(&["rev-parse", "refs/remotes/origin/main"])
            .trim()
            .to_string();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let r = Repo::discover(repo.root()).unwrap();
        let stale = StaleBase {
            behind: 1,
            tracking_ref: "refs/remotes/origin/main".into(),
            upstream_display: "origin/main".into(),
            can_fast_forward: true,
        };
        super::fast_forward_base(&mut t.cx, &RealGit, &r, repo.root(), "main", &stale).unwrap();
        // `main` is checked out at the primary worktree, so it fast-forwards in place.
        assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]).trim(), c2);
    }

    #[test]
    fn fast_forward_non_checked_out_base_moves_ref() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]); // topic at c1, not checked out
        repo.write("a.txt", "1\n");
        repo.commit_all("c2");
        let c2 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/topic", &c2]);
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let r = Repo::discover(repo.root()).unwrap();
        let stale = StaleBase {
            behind: 1,
            tracking_ref: "refs/remotes/origin/topic".into(),
            upstream_display: "origin/topic".into(),
            can_fast_forward: true,
        };
        super::fast_forward_base(&mut t.cx, &RealGit, &r, repo.root(), "topic", &stale).unwrap();
        assert_eq!(repo.git(&["rev-parse", "refs/heads/topic"]).trim(), c2);
    }

    #[test]
    fn fast_forward_refuses_diverged_base() {
        let repo = TestRepo::init();
        let c1 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("a.txt", "1\n");
        repo.commit_all("c2"); // main is now c2
        repo.git(&["update-ref", "refs/remotes/origin/main", &c1]); // origin/main behind
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let r = Repo::discover(repo.root()).unwrap();
        let stale = StaleBase {
            behind: 0,
            tracking_ref: "refs/remotes/origin/main".into(),
            upstream_display: "origin/main".into(),
            can_fast_forward: false,
        };
        let err = super::fast_forward_base(&mut t.cx, &RealGit, &r, repo.root(), "main", &stale)
            .unwrap_err();
        assert!(err.to_string().contains("fast-forward"));
    }
}
