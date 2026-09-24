//! `wt prune` — bulk cleanup of merged or stale worktrees (detached ones
//! included), and the local branches they leave behind (spec §7/§12).
//!
//! Selection lives in [`assess`]: each worktree and branch gets a verdict naming
//! why it qualifies and what, if anything, keeps it. This module reports those
//! verdicts and removes the ones nothing keeps.

mod assess;

use std::collections::HashSet;
use std::path::Path;

use crate::cli::PruneArgs;
use crate::commands::{Session, candidate_label, confirm, open_session};
use crate::config::wtconfig;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::porcelain::RawWorktree;
use crate::git::worktrees::{in_progress_branch, in_progress_op};
use crate::git::{
    branch_ref, current_branch, enumerate, is_clean_for_removal, local_branches, ops,
};
use crate::model::Worktree;
use crate::worktree::{build_worktrees, lock_repo};

use assess::{Assessor, Block, MergeTargets, Subject, Verdict};

/// Selects and removes prune candidates after confirmation (spec §7/§12).
pub(crate) fn run(cx: &mut Cx, args: &PruneArgs, json: bool) -> Result<u8> {
    if !args.includes_merged() && !args.includes_gone() && !args.includes_pushed() {
        return Err(Error::usage(
            "prune requires --merged, --gone, --pushed, or --all",
        ));
    }
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let root = session.primary_root.clone();
    // "Gone" and "still on the remote" are only as true as the remote-tracking
    // refs, so the modes that read them refresh them first (dry runs included,
    // so a preview never promises something the real run would not do).
    if args.needs_fetch() {
        fetch_remotes(git, &session.repo, &root)?;
    }
    let worktrees = build_worktrees(&session.repo, git)?;
    // The porcelain records carry what the rows do not: the lock, and a detached
    // worktree's HEAD commit.
    let raws = enumerate(git, &root)?;
    let targets = MergeTargets::resolve(&session.repo);
    let current = current_branch(session.repo.gix());
    let assessor = Assessor {
        git,
        root: &root,
        repo: &session.repo,
        args,
        targets: &targets,
    };

    let mut verdicts: Vec<Verdict> = worktrees
        .iter()
        .enumerate()
        .filter_map(|(i, w)| assessor.worktree(i, w, raw_for(&raws, w)))
        .collect();

    // Branches that keep a worktree (the primary checkout and any branch checked
    // out elsewhere) are left to the worktree path, so a branch is never counted
    // — or deleted — twice. Under `--all` the branch of a worktree being removed
    // is also judged as a bare branch, so one pass clears both.
    let removing: HashSet<usize> = verdicts
        .iter()
        .filter(|v| v.block.is_none())
        .filter_map(|v| match v.subject {
            Subject::Worktree { index, .. } => Some(index),
            Subject::Branch { .. } => None,
        })
        .collect();
    // A branch being rebased or bisected is in use too, although its worktree
    // is detached meanwhile and so names no branch. When a worktree's state
    // cannot be read, the branch it holds is unknown, so every bare branch is
    // kept rather than trusting git to refuse the one in use.
    let mut holder_unreadable = false;
    let held: Vec<String> = worktrees
        .iter()
        .filter(|w| !w.is_missing)
        .filter_map(|w| {
            in_progress_branch(git, &w.path).unwrap_or_else(|error| {
                tracing::warn!(target_wt = %w.path.display(), %error, "prune: cannot read in-progress branch");
                holder_unreadable = true;
                None
            })
        })
        .collect();
    let worktree_branches: HashSet<String> = worktrees
        .iter()
        .enumerate()
        .filter(|(i, _)| !(args.all && removing.contains(i)))
        .filter_map(|(_, w)| w.branch.clone())
        .chain(held)
        .collect();
    let mut branch_verdicts = assessor.branches(current.as_deref(), &worktree_branches)?;
    if holder_unreadable {
        for verdict in &mut branch_verdicts {
            verdict.block = Some(Block::HolderUnreadable);
        }
    }
    verdicts.extend(branch_verdicts);
    let (candidates, skipped): (Vec<Verdict>, Vec<Verdict>) =
        verdicts.into_iter().partition(|v| v.block.is_none());

    // The worktree/branch gap is the signal when a prune surprises someone.
    tracing::debug!(
        default = ?targets.default,
        merge_targets = ?targets.refs,
        worktrees = worktrees.len(),
        candidates = candidates.len(),
        skipped = skipped.len(),
        local_branches = local_branches(session.repo.gix()).map_or(0, |b| b.len()),
        "prune: candidate selection",
    );

    // Whatever qualified but is kept is always said out loud, on stderr (stdout
    // stays the candidate list, and clean for `--json`).
    if !json {
        for verdict in &skipped {
            if let Some(block) = &verdict.block {
                cx.err.line(&format!(
                    "skipping {}: {}",
                    verdict_label(&worktrees, verdict),
                    block.message()
                ))?;
            }
        }
    }

    // `--dry-run` (and the `--json` form) only report the candidate set.
    if args.dry_run || json {
        // Report the empty case explicitly on stderr so a dry-run that finds
        // nothing is never silently blank.
        if candidates.is_empty() && !json {
            cx.err.line("nothing to prune")?;
        }
        for verdict in &candidates {
            if json {
                cx.out.line(&verdict_json(&worktrees, verdict)?)?;
            } else {
                cx.out.line(&format!(
                    "would remove {}",
                    verdict_text(&worktrees, verdict)
                ))?;
            }
        }
        return Ok(0);
    }

    // Nothing is removed that was not listed: no blanket `git worktree prune`,
    // which would drop every missing worktree, including the ones kept above.
    if candidates.is_empty() {
        cx.err.line("nothing to prune")?;
        return Ok(0);
    }

    // Confirmation prompt (unless --force).
    if !args.force {
        cx.err.line("to remove:")?;
        for verdict in &candidates {
            cx.err
                .line(&format!("  {}", verdict_text(&worktrees, verdict)))?;
        }
        if !confirm(cx, "Proceed? [y/N] ")? {
            cx.err.line("aborted")?;
            return Ok(0);
        }
    }

    // Every removal below deletes worktrees, branches, and `wt.*` metadata, so
    // the whole loop is one mutation region under the advisory repo lock (issue
    // #99). `prune` runs no hooks, so nothing inside can re-enter `wt`.
    let lock = lock_repo(&root)?;
    let mut removed = 0_usize;
    // Branches whose worktree was not removed (it turned dirty, or git refused)
    // stay checked out, so their `--all` branch candidate is skipped too.
    // Worktree candidates come first.
    let mut kept: HashSet<&str> = HashSet::new();
    for verdict in &candidates {
        let pruned = match &verdict.subject {
            Subject::Worktree {
                index,
                locked,
                head,
            } => {
                let worktree = &worktrees[*index];
                let pruned = remove_worktree(
                    cx,
                    &assessor,
                    &session,
                    worktree,
                    head.as_deref(),
                    *locked && args.locked,
                )?;
                if !pruned && let Some(branch) = &worktree.branch {
                    kept.insert(branch);
                }
                pruned
            }
            Subject::Branch { name, .. } if kept.contains(name.as_str()) => false,
            Subject::Branch { name, .. } => remove_branch(cx, &assessor, name)?,
        };
        if pruned {
            removed += 1;
        }
    }

    drop(lock);
    tracing::debug!(removed, "prune: done");
    cx.err.line(&format!("pruned {removed} item(s)"))?;
    Ok(0)
}

/// Runs `git fetch --all --prune` when the repository has any remote, so the
/// remote-tracking refs prune reads reflect the remote now. A failed fetch aborts
/// rather than letting a stale ref vouch for commits the remote no longer has.
fn fetch_remotes(git: &dyn GitCli, repo: &Repo, root: &Path) -> Result<()> {
    if repo.gix().remote_names().is_empty() {
        tracing::debug!("prune: no remotes; skipping fetch");
        return Ok(());
    }
    tracing::debug!("prune: fetching remotes");
    ops::fetch_all_prune(git, root).map_err(|e| {
        Error::operation(format!(
            "cannot fetch remotes: {e}\n(pass --no-fetch to trust the last fetch)"
        ))
    })?;
    Ok(())
}

/// The porcelain record for `worktree`, matched by path.
fn raw_for<'a>(raws: &'a [RawWorktree], worktree: &Worktree) -> Option<&'a RawWorktree> {
    raws.iter().find(|raw| raw.path == worktree.path)
}

/// Removes one worktree candidate, returning whether it was removed. `head` is
/// a detached worktree's HEAD as assessed; `unlock` overrides a lock
/// (`--locked` on a locked worktree). This is the per-worktree body of the prune
/// loop (spec §12).
///
/// The guards are re-read here, under the repo lock, because the confirmation
/// prompt may have waited long enough for the worktree to change: new edits, a
/// new commit on a detached HEAD, or a rebase started. A removal git refuses is
/// reported and leaves the worktree — and so its branch — in place.
fn remove_worktree(
    cx: &mut Cx,
    assessor: &Assessor<'_>,
    session: &Session,
    worktree: &Worktree,
    head: Option<&str>,
    unlock: bool,
) -> Result<bool> {
    let (git, root) = (assessor.git, assessor.root);
    let label = candidate_label(worktree);
    if let Some(why) = changed_since_assessed(assessor, worktree, head) {
        cx.err.line(&format!("skipping {label}: {why}"))?;
        return Ok(false);
    }
    let path = worktree.path.to_string_lossy();
    // `--force` because the guards above are the safety decision (untracked
    // files included): git's own check also refuses a worktree with
    // submodules, and a lock needs it twice.
    // For a missing worktree this drops just its admin entry — never a blanket
    // `git worktree prune`, which would also drop missing worktrees prune chose
    // to keep.
    if let Err(error) = ops::worktree_remove(git, root, &path, true, unlock) {
        cx.err.line(&format!("could not remove {label}: {error}"))?;
        tracing::warn!(target_wt = %label, "prune: worktree remove failed");
        return Ok(false);
    }
    delete_merged_branch(assessor, worktree, &session.config);
    if let Some(branch) = &worktree.branch {
        let _ = wtconfig::clear_meta(git, root, branch);
    }
    tracing::debug!(target_wt = %label, "prune: removed worktree");
    Ok(true)
}

/// Why a present worktree may no longer be removed although it was a candidate
/// at selection, or `None` when it is unchanged: an operation now in progress,
/// a detached HEAD that moved off the assessed commit, or (without `--force`)
/// new uncommitted changes. A missing worktree cannot change.
fn changed_since_assessed(
    assessor: &Assessor<'_>,
    worktree: &Worktree,
    head: Option<&str>,
) -> Option<String> {
    if worktree.is_missing {
        // `git worktree remove --force` deletes whatever is at the path, so a
        // directory that came back (a remounted drive) — or one that cannot be
        // checked — must not be treated as gone.
        return match std::fs::symlink_metadata(&worktree.path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Ok(_) => Some("reappeared since it was assessed; run prune again".into()),
            Err(_) => Some(Block::Unreadable.message()),
        };
    }
    match in_progress_op(assessor.git, &worktree.path) {
        Ok(None) => {}
        Ok(Some(op)) => return Some(Block::InProgress(op).message()),
        Err(_) => return Some(Block::Unreadable.message()),
    }
    let moved = "HEAD moved since it was assessed; run prune again";
    if let Some(head) = head {
        let now = assessor.git.run(
            &worktree.path,
            &["rev-parse", "--verify", "--quiet", "HEAD"],
        );
        if now.map(|h| h.trim().to_string()).ok().as_deref() != Some(head) {
            return Some(moved.into());
        }
    }
    // A branch worktree must still be on its branch: one that detached and
    // committed meanwhile holds that commit in its HEAD alone.
    if let Some(branch) = &worktree.branch {
        let now = assessor
            .git
            .run(&worktree.path, &["symbolic-ref", "--quiet", "HEAD"]);
        if now.map(|r| r.trim().to_string()).ok() != Some(branch_ref(branch)) {
            return Some(moved.into());
        }
    }
    if !assessor.args.force && !still_clean(assessor, worktree) {
        return Some(Block::Dirty.message());
    }
    None
}

/// Whether a present worktree is still clean by the same rule as the selection
/// guard (untracked files included). A failed status read counts as dirty, so
/// the guard fails safe.
fn still_clean(assessor: &Assessor<'_>, worktree: &Worktree) -> bool {
    is_clean_for_removal(assessor.git, &worktree.path)
}

/// Deletes one bare-branch candidate, returning whether it was deleted. Its
/// safety — every commit on a remote or the default branch, or every change in
/// the default branch — is re-checked under the lock (it may have moved since
/// selection), and the branch is then deleted with `git branch -D`, since `-d`
/// only knows about HEAD and the upstream. Without `--force` an unsafe branch is
/// skipped. A branch already gone (deleted with its worktree) is skipped
/// quietly, and a delete failure is reported and skipped rather than aborting
/// the whole prune.
fn remove_branch(cx: &mut Cx, assessor: &Assessor<'_>, name: &str) -> Result<bool> {
    let (git, root) = (assessor.git, assessor.root);
    if !branch_exists(git, root, name) {
        tracing::debug!(branch = %name, "prune: branch already deleted");
        return Ok(false);
    }
    if !assessor.args.force && !assessor.branch_is_safe(name) {
        cx.err.line(&format!(
            "skipping {name} (branch): {}",
            Block::Unsafe.message()
        ))?;
        tracing::debug!(branch = %name, "prune: skip unsafe branch");
        return Ok(false);
    }
    match ops::delete_branch(git, root, name, true) {
        Ok(out) if out.success => {
            let _ = wtconfig::clear_meta(git, root, name);
            tracing::debug!(branch = %name, "prune: deleted branch");
            Ok(true)
        }
        Ok(out) => {
            cx.err
                .line(&format!("could not delete {name}: {}", out.stderr.trim()))?;
            tracing::warn!(branch = %name, "prune: branch delete failed");
            Ok(false)
        }
        Err(error) => {
            cx.err.line(&format!("could not delete {name}: {error}"))?;
            tracing::warn!(branch = %name, "prune: branch delete errored");
            Ok(false)
        }
    }
}

/// Whether the local branch `name` still exists.
fn branch_exists(git: &dyn GitCli, root: &Path, name: &str) -> bool {
    git.run_raw(
        root,
        &["rev-parse", "--verify", "--quiet", &branch_ref(name)],
    )
    .is_ok_and(|out| out.success)
}

/// A human label for a verdict's subject (worktree or bare branch).
fn verdict_label(worktrees: &[Worktree], verdict: &Verdict) -> String {
    match &verdict.subject {
        Subject::Worktree { index, .. } => candidate_label(&worktrees[*index]),
        Subject::Branch { name, .. } => format!("{name} (branch)"),
    }
}

/// A candidate's label followed by why it qualifies, e.g. `feat (branch):
/// merged by content`.
fn verdict_text(worktrees: &[Worktree], verdict: &Verdict) -> String {
    format!(
        "{}: {}",
        verdict_label(worktrees, verdict),
        verdict.reason.label()
    )
}

/// A machine-readable (`--json`) line for a prune candidate. A worktree emits its
/// full row; a bare branch emits a small object tagged `"kind": "branch"`.
fn verdict_json(worktrees: &[Worktree], verdict: &Verdict) -> Result<String> {
    match &verdict.subject {
        Subject::Worktree { index, .. } => worktrees[*index].to_json_line(),
        Subject::Branch { name, merged, safe } => Ok(serde_json::json!({
            "branch": name,
            "kind": "branch",
            "merged": merged,
            "safe": safe,
            "reason": verdict.reason.label(),
        })
        .to_string()),
    }
}

/// Deletes the branch of a removed worktree when `wt` created it, the config
/// allows it, and it is merged into the default branch by ancestry or content.
/// Best-effort: a branch left behind is reported as a later branch candidate
/// under `--all`, or simply kept.
fn delete_merged_branch(
    assessor: &Assessor<'_>,
    worktree: &Worktree,
    config: &crate::config::Config,
) {
    let Some(branch) = &worktree.branch else {
        return;
    };
    if !config.remove_delete_merged_branch || assessor.targets.is_default(branch) {
        return;
    }
    let meta = wtconfig::read_meta(assessor.repo.gix(), branch);
    if !meta.created_by_wt {
        return;
    }
    if !assessor.targets.is_merged(
        assessor.git,
        assessor.root,
        assessor.repo,
        &branch_ref(branch),
    ) {
        return;
    }
    match ops::delete_branch(assessor.git, assessor.root, branch, true) {
        Ok(out) if !out.success => {
            tracing::debug!(branch = %branch, stderr = %out.stderr.trim(), "prune: delete merged branch failed");
        }
        Err(error) => {
            tracing::debug!(branch = %branch, %error, "prune: delete merged branch could not run");
        }
        Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::PruneArgs;
    use crate::error::Error;
    use crate::git::cli::RealGit;
    use crate::testutil::{CannedInput, TestRepo, give_upstream, make_wt, wt_dir};

    fn prune_args(merged: bool, gone: bool, dry_run: bool, force: bool) -> PruneArgs {
        PruneArgs {
            merged,
            gone,
            pushed: false,
            all: false,
            locked: false,
            no_fetch: false,
            dry_run,
            force,
        }
    }

    /// `--all` alone (no `--merged`/`--gone`), as a dry run.
    fn all_dry_run() -> PruneArgs {
        PruneArgs {
            all: true,
            ..prune_args(false, false, true, false)
        }
    }

    /// `--pushed` alone, as a dry run.
    fn pushed_dry_run() -> PruneArgs {
        PruneArgs {
            pushed: true,
            ..prune_args(false, false, true, false)
        }
    }

    /// `--all` for real, without `--force` (the confirmation is answered by `-y`).
    fn all_run() -> PruneArgs {
        PruneArgs {
            all: true,
            ..prune_args(false, false, false, false)
        }
    }

    /// Runs prune with `args` and returns stdout (the dry-run report).
    fn report(repo: &TestRepo, args: &PruneArgs) -> String {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, args, false).unwrap();
        t.out.contents()
    }

    /// Runs prune with `args` and returns (stdout, stderr).
    fn report_both(repo: &TestRepo, args: &PruneArgs) -> (String, String) {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, args, false).unwrap();
        (t.out.contents(), t.err.contents())
    }

    /// Runs prune with `args`, answering the prompt with `-y`, and returns stderr.
    fn run_yes(repo: &TestRepo, args: &PruneArgs) -> String {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.assume_yes = true;
        super::run(&mut t.cx, args, false).unwrap();
        t.err.contents()
    }

    /// Records a remote-tracking ref `refs/remotes/origin/<remote_name>` at
    /// `name`'s tip, as if the branch had been pushed there.
    fn push_tip(repo: &TestRepo, name: &str, remote_name: &str) {
        repo.git(&[
            "update-ref",
            &format!("refs/remotes/origin/{remote_name}"),
            &format!("refs/heads/{name}"),
        ]);
    }

    fn has_branch(repo: &TestRepo, name: &str) -> bool {
        !repo.git(&["branch", "--list", name]).trim().is_empty()
    }

    /// A branch at the current tip — an ancestor of the default branch (merged),
    /// with no worktree.
    fn bare_branch(repo: &TestRepo, name: &str) {
        repo.git(&["branch", name]);
    }

    /// A branch carrying its own commit, so it is NOT an ancestor of the default
    /// branch; leaves the repo back on `main`.
    fn diverged_branch(repo: &TestRepo, name: &str) {
        repo.git(&["checkout", "-q", "-b", name]);
        repo.write(&format!("{name}.txt"), "x\n");
        repo.commit_all("diverge");
        repo.git(&["checkout", "-q", "main"]);
    }

    /// Configures an upstream for `name` whose tracking ref does not exist, so
    /// `upstream_of(...).is_gone` is true.
    fn give_gone_upstream(repo: &TestRepo, name: &str) {
        repo.git(&["config", &format!("branch.{name}.remote"), "origin"]);
        repo.git(&[
            "config",
            &format!("branch.{name}.merge"),
            &format!("refs/heads/{name}"),
        ]);
    }

    /// Creates a worktree on `branch` and commits in it so the branch diverges
    /// from main — i.e. it is not merged.
    fn make_unmerged_wt(repo: &TestRepo, branch: &str) {
        make_wt(repo, branch);
        let wt = wt_dir(repo, branch);
        std::fs::write(wt.join("change.txt"), "x\n").unwrap();
        let dir = wt.to_string_lossy().into_owned();
        repo.git(&["-C", &dir, "add", "-A"]);
        repo.git(&["-C", &dir, "commit", "-q", "-m", "unmerged change"]);
    }

    #[test]
    fn requires_a_mode_flag() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err =
            super::run(&mut t.cx, &prune_args(false, false, false, false), false).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn dry_run_reports_merged_candidates_without_removing() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt"); // merged into main (no new commits)
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, true, false), false).unwrap();
        assert!(t.out.contents().contains("would remove merged-wt"));
        // Still present (dry-run).
        assert!(repo.git(&["worktree", "list"]).contains("merged-wt"));
    }

    #[test]
    fn dry_run_reports_nothing_when_no_candidates() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, true, false), false).unwrap();
        // A dry-run that finds nothing says so on stderr; stdout stays empty.
        assert!(t.err.contents().contains("nothing to prune"));
        assert!(t.out.contents().is_empty());
    }

    #[test]
    fn force_prunes_merged_worktree_and_branch() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, true), false).unwrap();
        assert!(!repo.git(&["worktree", "list"]).contains("merged-wt"));
        assert!(
            repo.git(&["branch", "--list", "merged-wt"])
                .trim()
                .is_empty()
        );
    }

    /// Prune's removal loop deletes worktrees, branches, and `wt.*` metadata,
    /// so it runs under the advisory repo lock (issue #99). A concurrent holder
    /// blocks it outright rather than letting it interleave — and nothing is
    /// removed when it does.
    #[test]
    fn prune_is_excluded_by_a_concurrent_lock_holder() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let ws = crate::worktree::Workspace::discover(repo.root(), &t.cx.env, &RealGit).unwrap();
        let held = ws.lock().unwrap();
        let err = super::run(&mut t.cx, &prune_args(true, false, false, true), false).unwrap_err();
        drop(held);
        assert!(matches!(err, Error::LockUnavailable { .. }), "{err:?}");
        assert!(repo.git(&["worktree", "list"]).contains("merged-wt"));
    }

    #[test]
    fn confirmation_yes_prunes() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.input = Box::new(CannedInput::new(&["y"]));
        super::run(&mut t.cx, &prune_args(true, false, false, false), false).unwrap();
        assert!(t.err.contents().contains("Proceed?"));
        assert!(!repo.git(&["worktree", "list"]).contains("merged-wt"));
    }

    /// `--yes` answers the prompt but is not `--force`: a dirty worktree is still
    /// skipped. Keeping the two separable is what makes `-y` safe in a script.
    #[test]
    fn assume_yes_skips_prompt_but_not_the_dirty_guard() {
        let repo = TestRepo::init();
        make_wt(&repo, "clean-wt");
        make_wt(&repo, "dirty-wt");
        std::fs::write(wt_dir(&repo, "dirty-wt").join("README.md"), "dirty\n").unwrap();

        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.assume_yes = true;
        // No CannedInput: reading stdin at all would return "" and abort.
        super::run(&mut t.cx, &prune_args(true, false, false, false), false).unwrap();

        assert!(t.err.contents().contains("Proceed? [y/N] y (--yes)"));
        assert!(
            t.err
                .contents()
                .contains("skipping dirty-wt: uncommitted changes; use --force")
        );
        let list = repo.git(&["worktree", "list"]);
        assert!(!list.contains("clean-wt"), "{list}");
        assert!(list.contains("dirty-wt"), "{list}");
    }

    #[test]
    fn confirmation_no_aborts() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.input = Box::new(CannedInput::new(&["n"]));
        super::run(&mut t.cx, &prune_args(true, false, false, false), false).unwrap();
        assert!(t.err.contents().contains("aborted"));
        assert!(repo.git(&["worktree", "list"]).contains("merged-wt"));
    }

    #[test]
    fn gone_prunes_missing_worktrees() {
        let repo = TestRepo::init();
        make_wt(&repo, "goner");
        let repo_name = repo.root().file_name().unwrap().to_string_lossy();
        let wt_path = repo
            .root()
            .parent()
            .unwrap()
            .join(format!("{repo_name}.worktrees/{repo_name}-goner"));
        std::fs::remove_dir_all(&wt_path).unwrap();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(false, true, false, true), false).unwrap();
        assert!(!repo.git(&["worktree", "list"]).contains("goner"));
    }

    #[test]
    fn json_lists_candidates_without_removing() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, false), true).unwrap();
        let out = t.out.contents();
        assert_eq!(out.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["branch"], serde_json::json!("merged-wt"));
        // --json implies dry-run: still present.
        assert!(repo.git(&["worktree", "list"]).contains("merged-wt"));
    }

    #[test]
    fn merged_bare_branch_is_pruned() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old"); // no worktree, merged into main
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, true), false).unwrap();
        assert!(repo.git(&["branch", "--list", "old"]).trim().is_empty());
        // The default/current branch is never touched.
        assert!(repo.git(&["branch", "--list", "main"]).contains("main"));
    }

    #[test]
    fn gone_merged_bare_branch_is_pruned() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old");
        give_gone_upstream(&repo, "old"); // merged AND upstream gone
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(false, true, false, true), false).unwrap();
        assert!(repo.git(&["branch", "--list", "old"]).trim().is_empty());
    }

    #[test]
    fn gone_unmerged_branch_needs_force() {
        let repo = TestRepo::init();
        diverged_branch(&repo, "wip"); // not an ancestor of main
        give_gone_upstream(&repo, "wip");
        // Without --force the protected branch is skipped (confirmation says yes).
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.input = Box::new(CannedInput::new(&["y"]));
        super::run(&mut t.cx, &prune_args(false, true, false, false), false).unwrap();
        assert!(t.err.contents().contains("use --force"));
        assert!(repo.git(&["branch", "--list", "wip"]).contains("wip"));
        // With --force it is force-deleted.
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(false, true, false, true), false).unwrap();
        assert!(repo.git(&["branch", "--list", "wip"]).trim().is_empty());
    }

    #[test]
    fn all_selects_merged_and_gone() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old"); // merged only
        diverged_branch(&repo, "wip"); // gone only, commits nowhere else
        give_gone_upstream(&repo, "wip");
        let (all, skips) = report_both(&repo, &all_dry_run());
        assert!(all.contains("would remove old (branch): merged"), "{all}");
        // The gone branch qualifies, but its commits exist nowhere else: the
        // preview says it will be skipped rather than promising its removal.
        assert!(!all.contains("wip"), "{all}");
        assert!(
            skips.contains("skipping wip (branch): has work on no remote"),
            "{skips}"
        );
        // Same selection as `--merged --gone` here; each mode alone picks one.
        assert_eq!(all, report(&repo, &prune_args(true, true, true, false)));
        assert!(!report(&repo, &prune_args(true, false, true, false)).contains("wip"));
        assert!(!report(&repo, &prune_args(false, true, true, false)).contains("old"));
        // Nothing was removed.
        assert!(has_branch(&repo, "old"));
        assert!(has_branch(&repo, "wip"));
        // A real `--all` deletes the merged branch but keeps the gone branch whose
        // commits exist nowhere else.
        let err = run_yes(&repo, &all_run());
        assert!(err.contains("skipping wip"), "{err}");
        assert!(!has_branch(&repo, "old"));
        assert!(has_branch(&repo, "wip"));
    }

    #[test]
    fn all_selects_merged_and_missing_worktrees() {
        // `--all` covers both worktree paths: a merged worktree, and a missing
        // one whose branch is unmerged (so only the `--gone` path selects it).
        let repo = TestRepo::init();
        make_wt(&repo, "done");
        make_unmerged_wt(&repo, "lost");
        std::fs::remove_dir_all(wt_dir(&repo, "lost")).unwrap();
        let all = report(&repo, &all_dry_run());
        assert!(all.contains("would remove done: merged\n"), "{all}");
        assert!(all.contains("would remove lost: missing\n"), "{all}");
        assert!(!report(&repo, &prune_args(true, false, true, false)).contains("lost"));
        assert!(!report(&repo, &prune_args(false, true, true, false)).contains("done"));
    }

    #[test]
    fn pushed_branch_is_selected_by_pushed_and_all_only() {
        // Diverged from main but every commit is on origin: neither merged nor
        // gone, yet deleting it loses nothing.
        let repo = TestRepo::init();
        diverged_branch(&repo, "feat");
        give_upstream(&repo, "feat");
        diverged_branch(&repo, "local"); // commits exist only here
        let pushed = report(&repo, &pushed_dry_run());
        assert!(pushed.contains("would remove feat (branch)"), "{pushed}");
        assert!(!pushed.contains("local"), "{pushed}");
        let all = report(&repo, &all_dry_run());
        assert!(all.contains("would remove feat (branch)"), "{all}");
        assert!(!all.contains("local"), "{all}");
        assert!(!report(&repo, &prune_args(true, true, true, false)).contains("feat"));
        // Deleted without --force; the local-only branch is untouched.
        let err = run_yes(&repo, &all_run());
        assert!(!err.contains("skipping"), "{err}");
        assert!(!has_branch(&repo, "feat"));
        assert!(has_branch(&repo, "local"));
    }

    #[test]
    fn pushed_branch_without_upstream_is_selected() {
        // Pushed without `-u`: no upstream, but origin holds the tip.
        let repo = TestRepo::init();
        diverged_branch(&repo, "feat");
        push_tip(&repo, "feat", "feat");
        let pushed = report(&repo, &pushed_dry_run());
        assert!(pushed.contains("would remove feat (branch)"), "{pushed}");
    }

    #[test]
    fn pushed_branch_that_moved_since_push_is_not_selected() {
        let repo = TestRepo::init();
        diverged_branch(&repo, "feat");
        give_upstream(&repo, "feat");
        repo.git(&["checkout", "-q", "feat"]);
        repo.write("more.txt", "x\n");
        repo.commit_all("unpushed");
        repo.git(&["checkout", "-q", "main"]);
        assert!(!report(&repo, &pushed_dry_run()).contains("feat"));
    }

    #[test]
    fn merged_counts_the_remote_default_branch() {
        // Merged on origin while local main lags behind: `--merged` sees it via
        // origin/HEAD's target.
        let repo = TestRepo::init();
        diverged_branch(&repo, "feat");
        repo.git(&["update-ref", "refs/remotes/origin/main", "refs/heads/feat"]);
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        let merged = report(&repo, &prune_args(true, false, true, false));
        assert!(merged.contains("would remove feat (branch)"), "{merged}");
        // Deleted for real without --force.
        run_yes(&repo, &prune_args(true, false, false, false));
        assert!(!has_branch(&repo, "feat"));
        assert!(has_branch(&repo, "main"));
    }

    #[test]
    fn gone_branch_whose_commits_survive_needs_no_force() {
        // Upstream deleted, but the commits live on under another remote ref.
        let repo = TestRepo::init();
        diverged_branch(&repo, "shipped");
        give_gone_upstream(&repo, "shipped");
        push_tip(&repo, "shipped", "release");
        let err = run_yes(&repo, &prune_args(false, true, false, false));
        assert!(!err.contains("skipping"), "{err}");
        assert!(!has_branch(&repo, "shipped"));
    }

    #[test]
    fn all_keeps_the_worktree_of_a_live_pushed_branch() {
        // Pushed and live (upstream present): the branch is recoverable, but the
        // checkout is active work, so `--all` touches neither.
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "live");
        give_upstream(&repo, "live");
        let all = report(&repo, &all_dry_run());
        assert!(!all.contains("live"), "{all}");
    }

    #[test]
    fn all_removes_a_gone_worktree_and_its_recoverable_branch_in_one_pass() {
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "shipped");
        give_gone_upstream(&repo, "shipped");
        push_tip(&repo, "shipped", "release");
        let all = report(&repo, &all_dry_run());
        assert!(
            all.contains("would remove shipped: upstream gone\n"),
            "{all}"
        );
        assert!(
            all.contains("would remove shipped (branch): upstream gone"),
            "{all}"
        );
        // `--gone` alone removes the worktree and keeps the (unmerged) branch.
        assert!(!report(&repo, &prune_args(false, true, true, false)).contains("(branch)"));
        let err = run_yes(&repo, &all_run());
        assert!(err.contains("pruned 2 item(s)"), "{err}");
        assert!(!repo.git(&["worktree", "list"]).contains("shipped"));
        assert!(!has_branch(&repo, "shipped"));
    }

    #[test]
    fn all_removes_a_merged_worktree_and_branch_without_noise() {
        // The merged branch goes with its worktree; the branch candidate then
        // finds it already gone and skips quietly.
        let repo = TestRepo::init();
        make_wt(&repo, "done");
        let err = run_yes(&repo, &all_run());
        assert!(!err.contains("could not delete"), "{err}");
        assert!(!repo.git(&["worktree", "list"]).contains("done"));
        assert!(!has_branch(&repo, "done"));
    }

    #[test]
    fn all_deletes_the_branch_of_a_missing_worktree_in_one_pass() {
        // The missing worktree is still registered until Git reconciles its
        // metadata; the branch delete must not trip over that.
        let repo = TestRepo::init();
        make_wt(&repo, "done");
        std::fs::remove_dir_all(wt_dir(&repo, "done")).unwrap();
        let err = run_yes(&repo, &all_run());
        assert!(!err.contains("could not delete"), "{err}");
        assert!(!repo.git(&["worktree", "list"]).contains("done"));
        assert!(!has_branch(&repo, "done"));
    }

    #[test]
    fn pushed_alone_never_selects_worktrees() {
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "live");
        give_upstream(&repo, "live");
        std::fs::remove_dir_all(wt_dir(&repo, "live")).unwrap(); // even a missing one
        assert!(!report(&repo, &pushed_dry_run()).contains("live"));
    }

    #[test]
    fn all_keeps_the_branch_of_a_skipped_dirty_worktree() {
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "busy");
        give_gone_upstream(&repo, "busy");
        push_tip(&repo, "busy", "release");
        std::fs::write(wt_dir(&repo, "busy").join("README.md"), "dirty\n").unwrap();
        let err = run_yes(&repo, &all_run());
        assert!(err.contains("skipping busy: uncommitted changes"), "{err}");
        assert!(!err.contains("could not delete"), "{err}");
        assert!(has_branch(&repo, "busy"));
    }

    #[test]
    fn a_failed_recoverability_check_counts_as_unsafe() {
        let repo = TestRepo::init();
        // `rev-list` fails on a branch that does not exist; that must never read
        // as "safe to delete".
        let args = all_dry_run();
        with_assessor(&repo, &args, |assessor| {
            assert!(!assessor.recoverable("refs/heads/no-such-branch"));
            assert!(!assessor.branch_is_safe("no-such-branch"));
        });
    }

    /// Runs `f` with an [`super::Assessor`] over `repo` for `args`.
    fn with_assessor(repo: &TestRepo, args: &PruneArgs, f: impl FnOnce(&super::Assessor<'_>)) {
        with_assessor_git(repo, args, &RealGit, f);
    }

    fn with_assessor_git(
        repo: &TestRepo,
        args: &PruneArgs,
        git: &dyn crate::git::cli::GitCli,
        f: impl FnOnce(&super::Assessor<'_>),
    ) {
        let r = crate::git::discover::Repo::discover(repo.root()).unwrap();
        let targets = super::MergeTargets::resolve(&r);
        let assessor = super::Assessor {
            git,
            root: repo.root(),
            repo: &r,
            args,
            targets: &targets,
        };
        f(&assessor);
    }

    #[test]
    fn pushed_alone_satisfies_the_mode_requirement() {
        let repo = TestRepo::init();
        let err = run_yes(&repo, &pushed_dry_run());
        assert!(err.contains("nothing to prune"), "{err}");
    }

    /// A clone-like repo: `main` and `feat` pushed to a bare `origin`, with
    /// `feat` tracking `origin/feat`. Returns (local, remote).
    fn repo_with_remote() -> (TestRepo, TestRepo) {
        let remote = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", remote.root().to_str().unwrap()]);
        diverged_branch(&repo, "feat");
        repo.git(&["push", "-q", "-u", "origin", "main", "feat"]);
        repo.git(&["remote", "set-head", "origin", "main"]);
        (repo, remote)
    }

    fn json_branch(repo: &TestRepo, args: &PruneArgs, name: &str) -> Option<serde_json::Value> {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, args, true).unwrap();
        t.out
            .contents()
            .lines()
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .find(|v| v["branch"] == serde_json::json!(name))
    }

    #[test]
    fn fetch_refreshes_remote_refs_before_judging_safety() {
        // The branch was deleted on origin since the last fetch. The stale
        // tracking ref would vouch for its commits; the fetch shows they are
        // gone, so the branch is no longer safe to delete.
        let (repo, remote) = repo_with_remote();
        remote.git(&["branch", "-D", "feat"]);
        let offline = PruneArgs {
            no_fetch: true,
            ..all_dry_run()
        };
        let stale = json_branch(&repo, &offline, "feat").expect("selected offline");
        assert_eq!(stale["safe"], serde_json::json!(true));
        assert_eq!(stale["reason"], serde_json::json!("pushed"));
        // Fresh, it is gone and unsafe: `--json` lists only what would be
        // removed, so it drops out, and the text preview says why.
        assert!(json_branch(&repo, &all_dry_run(), "feat").is_none());
        let (_, skips) = report_both(&repo, &all_dry_run());
        assert!(
            skips.contains("skipping feat (branch): has work on no remote"),
            "{skips}"
        );
        // With --force it would be removed, and says it is unsafe.
        let forced = PruneArgs {
            force: true,
            ..all_dry_run()
        };
        let fresh = json_branch(&repo, &forced, "feat").expect("selected as gone");
        assert_eq!(fresh["safe"], serde_json::json!(false));
        assert_eq!(fresh["reason"], serde_json::json!("upstream gone"));
        assert!(has_branch(&repo, "feat"));
    }

    #[test]
    fn failed_fetch_aborts_unless_no_fetch() {
        let repo = TestRepo::init();
        repo.git(&[
            "remote",
            "add",
            "origin",
            "/nonexistent/wt-prune-remote.git",
        ]);
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(&mut t.cx, &all_dry_run(), false).unwrap_err();
        assert!(err.to_string().contains("--no-fetch"), "{err}");
        // `--merged` alone reads no remote state, so it never fetches.
        report(&repo, &prune_args(true, false, true, false));
        let offline = PruneArgs {
            no_fetch: true,
            ..all_dry_run()
        };
        report(&repo, &offline);
    }

    #[test]
    fn default_and_current_never_deleted() {
        let repo = TestRepo::init(); // only `main` (default + current)
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, true, false, true), false).unwrap();
        assert!(t.err.contents().contains("nothing to prune"));
        assert!(repo.git(&["branch", "--list", "main"]).contains("main"));
    }

    #[test]
    fn merged_only_skips_unmerged_worktree() {
        // A worktree whose branch is not merged into main must never be a
        // `--merged` prune candidate — a guard against pruning live work.
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "wip");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, true, false), false).unwrap();
        assert!(t.err.contents().contains("nothing to prune"));
    }

    #[test]
    fn dry_run_lists_bare_branch_without_deleting() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, true, false), false).unwrap();
        assert!(t.out.contents().contains("would remove old (branch)"));
        assert!(repo.git(&["branch", "--list", "old"]).contains("old"));
    }

    #[test]
    fn branch_with_worktree_uses_worktree_path() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt"); // branch + worktree
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, true), false).unwrap();
        // Pruned once via the worktree path — not double-counted as a bare branch.
        assert!(!repo.git(&["worktree", "list"]).contains("merged-wt"));
        assert!(
            repo.git(&["branch", "--list", "merged-wt"])
                .trim()
                .is_empty()
        );
        assert!(t.err.contents().contains("pruned 1 item(s)"));
    }

    #[test]
    fn json_lists_bare_branch() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, false), true).unwrap();
        let out = t.out.contents();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["branch"], serde_json::json!("old"));
        assert_eq!(v["kind"], serde_json::json!("branch"));
        assert_eq!(v["merged"], serde_json::json!(true));
        assert_eq!(v["safe"], serde_json::json!(true));
        assert_eq!(v["reason"], serde_json::json!("merged"));
        // --json implies dry-run: still present.
        assert!(repo.git(&["branch", "--list", "old"]).contains("old"));
    }

    /// Adds a detached worktree named `detached-<name>` (beside the repo) at
    /// `rev`, returning its path.
    fn detached_wt(repo: &TestRepo, name: &str, rev: &str) -> std::path::PathBuf {
        let path = repo
            .root()
            .parent()
            .unwrap()
            .join(format!("detached-{name}"));
        repo.git(&[
            "worktree",
            "add",
            "-q",
            "--detach",
            path.to_str().unwrap(),
            rev,
        ]);
        path
    }

    /// Commits a new file in the worktree at `path`, returning the new HEAD.
    fn commit_in(repo: &TestRepo, path: &std::path::Path, file: &str) -> String {
        std::fs::write(path.join(file), "x\n").unwrap();
        let dir = path.to_string_lossy().into_owned();
        repo.git(&["-C", &dir, "add", "-A"]);
        repo.git(&["-C", &dir, "commit", "-q", "-m", file]);
        repo.git(&["-C", &dir, "rev-parse", "HEAD"])
            .trim()
            .to_string()
    }

    fn worktree_listed(repo: &TestRepo, needle: &str) -> bool {
        repo.git(&["worktree", "list"]).contains(needle)
    }

    /// Squash-merges `branch` into `main` (checked out in the primary worktree).
    fn squash_into_main(repo: &TestRepo, branch: &str) {
        repo.git(&["merge", "-q", "--squash", branch]);
        repo.git(&["commit", "-q", "-m", &format!("squash {branch}")]);
    }

    #[test]
    fn a_detached_worktree_on_merged_work_is_pruned_as_merged() {
        let repo = TestRepo::init();
        detached_wt(&repo, "merged", "main");
        let merged = report(&repo, &prune_args(true, false, true, false));
        assert!(merged.contains("detached-merged: merged\n"), "{merged}");
        run_yes(&repo, &prune_args(true, false, false, false));
        assert!(!worktree_listed(&repo, "detached-merged"));
    }

    #[test]
    fn a_detached_worktree_is_pushed_only_when_its_head_is_on_a_remote() {
        let repo = TestRepo::init();
        let pushed = detached_wt(&repo, "pushed", "main");
        let head = commit_in(&repo, &pushed, "pr.txt");
        repo.git(&["update-ref", "refs/remotes/origin/pr-head", &head]);
        let local = detached_wt(&repo, "local", "main");
        commit_in(&repo, &local, "wip.txt");

        let report_pushed = report(&repo, &pushed_dry_run());
        assert!(
            report_pushed.contains("detached-pushed: pushed\n"),
            "{report_pushed}"
        );
        assert!(!report_pushed.contains("detached-local"), "{report_pushed}");
        // Not merged, so `--merged` leaves both.
        assert!(!report(&repo, &prune_args(true, false, true, false)).contains("detached-"));
        let err = run_yes(&repo, &all_run());
        assert!(err.contains("pruned 1 item(s)"), "{err}");
        assert!(!worktree_listed(&repo, "detached-pushed"));
        assert!(worktree_listed(&repo, "detached-local"));
    }

    #[test]
    fn a_dirty_detached_worktree_is_skipped_and_says_so() {
        let repo = TestRepo::init();
        let path = detached_wt(&repo, "dirty", "main");
        std::fs::write(path.join("README.md"), "edited\n").unwrap();
        let (out, err) = report_both(&repo, &all_dry_run());
        assert!(!out.contains("detached-dirty"), "{out}");
        assert!(err.contains("detached-dirty: uncommitted changes"), "{err}");
        run_yes(&repo, &all_run());
        assert!(worktree_listed(&repo, "detached-dirty"));
    }

    #[test]
    fn a_worktree_mid_rebase_is_kept_even_with_force() {
        // Mid-rebase a worktree is detached and may look merged; the rebase is
        // still someone's work in flight.
        let repo = TestRepo::init();
        let path = detached_wt(&repo, "rebasing", "main");
        let admin = repo.git(&[
            "-C",
            path.to_str().unwrap(),
            "rev-parse",
            "--absolute-git-dir",
        ]);
        std::fs::create_dir(std::path::Path::new(admin.trim()).join("rebase-merge")).unwrap();
        let forced = PruneArgs {
            all: true,
            ..prune_args(false, false, false, true)
        };
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &forced, false).unwrap();
        let err = t.err.contents();
        assert!(
            err.contains("detached-rebasing: rebase in progress"),
            "{err}"
        );
        assert!(worktree_listed(&repo, "detached-rebasing"));
    }

    #[test]
    fn a_squash_merged_worktree_with_a_live_upstream_is_merged_by_content() {
        // Pushed, upstream still present, and its work squashed into main under
        // another SHA: no ancestry, not gone, and `--pushed` skips checkouts.
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "chore");
        give_upstream(&repo, "chore");
        squash_into_main(&repo, "chore");
        let merged = report(&repo, &prune_args(true, false, true, false));
        assert!(
            merged.contains("would remove chore: merged by content"),
            "{merged}"
        );
        assert!(!report(&repo, &pushed_dry_run()).contains("chore"));
        let err = run_yes(&repo, &all_run());
        assert!(!err.contains("skipping"), "{err}");
        assert!(!err.contains("could not"), "{err}");
        assert!(!worktree_listed(&repo, "chore"));
        assert!(!has_branch(&repo, "chore"));
    }

    #[test]
    fn a_merge_only_branch_without_upstream_is_merged_by_content() {
        // An integration branch never pushed, holding only merges of work that
        // later reached main through squashes: its SHAs exist nowhere else, its
        // content is all in main.
        let repo = TestRepo::init();
        diverged_branch(&repo, "x");
        diverged_branch(&repo, "y");
        repo.git(&["checkout", "-q", "-b", "integration"]);
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge x", "x"]);
        repo.git(&["merge", "-q", "--no-ff", "-m", "merge y", "y"]);
        repo.git(&["checkout", "-q", "main"]);
        assert!(!report(&repo, &all_dry_run()).contains("integration"));
        squash_into_main(&repo, "x");
        squash_into_main(&repo, "y");
        let all = report(&repo, &all_dry_run());
        assert!(
            all.contains("would remove integration (branch): merged by content"),
            "{all}"
        );
        let err = run_yes(&repo, &all_run());
        assert!(!err.contains("skipping"), "{err}");
        assert!(!has_branch(&repo, "integration"));
    }

    #[test]
    fn a_gone_branch_whose_changes_landed_needs_no_force() {
        // `--gone` alone never ran the content check for selection, but safety
        // still counts a squash-merged branch as losing nothing.
        let repo = TestRepo::init();
        diverged_branch(&repo, "shipped");
        give_gone_upstream(&repo, "shipped");
        squash_into_main(&repo, "shipped");
        let err = run_yes(&repo, &prune_args(false, true, false, false));
        assert!(!err.contains("skipping"), "{err}");
        assert!(!has_branch(&repo, "shipped"));
    }

    #[test]
    fn the_current_worktree_is_never_pruned() {
        let repo = TestRepo::init();
        // Like a clone: without `origin/HEAD` the default branch falls back to
        // the current one, which inside the worktree would be `here` itself.
        repo.git(&["update-ref", "refs/remotes/origin/main", "refs/heads/main"]);
        repo.git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
        make_wt(&repo, "here");
        let inside = wt_dir(&repo, "here");
        let mut t = crate::testutil::test_cx(&[], inside.to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, true), false).unwrap();
        let err = t.err.contents();
        assert!(
            err.contains("skipping here: it is the current worktree"),
            "{err}"
        );
        assert!(worktree_listed(&repo, "here"));
        assert!(has_branch(&repo, "here"));
    }

    #[test]
    fn a_locked_worktree_is_skipped_unless_locked_is_passed() {
        let repo = TestRepo::init();
        make_wt(&repo, "agent");
        let path = wt_dir(&repo, "agent");
        repo.git(&[
            "worktree",
            "lock",
            "--reason",
            "claimed by agent",
            path.to_str().unwrap(),
        ]);
        let (out, err) = report_both(&repo, &all_dry_run());
        assert!(!out.contains("agent"), "{out}");
        assert!(
            err.contains("skipping agent: locked (claimed by agent); use --locked"),
            "{err}"
        );
        // `--force` is not an override for a lock.
        let forced = PruneArgs {
            all: true,
            ..prune_args(false, false, false, true)
        };
        run_yes(&repo, &forced);
        assert!(worktree_listed(&repo, "agent"));
        assert!(has_branch(&repo, "agent"));
        let unlocked = PruneArgs {
            locked: true,
            ..all_run()
        };
        let err = run_yes(&repo, &unlocked);
        assert!(!err.contains("could not"), "{err}");
        assert!(!worktree_listed(&repo, "agent"));
        assert!(!has_branch(&repo, "agent"));
    }

    #[test]
    fn a_missing_locked_worktree_is_reconciled_with_locked() {
        let repo = TestRepo::init();
        make_wt(&repo, "ghost");
        let path = wt_dir(&repo, "ghost");
        repo.git(&["worktree", "lock", path.to_str().unwrap()]);
        std::fs::remove_dir_all(&path).unwrap();
        let err = run_yes(&repo, &all_run());
        assert!(
            err.contains("skipping ghost: locked; use --locked"),
            "{err}"
        );
        assert!(worktree_listed(&repo, "ghost"));
        let unlocked = PruneArgs {
            locked: true,
            ..all_run()
        };
        run_yes(&repo, &unlocked);
        assert!(!worktree_listed(&repo, "ghost"));
        assert!(!has_branch(&repo, "ghost"));
    }

    /// Real git, except `worktree remove` is refused.
    struct RefusesRemove;
    impl crate::git::cli::GitCli for RefusesRemove {
        fn run_raw(
            &self,
            repo: &std::path::Path,
            args: &[&str],
        ) -> crate::error::Result<crate::git::cli::GitOutput> {
            if args.starts_with(&["worktree", "remove"]) {
                return Ok(crate::git::cli::GitOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "fatal: refused".into(),
                });
            }
            RealGit.run_raw(repo, args)
        }
    }

    #[test]
    fn a_refused_worktree_removal_is_reported_and_not_counted() {
        let repo = TestRepo::init();
        make_wt(&repo, "stuck");
        let mut t = crate::testutil::test_cx_with_git(
            &[],
            repo.root().to_str().unwrap(),
            std::sync::Arc::new(RefusesRemove),
        );
        t.cx.assume_yes = true;
        super::run(&mut t.cx, &all_run(), false).unwrap();
        let err = t.err.contents();
        assert!(err.contains("could not remove stuck: "), "{err}");
        assert!(err.contains("fatal: refused"), "{err}");
        assert!(err.contains("pruned 0 item(s)"), "{err}");
        // Its branch stays with it rather than failing a "checked out" delete.
        assert!(!err.contains("could not delete"), "{err}");
        assert!(worktree_listed(&repo, "stuck"));
        assert!(has_branch(&repo, "stuck"));
    }

    #[test]
    fn a_worktree_dirtied_after_the_prompt_is_kept() {
        let repo = TestRepo::init();
        make_wt(&repo, "late");
        let path = wt_dir(&repo, "late");
        // Stands in for an edit made while the prompt waited: selection saw it
        // clean, the removal re-reads it.
        let row = row_for(&repo, "late");
        let args = all_run();
        with_assessor(&repo, &args, |assessor| {
            assert_eq!(super::changed_since_assessed(assessor, &row, None), None);
        });
        std::fs::write(path.join("README.md"), "late edit\n").unwrap();
        with_assessor(&repo, &args, |assessor| {
            let why = super::changed_since_assessed(assessor, &row, None).unwrap();
            assert!(why.contains("uncommitted changes"), "{why}");
        });
    }

    #[test]
    fn an_untracked_file_added_after_the_prompt_keeps_the_worktree() {
        let repo = TestRepo::init();
        make_wt(&repo, "late-new");
        let path = wt_dir(&repo, "late-new");
        let row = row_for(&repo, "late-new");
        let args = all_run();
        std::fs::write(path.join("new.rs"), "fn main() {}\n").unwrap();
        with_assessor(&repo, &args, |assessor| {
            let why = super::changed_since_assessed(assessor, &row, None).unwrap();
            assert!(why.contains("uncommitted changes"), "{why}");
        });
    }

    #[test]
    fn untracked_files_keep_a_selected_worktree_whatever_the_config() {
        // `remove.untracked_blocks` defaults to false, but removal passes
        // `--force`: prune's own guard is the only thing between git and these
        // files, for every way a worktree can qualify. The repository also
        // hides untracked files from `git status`, which must not matter.
        let repo = TestRepo::init();
        repo.git(&["config", "status.showUntrackedFiles", "no"]);
        let detached = detached_wt(&repo, "fresh", "main");
        std::fs::write(detached.join("newfile.rs"), "wip\n").unwrap();
        make_unmerged_wt(&repo, "squashed");
        give_upstream(&repo, "squashed");
        squash_into_main(&repo, "squashed");
        std::fs::write(wt_dir(&repo, "squashed").join("notes.md"), "wip\n").unwrap();
        make_wt(&repo, "held");
        let held = wt_dir(&repo, "held");
        std::fs::write(held.join("scratch.txt"), "wip\n").unwrap();
        repo.git(&["worktree", "lock", held.to_str().unwrap()]);

        let (out, err) = report_both(
            &repo,
            &PruneArgs {
                locked: true,
                ..all_dry_run()
            },
        );
        for name in ["detached-fresh", "squashed", "held"] {
            assert!(!out.contains(name), "{out}");
            assert!(
                err.contains(&format!("{name}: uncommitted changes")),
                "{err}"
            );
        }
        let unlocked = PruneArgs {
            locked: true,
            ..all_run()
        };
        run_yes(&repo, &unlocked);
        assert!(detached.join("newfile.rs").exists());
        assert!(wt_dir(&repo, "squashed").join("notes.md").exists());
        assert!(held.join("scratch.txt").exists());

        // `--force` is the explicit override.
        let forced = PruneArgs {
            all: true,
            locked: true,
            ..prune_args(false, false, false, true)
        };
        run_yes(&repo, &forced);
        assert!(!worktree_listed(&repo, "detached-fresh"));
        assert!(!worktree_listed(&repo, "squashed"));
        assert!(!worktree_listed(&repo, "held"));
    }

    /// The worktree row whose path ends with `suffix`.
    fn row_for(repo: &TestRepo, suffix: &str) -> crate::model::Worktree {
        crate::worktree::build_worktrees(
            &crate::git::discover::Repo::discover(repo.root()).unwrap(),
            &RealGit,
        )
        .unwrap()
        .into_iter()
        .find(|w| w.path.to_string_lossy().ends_with(suffix))
        .unwrap()
    }

    #[test]
    fn a_missing_worktree_that_reappeared_after_the_prompt_is_kept() {
        // Assessed while its drive was unmounted; back by the time removal runs.
        // `worktree remove --force` would delete it, uncommitted edits and all.
        let repo = TestRepo::init();
        make_wt(&repo, "remounted");
        let path = wt_dir(&repo, "remounted");
        let mut row = row_for(&repo, "remounted");
        row.is_missing = true;
        let args = all_run();
        with_assessor(&repo, &args, |assessor| {
            let why = super::changed_since_assessed(assessor, &row, None).unwrap();
            assert!(why.contains("reappeared"), "{why}");
        });
        std::fs::remove_dir_all(&path).unwrap();
        with_assessor(&repo, &args, |assessor| {
            assert_eq!(super::changed_since_assessed(assessor, &row, None), None);
        });
    }

    #[test]
    fn a_branch_worktree_that_detached_after_the_prompt_is_kept() {
        let repo = TestRepo::init();
        make_wt(&repo, "hopper");
        let path = wt_dir(&repo, "hopper");
        let row = row_for(&repo, "hopper");
        let dir = path.to_string_lossy().into_owned();
        repo.git(&["-C", &dir, "checkout", "-q", "--detach"]);
        commit_in(&repo, &path, "stray.txt");
        let args = all_run();
        with_assessor(&repo, &args, |assessor| {
            let why = super::changed_since_assessed(assessor, &row, None).unwrap();
            assert!(why.contains("HEAD moved"), "{why}");
        });
    }

    #[test]
    fn a_detached_worktree_that_committed_after_the_prompt_is_kept() {
        // Clean both times, but its HEAD — the only holder of the new commit —
        // moved off the commit that was judged safe.
        let repo = TestRepo::init();
        let path = detached_wt(&repo, "busy-agent", "main");
        let assessed = repo.git(&["rev-parse", "main"]).trim().to_string();
        let row = row_for(&repo, "detached-busy-agent");
        let args = all_run();
        with_assessor(&repo, &args, |assessor| {
            assert_eq!(
                super::changed_since_assessed(assessor, &row, Some(&assessed)),
                None
            );
        });
        commit_in(&repo, &path, "late.txt");
        with_assessor(&repo, &args, |assessor| {
            let why = super::changed_since_assessed(assessor, &row, Some(&assessed)).unwrap();
            assert!(why.contains("HEAD moved"), "{why}");
        });
    }

    #[test]
    fn a_worktree_that_started_a_rebase_after_the_prompt_is_kept() {
        let repo = TestRepo::init();
        make_wt(&repo, "rebasing");
        let row = row_for(&repo, "rebasing");
        let admin = repo.git(&[
            "-C",
            wt_dir(&repo, "rebasing").to_str().unwrap(),
            "rev-parse",
            "--absolute-git-dir",
        ]);
        std::fs::create_dir(std::path::Path::new(admin.trim()).join("rebase-merge")).unwrap();
        let args = all_run();
        with_assessor(&repo, &args, |assessor| {
            let why = super::changed_since_assessed(assessor, &row, None).unwrap();
            assert_eq!(why, "rebase in progress");
        });
    }

    #[test]
    fn a_missing_detached_worktree_holding_the_only_copy_needs_force() {
        let repo = TestRepo::init();
        let path = detached_wt(&repo, "orphan", "main");
        commit_in(&repo, &path, "only-here.txt");
        std::fs::remove_dir_all(&path).unwrap();
        let err = run_yes(&repo, &all_run());
        assert!(
            err.contains("detached-orphan: has work on no remote"),
            "{err}"
        );
        assert!(worktree_listed(&repo, "detached-orphan"));
        let forced = PruneArgs {
            all: true,
            ..prune_args(false, false, false, true)
        };
        run_yes(&repo, &forced);
        assert!(!worktree_listed(&repo, "detached-orphan"));
    }

    #[test]
    fn a_missing_worktree_no_mode_selected_is_left_registered() {
        // `--merged` does not select a missing, unmerged worktree, and prune no
        // longer runs a blanket `git worktree prune` that would drop it anyway.
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "offline");
        std::fs::remove_dir_all(wt_dir(&repo, "offline")).unwrap();
        make_wt(&repo, "done"); // one real candidate, so the removal loop runs
        let err = run_yes(&repo, &prune_args(true, false, false, false));
        assert!(err.contains("pruned 1 item(s)"), "{err}");
        assert!(worktree_listed(&repo, "offline"));
        // The empty case is just as hands-off.
        run_yes(&repo, &prune_args(true, false, false, false));
        assert!(worktree_listed(&repo, "offline"));
    }

    #[test]
    fn a_missing_detached_worktree_whose_head_survives_is_pruned() {
        let repo = TestRepo::init();
        let path = detached_wt(&repo, "spent", "main");
        std::fs::remove_dir_all(&path).unwrap();
        let err = run_yes(&repo, &all_run());
        assert!(!err.contains("skipping"), "{err}");
        assert!(!worktree_listed(&repo, "detached-spent"));
    }

    /// Real git, except that a worktree whose path ends in `blind` cannot
    /// resolve its admin directory — its in-progress state is unreadable.
    struct BlindAdminDir;
    impl crate::git::cli::GitCli for BlindAdminDir {
        fn run_raw(
            &self,
            repo: &std::path::Path,
            args: &[&str],
        ) -> crate::error::Result<crate::git::cli::GitOutput> {
            if args == ["rev-parse", "--absolute-git-dir"]
                && repo.to_string_lossy().ends_with("blind")
            {
                return Ok(crate::git::cli::GitOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "fatal: unreadable".into(),
                });
            }
            RealGit.run_raw(repo, args)
        }
    }

    #[test]
    fn a_worktree_whose_state_turns_unreadable_after_the_prompt_is_kept() {
        let repo = TestRepo::init();
        make_wt(&repo, "blind");
        let row = row_for(&repo, "blind");
        let args = all_run();
        with_assessor(&repo, &args, |assessor| {
            assert_eq!(super::changed_since_assessed(assessor, &row, None), None);
        });
        with_assessor_git(&repo, &args, &BlindAdminDir, |assessor| {
            let why = super::changed_since_assessed(assessor, &row, None);
            assert_eq!(why, Some(super::Block::Unreadable.message()));
        });
    }

    #[test]
    fn a_missing_worktree_whose_path_cannot_be_checked_is_kept() {
        // `git worktree remove --force` would delete whatever is at the path,
        // so a path whose existence cannot be read is not treated as gone.
        let repo = TestRepo::init();
        make_wt(&repo, "offline");
        let mut row = row_for(&repo, "offline");
        let scratch = TestRepo::init();
        let parent = scratch.root().join("mount");
        row.path = parent.join("offline");
        row.is_missing = true;
        let args = all_run();
        with_assessor(&repo, &args, |assessor| {
            assert_eq!(super::changed_since_assessed(assessor, &row, None), None);
        });
        // A file where a directory should be: the lookup fails with ENOTDIR,
        // not NotFound (and unlike a permission error, even for root).
        std::fs::write(&parent, "not a directory\n").unwrap();
        with_assessor(&repo, &args, |assessor| {
            assert_eq!(
                super::changed_since_assessed(assessor, &row, None),
                Some(super::Block::Unreadable.message())
            );
        });
    }

    #[test]
    fn an_unreadable_worktree_keeps_every_bare_branch() {
        // The branch its rebase or bisect would return to is unknown, so no
        // bare branch is deleted on the strength of git refusing the held one.
        let repo = TestRepo::init();
        make_wt(&repo, "blind");
        repo.git(&["branch", "done"]);
        let mut t = crate::testutil::test_cx_with_git(
            &[],
            repo.root().to_str().unwrap(),
            std::sync::Arc::new(BlindAdminDir),
        );
        t.cx.assume_yes = true;
        super::run(&mut t.cx, &all_run(), false).unwrap();
        let err = t.err.contents();
        assert!(
            err.contains(
                "skipping done (branch): a worktree's rebase or bisect state cannot be read"
            ),
            "{err}"
        );
        assert!(has_branch(&repo, "done"));
        assert!(worktree_listed(&repo, "blind"));
    }

    #[test]
    fn a_worktree_without_a_porcelain_record_is_unreadable() {
        let repo = TestRepo::init();
        make_wt(&repo, "unlisted");
        let row = row_for(&repo, "unlisted");
        let args = all_dry_run();
        with_assessor(&repo, &args, |assessor| {
            let verdict = assessor.worktree(1, &row, None).expect("merged");
            assert_eq!(verdict.block, Some(super::Block::Unreadable));
        });
    }

    #[test]
    fn a_branch_being_rebased_is_not_a_bare_candidate() {
        // Mid-rebase its worktree is detached and names no branch, but the
        // branch is still in use: deleting it would strand the rebase.
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "feat");
        give_upstream(&repo, "feat"); // pushed, so `--pushed` would pick it bare
        repo.write("change.txt", "main\n");
        repo.commit_all("main conflicts");
        let dir = wt_dir(&repo, "feat").to_string_lossy().into_owned();
        let out = crate::git::cli::GitCli::run_raw(
            &RealGit,
            std::path::Path::new(&dir),
            &["rebase", "main"],
        )
        .unwrap();
        assert!(!out.success, "the rebase should stop on the conflict");
        let (all, _) = report_both(&repo, &all_dry_run());
        assert!(!all.contains("feat (branch)"), "{all}");
        run_yes(&repo, &all_run());
        assert!(has_branch(&repo, "feat"));
    }

    #[test]
    fn merged_is_reported_the_same_whatever_the_mode() {
        // A pushed branch whose work was squashed into main: `--pushed` picks it,
        // and still reports it merged.
        let repo = TestRepo::init();
        diverged_branch(&repo, "feat");
        give_upstream(&repo, "feat");
        squash_into_main(&repo, "feat");
        let pushed = json_branch(&repo, &pushed_dry_run(), "feat").expect("pushed");
        assert_eq!(pushed["reason"], serde_json::json!("pushed"));
        assert_eq!(pushed["merged"], serde_json::json!(true));
        let all = json_branch(&repo, &all_dry_run(), "feat").expect("merged");
        assert_eq!(all["merged"], serde_json::json!(true));
    }
}
