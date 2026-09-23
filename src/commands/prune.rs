//! `wt prune` — bulk cleanup of merged or stale worktrees, and the local
//! branches they leave behind (spec §7/§12).

use std::collections::HashSet;
use std::path::Path;

use crate::cli::PruneArgs;
use crate::commands::{Session, candidate_label, confirm, open_session, run_best_effort};
use crate::config::wtconfig;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::aheadbehind::is_recoverable;
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{
    branch_ref, current_branch, default_branch, default_tracking_ref, is_ancestor, local_branches,
    ops, resolve_hex, upstream_of,
};
use crate::model::Worktree;
use crate::worktree::{build_worktrees, guard_status, lock_repo};

/// A prune target: an existing worktree (an index into the worktree list) or a
/// bare local branch — one with no worktree — that qualified for removal.
enum Candidate {
    /// An existing worktree, identified by its index in the worktree list.
    Worktree(usize),
    /// A local branch with no worktree. `merged` records whether it is an
    /// ancestor of the default branch; `safe` whether every commit on it is also
    /// on a remote-tracking ref or the default branch, so deleting it loses
    /// nothing. An unsafe branch (a gone branch with local-only commits) needs
    /// `--force`.
    Branch {
        name: String,
        merged: bool,
        safe: bool,
    },
}

/// The refs a branch counts as merged into: the local default branch and, when
/// `origin/HEAD` is set, its remote-tracking ref — so work merged on the remote
/// is seen even while the local default lags behind.
struct MergeTargets {
    /// The default branch's short name (never itself a prune candidate).
    default: Option<String>,
    /// Full refs to test ancestry against, each known to resolve.
    refs: Vec<String>,
}

impl MergeTargets {
    fn resolve(repo: &Repo) -> Self {
        let default = default_branch(repo.gix());
        let refs = default
            .as_deref()
            .map(branch_ref)
            .into_iter()
            .chain(default_tracking_ref(repo.gix()))
            .filter(|r| resolve_hex(repo.gix(), r).is_some())
            .collect();
        MergeTargets { default, refs }
    }

    /// Whether `branch` is an ancestor of any target. The default branch itself
    /// is never "merged" (it is trivially its own ancestor).
    fn is_merged(&self, repo: &Repo, branch: &str) -> bool {
        self.default.as_deref() != Some(branch)
            && self
                .refs
                .iter()
                .any(|target| is_ancestor(repo.gix(), &branch_ref(branch), target))
    }

    /// The local default branch ref, if it resolves — the one non-remote ref
    /// whose commits count as kept by the recoverability check.
    fn local_default_ref(&self) -> Option<&str> {
        let local = branch_ref(self.default.as_deref()?);
        self.refs.iter().find(|r| **r == local).map(String::as_str)
    }
}

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
    let targets = MergeTargets::resolve(&session.repo);
    let current = current_branch(session.repo.gix());

    let mut candidates: Vec<Candidate> = worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| !w.is_main && is_candidate(&session.repo, w, args, &targets))
        .map(|(i, _)| Candidate::Worktree(i))
        .collect();

    // Branches that keep a worktree (the primary checkout and any branch checked
    // out elsewhere) are left to the worktree path, so a branch is never counted
    // — or deleted — twice. Under `--all` the branch of a worktree being removed
    // is also judged as a bare branch, so one pass clears both.
    let worktree_branches: HashSet<String> = worktrees
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            !(args.all
                && candidates
                    .iter()
                    .any(|c| matches!(c, Candidate::Worktree(j) if j == i)))
        })
        .filter_map(|(_, w)| w.branch.clone())
        .collect();
    candidates.extend(branch_candidates(
        git,
        &root,
        &session.repo,
        args,
        &targets,
        &current,
        &worktree_branches,
    )?);

    // The worktree/branch gap is the signal when a prune surprises someone.
    tracing::debug!(
        default = ?targets.default,
        merge_targets = ?targets.refs,
        worktrees = worktrees.len(),
        candidates = candidates.len(),
        local_branches = local_branches(session.repo.gix()).map_or(0, |b| b.len()),
        "prune: candidate selection",
    );

    // `--dry-run` (and the `--json` form) only report the candidate set.
    if args.dry_run || json {
        // Report the empty case explicitly on stderr (stdout stays clean for
        // `--json`) so a dry-run that finds nothing is never silently blank.
        if candidates.is_empty() && !json {
            cx.err.line("nothing to prune")?;
        }
        for candidate in &candidates {
            if json {
                cx.out.line(&candidate_json(&worktrees, candidate)?)?;
            } else {
                cx.out.line(&format!(
                    "would remove {}",
                    candidate_text(&worktrees, candidate)
                ))?;
            }
        }
        return Ok(0);
    }

    if candidates.is_empty() {
        cx.err.line("nothing to prune")?;
        let _lock = lock_repo(&root)?;
        ops::worktree_prune(git, &root)?;
        return Ok(0);
    }

    // Confirmation prompt (unless --force).
    if !args.force {
        cx.err.line("to remove:")?;
        for candidate in &candidates {
            cx.err
                .line(&format!("  {}", candidate_text(&worktrees, candidate)))?;
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
    // Branches whose worktree was skipped (dirty) stay checked out, so their
    // `--all` branch candidate is skipped too. Worktree candidates come first.
    let mut kept: HashSet<&str> = HashSet::new();
    let mut reconciled = false;
    for candidate in &candidates {
        // A missing worktree stays registered until Git's worktree metadata is
        // reconciled, and Git refuses to delete a branch it still sees checked
        // out — so reconcile once, after the worktrees and before the branches.
        if !reconciled && matches!(candidate, Candidate::Branch { .. }) {
            ops::worktree_prune(git, &root)?;
            reconciled = true;
        }
        let pruned = match candidate {
            Candidate::Worktree(index) => {
                let worktree = &worktrees[*index];
                let pruned = remove_worktree(cx, git, &session, &root, worktree, args, &targets)?;
                if !pruned && let Some(branch) = &worktree.branch {
                    kept.insert(branch);
                }
                pruned
            }
            Candidate::Branch { name, .. } if kept.contains(name.as_str()) => false,
            Candidate::Branch { name, safe, .. } => {
                remove_branch(cx, git, &root, name, *safe, args.force, &targets)?
            }
        };
        if pruned {
            removed += 1;
        }
    }

    // Reconcile Git's worktree admin metadata (equivalent to `git worktree prune`).
    ops::worktree_prune(git, &root)?;
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

/// Selects local branches that have no worktree but qualify for pruning: merged
/// into the default branch (`--merged`), with a gone upstream (`--gone`), with
/// every commit on a remote or the default branch (`--pushed`), or any of these
/// (`--all`). The default branch and the current branch are never selected, and
/// branches that keep a worktree are left to the worktree path.
fn branch_candidates(
    git: &dyn GitCli,
    root: &Path,
    repo: &Repo,
    args: &PruneArgs,
    targets: &MergeTargets,
    current: &Option<String>,
    worktree_branches: &HashSet<String>,
) -> Result<Vec<Candidate>> {
    let keep: Vec<&str> = targets.local_default_ref().into_iter().collect();
    let mut out = Vec::new();
    for branch in local_branches(repo.gix())? {
        if worktree_branches.contains(&branch)
            || targets.default.as_deref() == Some(branch.as_str())
            || current.as_deref() == Some(branch.as_str())
        {
            continue;
        }
        let merged = targets.is_merged(repo, &branch);
        let gone = upstream_of(repo.gix(), &branch).is_some_and(|u| u.is_gone);
        let by_mode = (args.includes_merged() && merged) || (args.includes_gone() && gone);
        // The recoverability check costs a subprocess; skip it for a branch no
        // selected mode could pick.
        if !by_mode && !args.includes_pushed() {
            continue;
        }
        let safe = branch_is_recoverable(git, root, &branch, &keep);
        tracing::trace!(branch = %branch, merged, gone, safe, "prune: branch classified");
        if by_mode || (args.includes_pushed() && safe) {
            out.push(Candidate::Branch {
                name: branch,
                merged,
                safe,
            });
        }
    }
    Ok(out)
}

/// [`is_recoverable`] for a local branch, treating a failed check as unsafe so a
/// git error can never be the reason a branch is deleted.
fn branch_is_recoverable(git: &dyn GitCli, root: &Path, branch: &str, keep: &[&str]) -> bool {
    is_recoverable(git, root, &branch_ref(branch), keep).unwrap_or_else(|e| {
        tracing::warn!(branch = %branch, error = %e, "prune: recoverability check failed");
        false
    })
}

/// Removes one worktree candidate, returning whether it was removed (a dirty
/// worktree is skipped unless `--force`). This is the per-worktree body of the
/// prune loop (spec §12).
fn remove_worktree(
    cx: &mut Cx,
    git: &dyn GitCli,
    session: &Session,
    root: &Path,
    worktree: &Worktree,
    args: &PruneArgs,
    targets: &MergeTargets,
) -> Result<bool> {
    // Dirty worktrees are skipped unless --force (spec §12).
    if !args.force && guard_status(worktree, session.config.remove_untracked_blocks).dirty {
        cx.err.line(&format!(
            "skipping dirty worktree {}",
            candidate_label(worktree)
        ))?;
        return Ok(false);
    }
    if !worktree.is_missing {
        let path = worktree.path.to_string_lossy();
        run_best_effort(
            git,
            root,
            &["worktree", "remove", "--force", &path],
            "prune: worktree remove",
        );
    }
    delete_merged_branch(git, &session.repo, root, worktree, &session.config, targets);
    if let Some(branch) = &worktree.branch {
        let _ = wtconfig::clear_meta(git, root, branch);
    }
    tracing::debug!(target_wt = %candidate_label(worktree), "prune: removed worktree");
    Ok(true)
}

/// Deletes one bare-branch candidate, returning whether it was deleted. A safe
/// branch — every commit also on a remote or the default branch — is re-checked
/// under the lock (it may have moved since selection) and then deleted with
/// `git branch -D`, since `-d` only knows about HEAD and the upstream. An unsafe
/// branch may hold commits found nowhere else, so it is skipped unless `--force`.
/// A branch already gone (deleted with its worktree) is skipped quietly, and a
/// delete failure is reported and skipped rather than aborting the whole prune.
fn remove_branch(
    cx: &mut Cx,
    git: &dyn GitCli,
    root: &Path,
    name: &str,
    safe: bool,
    force: bool,
    targets: &MergeTargets,
) -> Result<bool> {
    if !branch_exists(git, root, name) {
        tracing::debug!(branch = %name, "prune: branch already deleted");
        return Ok(false);
    }
    let keep: Vec<&str> = targets.local_default_ref().into_iter().collect();
    if !force && !(safe && branch_is_recoverable(git, root, name, &keep)) {
        cx.err.line(&format!(
            "skipping {name}: branch has commits on no remote or default branch; use --force"
        ))?;
        tracing::debug!(branch = %name, "prune: skip unrecoverable branch");
        return Ok(false);
    }
    match ops::delete_branch(git, root, name, true) {
        Ok(out) if out.success => {
            let _ = wtconfig::clear_meta(git, root, name);
            tracing::debug!(branch = %name, safe, "prune: deleted branch");
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

/// A human label for a prune candidate (worktree or bare branch).
fn candidate_text(worktrees: &[Worktree], candidate: &Candidate) -> String {
    match candidate {
        Candidate::Worktree(index) => candidate_label(&worktrees[*index]),
        Candidate::Branch { name, .. } => format!("{name} (branch)"),
    }
}

/// A machine-readable (`--json`) line for a prune candidate. A worktree emits its
/// full row; a bare branch emits a small object tagged `"kind": "branch"`.
fn candidate_json(worktrees: &[Worktree], candidate: &Candidate) -> Result<String> {
    match candidate {
        Candidate::Worktree(index) => worktrees[*index].to_json_line(),
        Candidate::Branch { name, merged, safe } => Ok(serde_json::json!({
            "branch": name,
            "kind": "branch",
            "merged": merged,
            "safe": safe,
        })
        .to_string()),
    }
}

/// Whether a worktree is a prune candidate for the given flags.
fn is_candidate(
    repo: &crate::git::discover::Repo,
    worktree: &Worktree,
    args: &PruneArgs,
    targets: &MergeTargets,
) -> bool {
    // `is_merged` never counts the default branch itself, so a worktree checked
    // out on the default branch is never pruned as merged.
    if args.includes_merged()
        && let Some(branch) = &worktree.branch
        && targets.is_merged(repo, branch)
    {
        return true;
    }
    if args.includes_gone() && (worktree.is_missing || upstream_is_gone(repo, worktree)) {
        return true;
    }
    false
}

/// Whether the worktree's upstream is configured but gone (offline check).
fn upstream_is_gone(repo: &crate::git::discover::Repo, worktree: &Worktree) -> bool {
    worktree
        .branch
        .as_deref()
        .and_then(|b| upstream_of(repo.gix(), b))
        .is_some_and(|u| u.is_gone)
}

/// Deletes a wt-created branch that is fully merged into the default branch.
fn delete_merged_branch(
    git: &dyn GitCli,
    repo: &crate::git::discover::Repo,
    root: &Path,
    worktree: &Worktree,
    config: &crate::config::Config,
    targets: &MergeTargets,
) {
    let Some(branch) = &worktree.branch else {
        return;
    };
    if !config.remove_delete_merged_branch {
        return;
    }
    let meta = wtconfig::read_meta(repo.gix(), branch);
    if !meta.created_by_wt {
        return;
    }
    if targets.is_merged(repo, branch) {
        run_best_effort(
            git,
            root,
            &["branch", "-D", branch],
            "prune: delete merged branch",
        );
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
                .contains("skipping dirty worktree dirty-wt")
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
        let all = report(&repo, &all_dry_run());
        assert!(all.contains("would remove old (branch)"), "{all}");
        assert!(all.contains("would remove wip (branch)"), "{all}");
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
        assert!(all.contains("would remove done\n"), "{all}");
        assert!(all.contains("would remove lost\n"), "{all}");
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
        assert!(all.contains("would remove shipped\n"), "{all}");
        assert!(all.contains("would remove shipped (branch)"), "{all}");
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
        assert!(err.contains("skipping dirty worktree busy"), "{err}");
        assert!(!err.contains("could not delete"), "{err}");
        assert!(has_branch(&repo, "busy"));
    }

    #[test]
    fn a_failed_recoverability_check_counts_as_unsafe() {
        let repo = TestRepo::init();
        // `rev-list` fails on a branch that does not exist; that must never read
        // as "safe to delete".
        assert!(!super::branch_is_recoverable(
            &RealGit,
            repo.root(),
            "no-such-branch",
            &[]
        ));
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
        let fresh = json_branch(&repo, &all_dry_run(), "feat").expect("selected as gone");
        assert_eq!(fresh["safe"], serde_json::json!(false));
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
        // --json implies dry-run: still present.
        assert!(repo.git(&["branch", "--list", "old"]).contains("old"));
    }
}
