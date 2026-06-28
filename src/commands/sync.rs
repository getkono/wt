//! `wt sync [<query>]` — pull then push a worktree's branch (issue #63).
//!
//! Sync is "standard like VS Code": it fetches the remote, fast-forwards the
//! branch when it is strictly behind its upstream, and pushes when it is strictly
//! ahead. It is safety-first and never rewrites history — a diverged branch is
//! refused with guidance (rebase/merge manually), and a fast-forward is refused
//! over a dirty tree. After a fast-forward that may have changed submodule
//! definitions it offers to initialize them (the CLI prompts; the TUI follows the
//! `[submodules] init` policy). The core (`sync_worktree`) is shared with the TUI
//! (`tui::runtime`), mirroring `commands::checkout`.

use std::path::{Path, PathBuf};

use crate::cli::SyncArgs;
use crate::commands::checkout::fetch_remote_best_effort;
use crate::commands::{
    Resolution, Session, candidate_label, confirm, maybe_init_submodules, open_session,
    resolve_query, same_path,
};
use crate::config::SubmoduleInit;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{branch_ref, current_branch, enumerate, is_ancestor, ops, status_of, upstream_of};
use crate::worktree_service::{build_rows, build_worktrees};

/// What a sync did to a worktree's branch, for the caller's messaging (CLI + TUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncOutcome {
    /// Already in sync with the upstream (equal tips, or nothing left to do).
    UpToDate,
    /// The branch was fast-forwarded to its upstream (pulled).
    FastForwarded,
    /// The branch was strictly ahead and was pushed to its upstream.
    Pushed,
    /// The branch has diverged from its upstream; refused (no rewrite, no push).
    Diverged,
    /// A worktree-less branch has diverged from its upstream; it cannot be merged
    /// in place (it has no working tree), so the user must check it out first.
    DivergedNoWorktree,
    /// No present upstream is configured (or detached HEAD); nothing to sync with.
    NoUpstream,
    /// A fast-forward was needed but the worktree is dirty; refused.
    Dirty,
    /// The push was rejected by the remote (non-fast-forward / protected branch).
    PushRejected,
}

/// Syncs one or more worktrees (default: the current worktree; `--all` for every
/// one; or the worktree matched by `<query>`). Prints a per-worktree result line,
/// or newline-delimited JSON rows (post-sync state) with `--json`.
pub(crate) fn run(cx: &mut Cx, args: &SyncArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    // A `<query>` may name a worktree-less branch, so resolve over branch rows too
    // (issue #47/#63); `--all` and the default stay worktrees-only so `--all` does
    // not mass-sync every local branch.
    let worktrees = if args.query.is_some() {
        build_rows(&session.repo, git)?
    } else {
        build_worktrees(&session.repo, git)?
    };

    let selected: Vec<usize> = if args.all {
        (0..worktrees.len()).collect()
    } else if let Some(query) = &args.query {
        match resolve_query(cx, &worktrees, query) {
            Resolution::Found(index) => vec![index],
            Resolution::Ambiguous => return Ok(3),
            Resolution::NotFound => {
                return Err(Error::NotFound {
                    query: query.clone(),
                });
            }
        }
    } else {
        match worktrees.iter().position(|w| w.is_current) {
            Some(index) => vec![index],
            None => return Err(Error::NoCurrentWorktree),
        }
    };

    let submodule_override = args.submodule_override();
    for &index in &selected {
        let worktree = &worktrees[index];
        let label = candidate_label(worktree);
        // A deleted worktree has no working tree on disk to sync — skip it so a
        // missing row in `--all` does not abort the rest.
        if worktree.is_missing {
            let _ = cx.err.line(&format!("skipping missing worktree {label}"));
            continue;
        }
        let outcome = if worktree.has_worktree {
            sync_worktree(
                cx,
                git,
                &session,
                &worktree.path,
                submodule_override,
                !json,
                args.no_push,
            )?
        } else {
            // A worktree-less branch row: move the branch ref in place.
            match &worktree.branch {
                Some(branch) => sync_branch(cx, git, &session, branch, args.no_push)?,
                None => continue,
            }
        };
        if !json {
            cx.out
                .line(&format!("{label}: {}", outcome_note(outcome)))?;
        }
    }

    if json {
        // Re-read post-sync state so the emitted rows reflect the new ahead/behind.
        // Include branch rows so a synced worktree-less branch is found by its
        // stable `branch://<branch>` virtual path.
        let repo = Repo::discover(&session.primary_root)?;
        let fresh = build_rows(&repo, git)?;
        for &index in &selected {
            let target = &worktrees[index].path;
            if let Some(worktree) = fresh.iter().find(|w| same_path(&w.path, target)) {
                cx.out.line(&worktree.to_json_line()?)?;
            }
        }
    }
    Ok(0)
}

/// Syncs the branch checked out in `worktree_dir` in place: fetch, fast-forward
/// when behind (never over a dirty tree), push when ahead, refuse on divergence.
/// The repo is discovered from `worktree_dir` so `--all` targets each resolve
/// their own branch/upstream. `prompt` enables the interactive submodule
/// confirmation (CLI only); the TUI passes `false` and lets the policy decide.
/// Returns what the sync did so the caller can phrase its result.
pub(crate) fn sync_worktree(
    cx: &mut Cx,
    git: &dyn GitCli,
    session: &Session,
    worktree_dir: &Path,
    submodule_override: Option<bool>,
    prompt: bool,
    no_push: bool,
) -> Result<SyncOutcome> {
    let remote = session.config.pr_default_remote.clone();

    // Resolve the current branch (detached/unborn HEAD has nothing to sync).
    let repo = Repo::discover(worktree_dir)?;
    let Some(branch) = current_branch(repo.gix()) else {
        return Ok(SyncOutcome::NoUpstream);
    };
    // A present upstream must be configured to sync against.
    if upstream_of(repo.gix(), &branch).is_none_or(|u| u.is_gone) {
        return Ok(SyncOutcome::NoUpstream);
    }

    // Best-effort fetch so the tracking ref reflects the remote (offline-tolerant).
    let _ = fetch_remote_best_effort(cx, git, repo.gix(), worktree_dir, &remote);

    // Re-discover so the freshly fetched tracking ref is visible.
    let repo = Repo::discover(worktree_dir)?;
    let Some(upstream) = upstream_of(repo.gix(), &branch).filter(|u| !u.is_gone) else {
        return Ok(SyncOutcome::NoUpstream);
    };

    let full_ref = branch_ref(&branch);
    let behind = is_ancestor(repo.gix(), &full_ref, &upstream.tracking_ref);
    let ahead = is_ancestor(repo.gix(), &upstream.tracking_ref, &full_ref);
    match (ahead, behind) {
        // Strictly behind: fast-forward, but never discard uncommitted work.
        (false, true) => {
            if status_of(git, worktree_dir)?.dirty {
                let _ = cx.err.line(&format!(
                    "warning: {branch} is behind {} but the worktree is dirty; commit or stash first",
                    upstream.display
                ));
                return Ok(SyncOutcome::Dirty);
            }
            ops::merge_ff_only(git, worktree_dir, &upstream.tracking_ref)?;
            // A fast-forward can introduce new submodule definitions (issue #50).
            maybe_sync_submodules(
                cx,
                git,
                worktree_dir,
                session.config.submodules_init,
                submodule_override,
                prompt,
            )?;
            Ok(SyncOutcome::FastForwarded)
        }
        // Diverged: never rewrite history — refuse and guide the user.
        (false, false) => {
            let _ = cx.err.line(&format!(
                "warning: {branch} has diverged from {}; not fast-forwarding — rebase or merge manually",
                upstream.display
            ));
            Ok(SyncOutcome::Diverged)
        }
        // Strictly ahead: push (a push never touches the working tree, so no dirty
        // guard); `--no-push` makes sync pull-only.
        (true, false) => {
            if no_push {
                Ok(SyncOutcome::UpToDate)
            } else {
                push_branch(cx, git, worktree_dir, &remote, &branch)
            }
        }
        // Equal tips (each is an ancestor of the other): nothing to do.
        (true, true) => Ok(SyncOutcome::UpToDate),
    }
}

/// Syncs a worktree-less `branch` by moving its ref directly: fetch, fast-forward
/// the local ref to its upstream when strictly behind, push when strictly ahead,
/// and refuse on divergence (a non-checked-out branch has no working tree to merge
/// in). The branch must not be checked out — moving a checked-out branch's ref is
/// refused with guidance to sync from that worktree instead. Refs and config are
/// repo-global, so every step runs from the primary root. Shared with the TUI's
/// branch-row sync, mirroring [`sync_worktree`] (issue #47/#63).
pub(crate) fn sync_branch(
    cx: &mut Cx,
    git: &dyn GitCli,
    session: &Session,
    branch: &str,
    no_push: bool,
) -> Result<SyncOutcome> {
    let remote = session.config.pr_default_remote.clone();
    let root = &session.primary_root;

    // A branch checked out in a worktree has a working tree and a locked ref; we
    // must not move it from here. Point the user at that worktree to sync in place.
    if let Some(path) = checked_out_at(git, root, branch)? {
        return Err(Error::operation(format!(
            "{branch} is checked out at {}; sync from that worktree",
            path.display()
        )));
    }

    // A present upstream must be configured to sync against.
    let repo = Repo::discover(root)?;
    if upstream_of(repo.gix(), branch).is_none_or(|u| u.is_gone) {
        return Ok(SyncOutcome::NoUpstream);
    }

    // Best-effort fetch so the tracking ref reflects the remote (offline-tolerant).
    let _ = fetch_remote_best_effort(cx, git, repo.gix(), root, &remote);

    // Re-discover so the freshly fetched tracking ref is visible.
    let repo = Repo::discover(root)?;
    let Some(upstream) = upstream_of(repo.gix(), branch).filter(|u| !u.is_gone) else {
        return Ok(SyncOutcome::NoUpstream);
    };

    let full_ref = branch_ref(branch);
    let behind = is_ancestor(repo.gix(), &full_ref, &upstream.tracking_ref);
    let ahead = is_ancestor(repo.gix(), &upstream.tracking_ref, &full_ref);
    match (ahead, behind) {
        // Strictly behind: fast-forward the ref in place (no working tree to touch,
        // so no dirty guard and no submodule step). `git branch -f` would itself
        // refuse a checked-out branch — a backstop to the guard above.
        (false, true) => {
            ops::set_branch_ref(git, root, branch, &upstream.tracking_ref)?;
            Ok(SyncOutcome::FastForwarded)
        }
        // Diverged: a worktree-less branch cannot be merged in place — guide the
        // user to check it out (never rewrite history, never push).
        (false, false) => {
            let _ = cx.err.line(&format!(
                "warning: {branch} has diverged from {}; check it out in a worktree to rebase or merge manually",
                upstream.display
            ));
            Ok(SyncOutcome::DivergedNoWorktree)
        }
        // Strictly ahead: push (a push never touches a working tree); `--no-push`
        // makes sync pull-only.
        (true, false) => {
            if no_push {
                Ok(SyncOutcome::UpToDate)
            } else {
                push_branch(cx, git, root, &remote, branch)
            }
        }
        // Equal tips (each is an ancestor of the other): nothing to do.
        (true, true) => Ok(SyncOutcome::UpToDate),
    }
}

/// The path of the worktree that has `branch` checked out, or `None` if no
/// worktree does. Bare entries (no working directory) never count. Used to refuse
/// moving a checked-out branch's ref from elsewhere.
fn checked_out_at(git: &dyn GitCli, root: &Path, branch: &str) -> Result<Option<PathBuf>> {
    Ok(enumerate(git, root)?
        .into_iter()
        .find(|w| !w.is_bare && w.branch.as_deref() == Some(branch))
        .map(|w| w.path))
}

/// Pushes `branch` to `remote`, classifying a rejected push as a sentinel
/// outcome (never an error) so `--all` keeps going. The rejection reason is
/// written to stderr.
fn push_branch(
    cx: &mut Cx,
    git: &dyn GitCli,
    dir: &Path,
    remote: &str,
    branch: &str,
) -> Result<SyncOutcome> {
    let out = ops::push(git, dir, remote, branch)?;
    if out.success {
        Ok(SyncOutcome::Pushed)
    } else {
        let _ = cx.err.line(&format!(
            "warning: push of {branch} to {remote} was rejected: {}",
            out.stderr.trim()
        ));
        Ok(SyncOutcome::PushRejected)
    }
}

/// After a fast-forward, handles submodules whose definitions may have changed.
/// In the CLI at a TTY (`prompt`) it asks before running
/// `git submodule update --init --recursive`, only when there are uninitialized
/// submodules; otherwise (the TUI / non-interactive) it follows the
/// `[submodules] init` policy via [`maybe_init_submodules`]. Best-effort.
fn maybe_sync_submodules(
    cx: &mut Cx,
    git: &dyn GitCli,
    dir: &Path,
    policy: SubmoduleInit,
    submodule_override: Option<bool>,
    prompt: bool,
) -> Result<()> {
    if prompt && cx.err.is_tty() {
        let pending = crate::git::submodule::uninitialized(git, dir)?;
        if pending.is_empty() {
            return Ok(());
        }
        let ask = format!(
            "submodule definitions changed ({} new); run `git submodule update --init --recursive`? [y/N] ",
            pending.len()
        );
        if confirm(cx, &ask)?
            && let Err(e) = crate::git::submodule::update_init(git, dir)
        {
            let _ = cx
                .err
                .line(&format!("warning: failed to initialize submodules: {e}"));
        }
        Ok(())
    } else {
        maybe_init_submodules(cx, git, dir, policy, submodule_override)
    }
}

/// A human result note for the CLI per-worktree summary line.
fn outcome_note(outcome: SyncOutcome) -> &'static str {
    match outcome {
        SyncOutcome::UpToDate => "up to date",
        SyncOutcome::FastForwarded => "fast-forwarded",
        SyncOutcome::Pushed => "pushed",
        SyncOutcome::Diverged => "diverged (resolve manually)",
        SyncOutcome::DivergedNoWorktree => "diverged (check out to merge manually)",
        SyncOutcome::NoUpstream => "no upstream",
        SyncOutcome::Dirty => "dirty (commit or stash first)",
        SyncOutcome::PushRejected => "push rejected",
    }
}

/// A human suffix describing the sync outcome, for the TUI status line.
pub(crate) fn sync_suffix(outcome: SyncOutcome) -> &'static str {
    match outcome {
        SyncOutcome::FastForwarded => " (fast-forwarded)",
        SyncOutcome::Pushed => " (pushed)",
        SyncOutcome::Diverged => " (diverged from origin)",
        SyncOutcome::DivergedNoWorktree => " (diverged — check out to merge manually)",
        SyncOutcome::NoUpstream => " (no upstream)",
        SyncOutcome::Dirty => " (dirty — commit/stash first)",
        SyncOutcome::PushRejected => " (push rejected)",
        SyncOutcome::UpToDate => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::Stream;
    use crate::git::cli::{GitCli, GitOutput, RealGit};
    use crate::testutil::{CannedInput, SharedBuf, TestCx, TestRepo, test_cx};

    fn args(query: Option<&str>, all: bool, no_push: bool) -> SyncArgs {
        SyncArgs {
            query: query.map(str::to_string),
            all,
            no_push,
            init_submodules: false,
            no_init_submodules: false,
        }
    }

    /// Calls the shared core against `repo`'s current worktree (no push suppressed,
    /// no interactive prompt), returning the test context and the result.
    fn sync(repo: &TestRepo) -> (TestCx, Result<SyncOutcome>) {
        sync_opts(repo, false)
    }

    /// Like [`sync`] but with an explicit `no_push`.
    fn sync_opts(repo: &TestRepo, no_push: bool) -> (TestCx, Result<SyncOutcome>) {
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let git = t.cx.git.clone();
        let session = open_session(&t.cx, git.as_ref()).unwrap();
        let dir = session.repo.current_workdir().unwrap();
        let res = super::sync_worktree(
            &mut t.cx,
            git.as_ref(),
            &session,
            &dir,
            None,
            false,
            no_push,
        );
        (t, res)
    }

    /// A repo whose checked-out `main` is strictly behind `origin/main` on a bare
    /// origin (set up to fast-forward on sync).
    fn repo_behind_upstream() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("f.txt", "advanced\n");
        repo.commit_all("advanced");
        repo.git(&["push", "-q", "origin", "main"]);
        // Rewind local main to base so it is strictly behind origin/main.
        repo.git(&["reset", "-q", "--hard", &base]);
        (repo, bare)
    }

    /// A repo whose checked-out `main` is strictly ahead of `origin/main` (one
    /// unpushed local commit) on a bare origin.
    fn repo_ahead_of_upstream() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        repo.write("g.txt", "local\n");
        repo.commit_all("local commit");
        (repo, bare)
    }

    /// A repo whose checked-out `main` has diverged from `origin/main` (each has a
    /// unique commit atop a common base). Returns the local tip hash.
    fn repo_diverged_from_upstream() -> (TestRepo, TestRepo, String) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("o.txt", "origin side\n");
        repo.commit_all("origin commit");
        repo.git(&["push", "-q", "origin", "main"]);
        repo.git(&["reset", "-q", "--hard", &base]);
        repo.write("l.txt", "local side\n");
        repo.commit_all("local commit");
        let local = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        (repo, bare, local)
    }

    /// A repo whose checked-out `main` equals `origin/main` (nothing to sync).
    fn repo_up_to_date() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        (repo, bare)
    }

    #[test]
    fn fast_forwards_when_behind() {
        let (repo, _bare) = repo_behind_upstream();
        let (_t, res) = sync(&repo);
        assert_eq!(res.unwrap(), SyncOutcome::FastForwarded);
        assert_eq!(
            repo.git(&["rev-parse", "main"]).trim(),
            repo.git(&["rev-parse", "refs/remotes/origin/main"]).trim()
        );
    }

    #[test]
    fn pushes_when_ahead() {
        let (repo, bare) = repo_ahead_of_upstream();
        let local = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        let (_t, res) = sync(&repo);
        assert_eq!(res.unwrap(), SyncOutcome::Pushed);
        // The bare origin advanced to the local tip.
        assert_eq!(bare.git(&["rev-parse", "main"]).trim(), local);
    }

    #[test]
    fn ahead_with_no_push_is_up_to_date_and_does_not_push() {
        let (repo, bare) = repo_ahead_of_upstream();
        let before = bare.git(&["rev-parse", "main"]).trim().to_string();
        let (_t, res) = sync_opts(&repo, true);
        assert_eq!(res.unwrap(), SyncOutcome::UpToDate);
        assert_eq!(bare.git(&["rev-parse", "main"]).trim(), before);
    }

    #[test]
    fn diverged_refuses_and_warns() {
        let (repo, bare, local) = repo_diverged_from_upstream();
        let origin_before = bare.git(&["rev-parse", "main"]).trim().to_string();
        let (t, res) = sync(&repo);
        assert_eq!(res.unwrap(), SyncOutcome::Diverged);
        assert!(t.err.contents().contains("diverged"));
        // No rewrite and no push.
        assert_eq!(repo.git(&["rev-parse", "main"]).trim(), local);
        assert_eq!(bare.git(&["rev-parse", "main"]).trim(), origin_before);
    }

    #[test]
    fn dirty_blocks_fast_forward() {
        let (repo, _bare) = repo_behind_upstream();
        let before = repo.git(&["rev-parse", "main"]).trim().to_string();
        repo.write("README.md", "dirty\n"); // a tracked modification
        let (t, res) = sync(&repo);
        assert_eq!(res.unwrap(), SyncOutcome::Dirty);
        assert!(t.err.contents().contains("commit or stash"));
        // The branch was not moved.
        assert_eq!(repo.git(&["rev-parse", "main"]).trim(), before);
    }

    #[test]
    fn dirty_does_not_block_push_when_ahead() {
        // The dirty guard is fast-forward-only: a push never touches the tree.
        let (repo, bare) = repo_ahead_of_upstream();
        let local = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("README.md", "dirty\n");
        let (_t, res) = sync(&repo);
        assert_eq!(res.unwrap(), SyncOutcome::Pushed);
        assert_eq!(bare.git(&["rev-parse", "main"]).trim(), local);
    }

    #[test]
    fn no_upstream_is_reported() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "topic"]); // no upstream, no remote
        let (_t, res) = sync(&repo);
        assert_eq!(res.unwrap(), SyncOutcome::NoUpstream);
    }

    #[test]
    fn up_to_date_when_equal() {
        let (repo, _bare) = repo_up_to_date();
        let (_t, res) = sync(&repo);
        assert_eq!(res.unwrap(), SyncOutcome::UpToDate);
    }

    #[test]
    fn detached_head_has_no_upstream() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "--detach"]);
        let (_t, res) = sync(&repo);
        assert_eq!(res.unwrap(), SyncOutcome::NoUpstream);
    }

    /// A [`GitCli`] whose `run_raw` returns a fixed canned outcome (for testing
    /// `push_branch`'s success/rejection classification without a real remote).
    struct CannedGit(GitOutput);
    impl GitCli for CannedGit {
        fn run_raw(&self, _repo: &Path, _args: &[&str]) -> Result<GitOutput> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn push_branch_reports_success() {
        let git = CannedGit(GitOutput {
            success: true,
            stdout: String::new(),
            stderr: String::new(),
        });
        let mut t = test_cx(&[], "/work");
        let out =
            super::push_branch(&mut t.cx, &git, Path::new("/work"), "origin", "main").unwrap();
        assert_eq!(out, SyncOutcome::Pushed);
    }

    #[test]
    fn push_branch_reports_rejection_and_warns() {
        let git = CannedGit(GitOutput {
            success: false,
            stdout: String::new(),
            stderr: "! [rejected] main -> main (non-fast-forward)".into(),
        });
        let mut t = test_cx(&[], "/work");
        let out =
            super::push_branch(&mut t.cx, &git, Path::new("/work"), "origin", "main").unwrap();
        assert_eq!(out, SyncOutcome::PushRejected);
        assert!(t.err.contents().contains("rejected"));
    }

    /// A repo with one submodule deinitialized, so it reports as uninitialized but
    /// `update --init` can reuse `.git/modules` (no file-protocol clone).
    fn repo_with_uninitialized_submodule() -> TestRepo {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        repo.deinit_submodule("libs/sub");
        repo
    }

    /// Builds a `cx` whose stderr is a TTY (so the submodule prompt fires) wired
    /// to the given canned input line.
    fn tty_cx(repo: &TestRepo, answer: &str) -> (TestCx, SharedBuf) {
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let err = SharedBuf::new();
        t.cx.err = Stream::new(Box::new(err.clone()), true);
        t.cx.input = Box::new(CannedInput::new(&[answer]));
        (t, err)
    }

    #[test]
    fn submodule_prompt_yes_initializes() {
        let repo = repo_with_uninitialized_submodule();
        let (mut t, err) = tty_cx(&repo, "y");
        super::maybe_sync_submodules(
            &mut t.cx,
            &RealGit,
            repo.root(),
            SubmoduleInit::Never,
            None,
            true,
        )
        .unwrap();
        assert!(repo.root().join("libs/sub/sub.txt").exists());
        assert!(err.contents().contains("submodule definitions changed"));
    }

    #[test]
    fn submodule_prompt_no_leaves_uninitialized() {
        let repo = repo_with_uninitialized_submodule();
        let (mut t, _err) = tty_cx(&repo, "n");
        super::maybe_sync_submodules(
            &mut t.cx,
            &RealGit,
            repo.root(),
            SubmoduleInit::Never,
            None,
            true,
        )
        .unwrap();
        assert!(!repo.root().join("libs/sub/sub.txt").exists());
    }

    #[test]
    fn submodule_policy_path_follows_override_without_prompting() {
        // prompt=false (the TUI / non-interactive path): the override decides and
        // no input is read.
        let repo = repo_with_uninitialized_submodule();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        super::maybe_sync_submodules(
            &mut t.cx,
            &RealGit,
            repo.root(),
            SubmoduleInit::Never,
            Some(true),
            false,
        )
        .unwrap();
        assert!(repo.root().join("libs/sub/sub.txt").exists());
    }

    #[test]
    fn sync_suffix_covers_every_outcome() {
        assert_eq!(super::sync_suffix(SyncOutcome::UpToDate), "");
        assert!(super::sync_suffix(SyncOutcome::FastForwarded).contains("fast-forwarded"));
        assert!(super::sync_suffix(SyncOutcome::Pushed).contains("pushed"));
        assert!(super::sync_suffix(SyncOutcome::Diverged).contains("diverged"));
        assert!(super::sync_suffix(SyncOutcome::NoUpstream).contains("no upstream"));
        assert!(super::sync_suffix(SyncOutcome::Dirty).contains("dirty"));
        assert!(super::sync_suffix(SyncOutcome::PushRejected).contains("push rejected"));
    }

    #[test]
    fn run_syncs_current_worktree() {
        let (repo, _bare) = repo_behind_upstream();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(None, false, false), false).unwrap();
        assert_eq!(code, 0);
        assert!(t.out.contents().contains("fast-forwarded"));
    }

    #[test]
    fn run_all_reports_each_without_aborting() {
        // main is behind origin (fast-forwards); a second worktree on `topic` has
        // no upstream (a condition, not an abort).
        let (repo, _bare) = repo_behind_upstream();
        repo.add_worktree("topic", "../wt-topic");
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(None, true, false), false).unwrap();
        assert_eq!(code, 0);
        let out = t.out.contents();
        assert!(out.contains("fast-forwarded"));
        assert!(out.contains("no upstream"));
    }

    #[test]
    fn run_all_skips_missing_worktree() {
        let repo = TestRepo::init();
        repo.add_worktree("gone", "../wt-gone");
        std::fs::remove_dir_all(repo.root().parent().unwrap().join("wt-gone")).unwrap();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(None, true, false), false).unwrap();
        assert_eq!(code, 0);
        assert!(t.err.contents().contains("skipping missing worktree gone"));
    }

    #[test]
    fn run_json_emits_rows() {
        let (repo, _bare) = repo_up_to_date();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(None, false, false), true).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(t.out.contents().trim()).unwrap();
        assert_eq!(v["schema_version"], serde_json::json!(1));
        assert_eq!(v["branch"], serde_json::json!("main"));
    }

    #[test]
    fn run_no_push_skips_push() {
        let (repo, bare) = repo_ahead_of_upstream();
        let before = bare.git(&["rev-parse", "main"]).trim().to_string();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(None, false, true), false).unwrap();
        assert_eq!(code, 0);
        assert_eq!(bare.git(&["rev-parse", "main"]).trim(), before);
    }

    #[test]
    fn run_unknown_query_is_not_found() {
        let repo = TestRepo::init();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(
            &mut t.cx,
            &args(Some("does-not-exist"), false, false),
            false,
        )
        .unwrap_err();
        assert!(matches!(err, Error::NotFound { .. }));
    }

    // --- worktree-less branch sync (issue #47/#63) ---

    /// Calls the worktree-less branch core against `repo` for `branch`.
    fn sync_branch_call(
        repo: &TestRepo,
        branch: &str,
        no_push: bool,
    ) -> (TestCx, Result<SyncOutcome>) {
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let git = t.cx.git.clone();
        let session = open_session(&t.cx, git.as_ref()).unwrap();
        let res = super::sync_branch(&mut t.cx, git.as_ref(), &session, branch, no_push);
        (t, res)
    }

    /// A repo left on `main` with a worktree-less `feat` tracking `origin/feat` and
    /// strictly behind it (set up to fast-forward the ref on sync).
    fn repo_branch_behind() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        repo.git(&["checkout", "-q", "-b", "feat"]);
        repo.git(&["push", "-q", "-u", "origin", "feat"]);
        let base = repo.git(&["rev-parse", "feat"]).trim().to_string();
        repo.write("f.txt", "advanced\n");
        repo.commit_all("advanced feat");
        repo.git(&["push", "-q", "origin", "feat"]);
        repo.git(&["checkout", "-q", "main"]);
        // Rewind local feat so it is strictly behind origin/feat (it is not checked
        // out, so the ref can be moved without a worktree).
        repo.git(&["branch", "-f", "feat", &base]);
        (repo, bare)
    }

    /// A repo left on `main` with a worktree-less `feat` one commit ahead of
    /// `origin/feat`.
    fn repo_branch_ahead() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        repo.git(&["checkout", "-q", "-b", "feat"]);
        repo.git(&["push", "-q", "-u", "origin", "feat"]);
        repo.write("g.txt", "local\n");
        repo.commit_all("local feat commit");
        repo.git(&["checkout", "-q", "main"]);
        (repo, bare)
    }

    /// A repo left on `main` with a worktree-less `feat` diverged from
    /// `origin/feat` (each has a unique commit atop a common base).
    fn repo_branch_diverged() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        repo.git(&["checkout", "-q", "-b", "feat"]);
        repo.git(&["push", "-q", "-u", "origin", "feat"]);
        let base = repo.git(&["rev-parse", "feat"]).trim().to_string();
        repo.write("o.txt", "origin side\n");
        repo.commit_all("origin feat commit");
        repo.git(&["push", "-q", "origin", "feat"]);
        repo.git(&["reset", "-q", "--hard", &base]);
        repo.write("l.txt", "local side\n");
        repo.commit_all("local feat commit");
        repo.git(&["checkout", "-q", "main"]);
        (repo, bare)
    }

    /// A repo left on `main` with a worktree-less `feat` equal to `origin/feat`.
    fn repo_branch_up_to_date() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        repo.git(&["push", "-q", "-u", "origin", "main"]);
        repo.git(&["checkout", "-q", "-b", "feat"]);
        repo.git(&["push", "-q", "-u", "origin", "feat"]);
        repo.git(&["checkout", "-q", "main"]);
        (repo, bare)
    }

    #[test]
    fn branch_fast_forwards_when_behind() {
        let (repo, _bare) = repo_branch_behind();
        let (_t, res) = sync_branch_call(&repo, "feat", false);
        assert_eq!(res.unwrap(), SyncOutcome::FastForwarded);
        // The local ref was moved up to the tracking ref (no worktree involved).
        assert_eq!(
            repo.git(&["rev-parse", "feat"]).trim(),
            repo.git(&["rev-parse", "refs/remotes/origin/feat"]).trim()
        );
    }

    #[test]
    fn branch_pushes_when_ahead() {
        let (repo, bare) = repo_branch_ahead();
        let local = repo.git(&["rev-parse", "feat"]).trim().to_string();
        let (_t, res) = sync_branch_call(&repo, "feat", false);
        assert_eq!(res.unwrap(), SyncOutcome::Pushed);
        assert_eq!(bare.git(&["rev-parse", "feat"]).trim(), local);
    }

    #[test]
    fn branch_ahead_with_no_push_is_up_to_date() {
        let (repo, bare) = repo_branch_ahead();
        let before = bare.git(&["rev-parse", "feat"]).trim().to_string();
        let (_t, res) = sync_branch_call(&repo, "feat", true);
        assert_eq!(res.unwrap(), SyncOutcome::UpToDate);
        assert_eq!(bare.git(&["rev-parse", "feat"]).trim(), before);
    }

    #[test]
    fn branch_diverged_refuses_and_warns() {
        let (repo, bare) = repo_branch_diverged();
        let local = repo.git(&["rev-parse", "feat"]).trim().to_string();
        let origin_before = bare.git(&["rev-parse", "feat"]).trim().to_string();
        let (t, res) = sync_branch_call(&repo, "feat", false);
        assert_eq!(res.unwrap(), SyncOutcome::DivergedNoWorktree);
        assert!(t.err.contents().contains("diverged"));
        assert!(t.err.contents().contains("check it out"));
        // No rewrite and no push.
        assert_eq!(repo.git(&["rev-parse", "feat"]).trim(), local);
        assert_eq!(bare.git(&["rev-parse", "feat"]).trim(), origin_before);
    }

    #[test]
    fn branch_up_to_date_when_equal() {
        let (repo, _bare) = repo_branch_up_to_date();
        let (_t, res) = sync_branch_call(&repo, "feat", false);
        assert_eq!(res.unwrap(), SyncOutcome::UpToDate);
    }

    #[test]
    fn branch_without_upstream_is_no_upstream() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]); // local-only, not checked out
        let (_t, res) = sync_branch_call(&repo, "topic", false);
        assert_eq!(res.unwrap(), SyncOutcome::NoUpstream);
    }

    #[test]
    fn branch_checked_out_elsewhere_is_refused_with_path() {
        // A branch that turns out to be checked out in a worktree must not have its
        // ref moved from here; the guard refuses and names the worktree path.
        let repo = TestRepo::init();
        repo.add_worktree("feat", "../wt-feat");
        let (_t, res) = sync_branch_call(&repo, "feat", false);
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("checked out"), "got {msg:?}");
        assert!(msg.contains("wt-feat"), "got {msg:?}");
    }

    #[test]
    fn run_syncs_worktree_less_branch_by_query() {
        let (repo, _bare) = repo_branch_behind();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(Some("feat"), false, false), false).unwrap();
        assert_eq!(code, 0);
        assert!(t.out.contents().contains("fast-forwarded"));
        assert_eq!(
            repo.git(&["rev-parse", "feat"]).trim(),
            repo.git(&["rev-parse", "refs/remotes/origin/feat"]).trim()
        );
    }

    #[test]
    fn run_json_emits_branch_row_for_worktree_less_branch() {
        let (repo, _bare) = repo_branch_up_to_date();
        let mut t = test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(Some("feat"), false, false), true).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(t.out.contents().trim()).unwrap();
        assert_eq!(v["branch"], serde_json::json!("feat"));
        // A worktree-less branch has no checkout, so its path is the sentinel.
        assert_eq!(v["path"], serde_json::json!("branch://feat"));
    }

    #[test]
    fn outcome_note_covers_diverged_no_worktree() {
        assert!(super::outcome_note(SyncOutcome::DivergedNoWorktree).contains("check out"));
        assert!(super::sync_suffix(SyncOutcome::DivergedNoWorktree).contains("check out"));
    }
}
