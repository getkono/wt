//! `wt checkout <branch>` (alias `co`) — switch the branch checked out in a
//! worktree *in place*, syncing it with origin (issue #32).
//!
//! Unlike `wt new` (which creates a worktree) this changes the HEAD of an
//! existing worktree: it fetches the remote, checks out `branch` (git DWIM
//! creates a local tracking branch from `origin/<branch>` when the branch is
//! remote-only), then fast-forwards the branch to its upstream when it is
//! strictly behind. A diverged branch is never rewritten — it is left as-is with
//! a warning. The core logic is shared with the TUI (`tui::runtime`).

use std::path::Path;

use crate::cli::CheckoutArgs;
use crate::commands::{Session, emit_worktree, maybe_init_submodules, open_session, same_path};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{
    branch_ref, is_ancestor, ops, remote_branches, resolve_hex, status_of, upstream_of,
    validate_branch_name,
};
use crate::worktree_service::enumerate_worktrees;

/// What the post-checkout origin sync did, for the caller's messaging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncOutcome {
    /// The branch was already up to date with its upstream (or has none).
    UpToDate,
    /// The local branch was fast-forwarded to its upstream.
    FastForwarded,
    /// The local branch has diverged from its upstream; left as-is (warned).
    Diverged,
    /// No remote was configured / the fetch failed; checked out without syncing.
    FetchSkipped,
}

/// Switches the current worktree to `branch`, syncing it with origin, then emits
/// the worktree path so the shell wrapper `cd`s into it (a no-op for the common
/// current-worktree case; the right behavior when `-C` targeted another).
pub(crate) fn run(cx: &mut Cx, args: &CheckoutArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let worktree_dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    let outcome = checkout_branch_in_worktree(
        cx,
        git,
        &session,
        &worktree_dir,
        &args.branch,
        args.force,
        args.submodule_override(),
    )?;
    log_sync_outcome(cx, &args.branch, outcome);
    emit_worktree(
        cx,
        &worktree_dir,
        json,
        args.no_switch,
        &format!("checked out {} in", args.branch),
    )
}

/// Switches `worktree_dir` to `branch` in place and syncs it with origin
/// (fetch + fast-forward). Emits no stdout; a divergence/fetch warning is written
/// to stderr here. Returns what the sync did so the caller can phrase its result.
pub(crate) fn checkout_branch_in_worktree(
    cx: &mut Cx,
    git: &dyn GitCli,
    session: &Session,
    worktree_dir: &Path,
    branch: &str,
    force: bool,
    submodule_override: Option<bool>,
) -> Result<SyncOutcome> {
    let repo = &session.repo;
    let remote = session.config.pr_default_remote.clone();

    // Validate the branch name before touching git (usage error, exit 2).
    if let Err(msg) = validate_branch_name(branch) {
        return Err(Error::usage(msg));
    }

    // Normalize a `<remote>/<name>` selection to the short name so git DWIM
    // creates a local tracking branch (a literal remote ref would detach HEAD).
    let branch_owned = normalize_remote_branch(repo, branch);
    let branch = branch_owned.as_str();

    ensure_branch_available(git, repo, worktree_dir, branch, force)?;
    let fetch_skipped = fetch_remote_best_effort(cx, git, worktree_dir, &remote);
    ensure_branch_exists(git, repo, worktree_dir, branch, &remote)?;

    // Check out the branch (DWIM creates a local tracking branch when remote-only).
    let mut argv: Vec<&str> = vec!["checkout"];
    if force {
        argv.push("--force");
    }
    argv.push(branch);
    git.run(worktree_dir, &argv)?;

    let outcome = sync_with_upstream(cx, git, worktree_dir, branch, fetch_skipped)?;
    // Switching can introduce new submodule definitions (issue #50); initialize
    // them when the policy (or `--init-submodules`) asks. Non-fatal.
    maybe_init_submodules(
        cx,
        git,
        worktree_dir,
        session.config.submodules_init,
        submodule_override,
    )?;
    Ok(outcome)
}

/// Pre-checkout safety guards: refuse if `branch` is already checked out in
/// *another* worktree (git forbids it; the target worktree's own branch is fine —
/// re-checking it out still syncs, like a pull), or if the target worktree is
/// dirty and `--force` was not given (force discards tracked changes).
fn ensure_branch_available(
    git: &dyn GitCli,
    repo: &Repo,
    worktree_dir: &Path,
    branch: &str,
    force: bool,
) -> Result<()> {
    let worktrees = enumerate_worktrees(repo, git)?;
    if let Some(other) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch) && !same_path(&w.path, worktree_dir))
    {
        return Err(Error::operation(format!(
            "branch {branch:?} is already checked out at {}",
            other.path.display()
        )));
    }
    if status_of(git, worktree_dir)?.dirty && !force {
        return Err(Error::operation(
            "worktree has uncommitted changes; commit/stash, or use --force",
        ));
    }
    Ok(())
}

/// Best-effort fetch of `remote` so `origin/<branch>` and the fast-forward see the
/// latest. Returns whether the fetch was skipped — `true` when no remote is
/// configured or it failed (a non-fatal warning is written to stderr), which the
/// upstream sync uses to tell "nothing to sync with" from "already up to date".
/// Shared with `wt sync` (issue #63).
pub(crate) fn fetch_remote_best_effort(
    cx: &mut Cx,
    git: &dyn GitCli,
    worktree_dir: &Path,
    remote: &str,
) -> bool {
    if !remote_configured(git, worktree_dir, remote) {
        return true;
    }
    match ops::fetch(git, worktree_dir, remote) {
        Ok(out) if out.success => false,
        _ => {
            let _ = cx.err.line(&format!(
                "warning: failed to fetch {remote}; checking out offline"
            ));
            true
        }
    }
}

/// Confirms `branch` exists locally or as a remote-tracking ref on `remote` (after
/// the fetch), erroring otherwise.
fn ensure_branch_exists(
    git: &dyn GitCli,
    repo: &Repo,
    worktree_dir: &Path,
    branch: &str,
    remote: &str,
) -> Result<()> {
    let local_exists = resolve_hex(repo.gix(), &branch_ref(branch)).is_some();
    let remote_ref = format!("refs/remotes/{remote}/{branch}");
    let remote_exists = git
        .run_raw(
            worktree_dir,
            &["rev-parse", "--verify", "--quiet", &remote_ref],
        )
        .map(|o| o.success)
        .unwrap_or(false);
    if !local_exists && !remote_exists {
        return Err(Error::operation(format!(
            "branch {branch:?} not found locally or on {remote}"
        )));
    }
    Ok(())
}

/// After a checkout, fast-forwards `branch` to its upstream when strictly behind;
/// warns and leaves it as-is on divergence. `fetch_skipped` distinguishes "no
/// remote to sync with" from "already up to date". Re-discovers the repo so the
/// upstream config the DWIM checkout just wrote is visible.
fn sync_with_upstream(
    cx: &mut Cx,
    git: &dyn GitCli,
    worktree_dir: &Path,
    branch: &str,
    fetch_skipped: bool,
) -> Result<SyncOutcome> {
    let repo = Repo::discover(worktree_dir)?;
    let Some(upstream) = upstream_of(repo.gix(), branch) else {
        return Ok(if fetch_skipped {
            SyncOutcome::FetchSkipped
        } else {
            SyncOutcome::UpToDate
        });
    };
    if upstream.is_gone {
        return Ok(SyncOutcome::FetchSkipped);
    }
    let full_ref = branch_ref(branch);
    let behind = is_ancestor(git, worktree_dir, &full_ref, &upstream.tracking_ref);
    let ahead = is_ancestor(git, worktree_dir, &upstream.tracking_ref, &full_ref);
    match (ahead, behind) {
        // Strictly behind: a proven-clean fast-forward (the tree is clean here).
        (false, true) => {
            ops::merge_ff_only(git, worktree_dir, &upstream.tracking_ref)?;
            Ok(SyncOutcome::FastForwarded)
        }
        // Diverged: never rewrite history — warn and leave the branch as-is.
        (false, false) => {
            let _ = cx.err.line(&format!(
                "warning: {branch} has diverged from {}; not fast-forwarding",
                upstream.display
            ));
            Ok(SyncOutcome::Diverged)
        }
        // Strictly ahead, or equal tips: nothing to pull.
        _ => Ok(SyncOutcome::UpToDate),
    }
}

/// If `branch` is a `<remote>/<name>` remote-tracking ref with no local branch of
/// that exact name, returns the short `<name>` so `git checkout` DWIM-creates a
/// local tracking branch. Otherwise returns `branch` unchanged.
fn normalize_remote_branch(repo: &Repo, branch: &str) -> String {
    // A local branch of this exact (possibly slashed) name always wins.
    if resolve_hex(repo.gix(), &branch_ref(branch)).is_some() {
        return branch.to_string();
    }
    // Strip a remote prefix only when `branch` names an actual remote-tracking
    // branch (e.g. `origin/feat` → `feat`).
    if let Ok(remotes) = remote_branches(repo.gix())
        && remotes.iter().any(|r| r == branch)
        && let Some((_, rest)) = branch.split_once('/')
    {
        return rest.to_string();
    }
    branch.to_string()
}

/// Whether `remote` is configured for the repository at `worktree_dir`.
pub(crate) fn remote_configured(git: &dyn GitCli, worktree_dir: &Path, remote: &str) -> bool {
    git.run_raw(worktree_dir, &["remote", "get-url", remote])
        .map(|o| o.success)
        .unwrap_or(false)
}

/// A human suffix describing the sync outcome, for the TUI status line.
pub(crate) fn sync_suffix(outcome: SyncOutcome) -> &'static str {
    match outcome {
        SyncOutcome::FastForwarded => " (fast-forwarded)",
        SyncOutcome::Diverged => " (diverged from origin)",
        SyncOutcome::UpToDate | SyncOutcome::FetchSkipped => "",
    }
}

/// Logs the sync outcome to stderr. The divergence warning is emitted in the
/// core, so here we add only a fast-forward / offline note at `-v`.
fn log_sync_outcome(cx: &mut Cx, branch: &str, outcome: SyncOutcome) {
    if cx.verbose == 0 {
        return;
    }
    match outcome {
        SyncOutcome::FastForwarded => {
            let _ = cx
                .err
                .line(&format!("fast-forwarded {branch} to its upstream"));
        }
        SyncOutcome::FetchSkipped => {
            let _ = cx.err.line(&format!(
                "checked out {branch} without syncing (no upstream)"
            ));
        }
        SyncOutcome::UpToDate | SyncOutcome::Diverged => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::open_session;
    use crate::testutil::{TestCx, TestRepo, test_cx};

    fn args(branch: &str) -> CheckoutArgs {
        CheckoutArgs {
            branch: branch.to_string(),
            no_switch: false,
            force: false,
            init_submodules: false,
            no_init_submodules: false,
        }
    }

    /// Calls the shared core against `repo`'s current worktree, returning the
    /// test context (for stdout/stderr) and the result.
    fn checkout(repo: &TestRepo, branch: &str, force: bool) -> (TestCx, Result<SyncOutcome>) {
        checkout_with_submodules(repo, branch, force, None)
    }

    /// Like [`checkout`] but with an explicit submodule-init override.
    fn checkout_with_submodules(
        repo: &TestRepo,
        branch: &str,
        force: bool,
        submodule_override: Option<bool>,
    ) -> (TestCx, Result<SyncOutcome>) {
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let git = t.cx.git.clone();
        let session = open_session(&t.cx, git.as_ref()).unwrap();
        let dir = session.repo.current_workdir().unwrap();
        let res = super::checkout_branch_in_worktree(
            &mut t.cx,
            git.as_ref(),
            &session,
            &dir,
            branch,
            force,
            submodule_override,
        );
        (t, res)
    }

    fn current_branch(repo: &TestRepo) -> String {
        repo.git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .trim()
            .to_string()
    }

    /// A repo with `<branch>` present on a bare `origin` but not locally — the
    /// remote-only DWIM case. A real remote (vs. a hand-made tracking ref) so the
    /// command's full `git fetch origin` keeps the branch even under `fetch.prune`.
    fn repo_with_remote_branch(branch: &str) -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["checkout", "-q", "-b", branch]);
        repo.write("f.txt", "remote work\n");
        repo.commit_all("remote commit");
        repo.git(&["push", "-q", "origin", branch]);
        // Drop it locally so it is remote-only (origin/<branch> remains).
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["branch", "-D", branch]);
        (repo, bare)
    }

    /// A repo whose local `<branch>` is strictly behind `origin/<branch>` (a bare
    /// origin holds the advanced tip), set up to fast-forward on checkout.
    fn repo_behind_origin(branch: &str) -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["checkout", "-q", "-b", branch]);
        let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("f.txt", "advanced\n");
        repo.commit_all("advanced");
        repo.git(&["push", "-q", "-u", "origin", branch]);
        repo.git(&["checkout", "-q", "main"]);
        repo.git(&["branch", "-f", branch, &base]);
        (repo, bare)
    }

    /// A repo whose local `<branch>` has diverged from `origin/<branch>` (each has
    /// a unique commit atop a common base). Returns the local tip hash.
    fn repo_diverged_from_origin(branch: &str) -> (TestRepo, TestRepo, String) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["checkout", "-q", "-b", branch]);
        let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("o.txt", "origin side\n");
        repo.commit_all("origin commit");
        repo.git(&["push", "-q", "-u", "origin", branch]);
        repo.git(&["reset", "-q", "--hard", &base]);
        repo.write("l.txt", "local side\n");
        repo.commit_all("local commit");
        let local = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["checkout", "-q", "main"]);
        (repo, bare, local)
    }

    #[test]
    fn checks_out_remote_only_branch_creating_tracking_branch() {
        let (repo, _bare) = repo_with_remote_branch("feature/x");
        let (_t, res) = checkout(&repo, "feature/x", false);
        assert_eq!(res.unwrap(), SyncOutcome::UpToDate);
        // A local branch was DWIM-created, tracking origin, and is checked out.
        assert!(
            !repo
                .git(&["rev-parse", "--verify", "refs/heads/feature/x"])
                .trim()
                .is_empty()
        );
        assert_eq!(
            repo.git(&["config", "--get", "branch.feature/x.remote"])
                .trim(),
            "origin"
        );
        assert_eq!(current_branch(&repo), "feature/x");
        // The remote head's file is present (correct head checked out).
        assert!(repo.root().join("f.txt").exists());
    }

    #[test]
    fn normalizes_remote_prefixed_selection() {
        // Passing `origin/feature/x` checks out the short `feature/x` (DWIM),
        // rather than detaching HEAD on the remote-tracking ref.
        let (repo, _bare) = repo_with_remote_branch("feature/x");
        let (_t, res) = checkout(&repo, "origin/feature/x", false);
        assert_eq!(res.unwrap(), SyncOutcome::UpToDate);
        assert_eq!(current_branch(&repo), "feature/x");
    }

    #[test]
    fn fast_forwards_a_branch_behind_origin() {
        let (repo, _bare) = repo_behind_origin("feat");
        let (_t, res) = checkout(&repo, "feat", false);
        assert_eq!(res.unwrap(), SyncOutcome::FastForwarded);
        assert_eq!(current_branch(&repo), "feat");
        // The local branch now matches origin's advanced tip.
        assert_eq!(
            repo.git(&["rev-parse", "feat"]).trim(),
            repo.git(&["rev-parse", "refs/remotes/origin/feat"]).trim()
        );
    }

    #[test]
    fn diverged_branch_warns_and_is_left_unchanged() {
        let (repo, _bare, local) = repo_diverged_from_origin("feat");
        let (t, res) = checkout(&repo, "feat", false);
        assert_eq!(res.unwrap(), SyncOutcome::Diverged);
        assert!(t.err.contents().contains("diverged"));
        // The local commit is intact (no rewrite).
        assert_eq!(repo.git(&["rev-parse", "feat"]).trim(), local);
    }

    #[test]
    fn refuses_dirty_worktree_without_force() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        repo.write("README.md", "dirty\n"); // modify a tracked file
        let (_t, res) = checkout(&repo, "topic", false);
        let err = res.unwrap_err().to_string();
        assert!(err.contains("uncommitted changes"));
        assert!(err.contains("--force"));
        // Still on main (no switch happened).
        assert_eq!(current_branch(&repo), "main");
    }

    #[test]
    fn force_discards_changes_and_switches() {
        let repo = TestRepo::init();
        // `topic` is created from main (README.md = "init\n").
        repo.git(&["branch", "topic"]);
        repo.write("README.md", "dirty\n");
        let (_t, res) = checkout(&repo, "topic", true);
        assert_eq!(res.unwrap(), SyncOutcome::FetchSkipped);
        assert_eq!(current_branch(&repo), "topic");
        // The uncommitted change was discarded.
        assert_eq!(
            std::fs::read_to_string(repo.root().join("README.md")).unwrap(),
            "init\n"
        );
    }

    #[test]
    fn refuses_branch_checked_out_in_another_worktree() {
        let repo = TestRepo::init();
        repo.add_worktree("dup", "../dup");
        let (_t, res) = checkout(&repo, "dup", false);
        assert!(res.unwrap_err().to_string().contains("already checked out"));
    }

    #[test]
    fn checks_out_local_branch_without_remote() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        let (_t, res) = checkout(&repo, "topic", false);
        // No remote: the fetch is skipped and there is no upstream to sync.
        assert_eq!(res.unwrap(), SyncOutcome::FetchSkipped);
        assert_eq!(current_branch(&repo), "topic");
    }

    #[test]
    fn current_branch_is_allowed_and_not_an_elsewhere_error() {
        let repo = TestRepo::init();
        let (_t, res) = checkout(&repo, "main", false);
        assert_eq!(res.unwrap(), SyncOutcome::FetchSkipped);
        assert_eq!(current_branch(&repo), "main");
    }

    #[test]
    fn unknown_branch_is_an_error() {
        let repo = TestRepo::init();
        let (_t, res) = checkout(&repo, "does-not-exist", false);
        assert!(res.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn invalid_branch_name_is_a_usage_error() {
        let repo = TestRepo::init();
        let (_t, res) = checkout(&repo, "a b", false);
        assert!(matches!(res.unwrap_err(), Error::Usage(_)));
    }

    #[test]
    fn reattaches_from_detached_head() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "--detach"]);
        let (_t, res) = checkout(&repo, "main", false);
        assert_eq!(res.unwrap(), SyncOutcome::FetchSkipped);
        assert_eq!(current_branch(&repo), "main");
    }

    #[test]
    fn run_no_switch_prints_note_to_stderr() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let mut a = args("topic");
        a.no_switch = true;
        let code = super::run(&mut t.cx, &a, false).unwrap();
        assert_eq!(code, 0);
        assert!(t.out.contents().is_empty());
        assert!(t.err.contents().contains("checked out topic in"));
    }

    #[test]
    fn run_json_emits_worktree_row() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args("topic"), true).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(t.out.contents().trim()).unwrap();
        assert_eq!(v["branch"], serde_json::json!("topic"));
        assert_eq!(v["schema_version"], serde_json::json!(1));
    }

    #[test]
    fn run_prints_path_for_cd_by_default() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args("topic"), false).unwrap();
        assert_eq!(code, 0);
        // The worktree path is printed (so the shell wrapper cd's into it).
        let out = t.out.contents();
        assert!(crate::commands::same_path(
            std::path::Path::new(out.trim()),
            repo.root()
        ));
        assert_eq!(current_branch(&repo), "topic");
    }

    /// A repo with a submodule committed on `main` and a `topic` branch sharing
    /// it, then deinitialized so it reports as uninitialized while keeping its
    /// objects under `.git/modules` (so init reuses them, no file-protocol clone).
    fn repo_with_uninitialized_submodule_on_topic() -> TestRepo {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        repo.git(&["branch", "topic"]);
        repo.deinit_submodule("libs/sub");
        repo
    }

    #[test]
    fn checkout_initializes_submodules_when_enabled() {
        // Switching to a branch with an uninitialized submodule, with init forced
        // on, populates it (issue #50 — "new module definitions on branch switch").
        let repo = repo_with_uninitialized_submodule_on_topic();
        let (t, res) = checkout_with_submodules(&repo, "topic", false, Some(true));
        assert!(res.is_ok());
        assert_eq!(current_branch(&repo), "topic");
        assert!(repo.root().join("libs/sub/sub.txt").exists());
        assert!(t.err.contents().contains("initializing 1 submodule"));
    }

    #[test]
    fn checkout_default_leaves_submodules_uninitialized() {
        let repo = repo_with_uninitialized_submodule_on_topic();
        let (t, res) = checkout(&repo, "topic", false);
        assert!(res.is_ok());
        assert_eq!(current_branch(&repo), "topic");
        assert!(!repo.root().join("libs/sub/sub.txt").exists());
        assert!(!t.err.contents().contains("initializing"));
    }

    #[test]
    fn verbose_logs_fast_forward() {
        let (repo, _bare) = repo_behind_origin("feat");
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        t.cx.verbose = 1;
        super::run(&mut t.cx, &args("feat"), false).unwrap();
        assert!(t.err.contents().contains("fast-forwarded"));
    }

    #[test]
    fn successful_fetch_without_upstream_reports_up_to_date() {
        // A configured, reachable remote whose fetch succeeds, but the checked-out
        // branch has no upstream: the sync reports UpToDate (a skipped or failed
        // fetch would report FetchSkipped), and no offline warning is printed. This
        // pins the fetch-skipped bool that `fetch_remote_best_effort` returns.
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "origin", "main"]);
        repo.git(&["branch", "topic"]); // local branch, no upstream
        let (t, res) = checkout(&repo, "topic", false);
        assert_eq!(res.unwrap(), SyncOutcome::UpToDate);
        assert_eq!(current_branch(&repo), "topic");
        assert!(!t.err.contents().contains("failed to fetch"));
    }

    #[test]
    fn failed_fetch_warns_and_checks_out_offline() {
        // A configured but unreachable remote (bad URL): the fetch runs and fails,
        // so a warning is printed and the checkout proceeds offline (FetchSkipped) —
        // distinct from "fetch succeeded" (UpToDate) and from "no remote at all".
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", "/nonexistent/origin.git"]);
        repo.git(&["branch", "topic"]); // local branch, no upstream
        let (t, res) = checkout(&repo, "topic", false);
        assert_eq!(res.unwrap(), SyncOutcome::FetchSkipped);
        assert_eq!(current_branch(&repo), "topic");
        assert!(t.err.contents().contains("failed to fetch origin"));
    }
}
