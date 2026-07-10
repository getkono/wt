//! The TUI effect handlers (spec §10): the synchronous command runners that a
//! background job executes (`run_*_command`), the `apply_*` functions that fold a
//! finished [`JobOutcome`](super::JobOutcome) back into the [`App`], and the
//! `do_*` entry points the event loop and tests drive. The async event loop, job
//! scheduling, and the compose loop stay in the parent [`super`] module.

use super::*;

/// Builds the `NewArgs` a TUI create/materialize uses. Submodule init is always
/// suppressed here (`no_init_submodules`) so the worktree-add stays fast and never
/// blocks; the TUI schedules any needed init as its own background job on the new
/// row instead (issue #46 overhaul), driven by the `[submodules] init` policy.
pub(super) fn tui_new_args(branch: &str, base: Option<String>) -> NewArgs {
    NewArgs {
        branch: branch.to_string(),
        from: base,
        track: None,
        no_track: false,
        no_switch: true,
        no_hooks: false,
        // `--start` is CLI-only: the TUI's `CapturingHookRunner` would swallow the
        // command's output, and there is no shell wrapper to `cd` afterwards.
        start: None,
        copy_from: None,
        init_submodules: false,
        no_init_submodules: true,
    }
}

/// Runs a create with staleness handling (issue #56). With no decision yet, it
/// pre-flights the base and, if behind, returns `NeedsStaleConfirm` *without*
/// creating; otherwise (or with a concrete decision) it creates, fast-forwarding
/// the base first when the user chose to update. Hooks are captured so their
/// output never paints the TUI.
pub(super) fn run_create_command(
    cx: &mut Cx,
    branch: &str,
    base: Option<String>,
    decision: Option<CreateDecision>,
) -> CreateOutcome {
    let args = tui_new_args(branch, base);
    match decision {
        None => match commands::new::detect_stale_base(cx, &args) {
            Ok(Some(stale)) => CreateOutcome::NeedsStaleConfirm {
                behind: stale.behind,
                upstream_display: stale.upstream_display,
                can_fast_forward: stale.can_fast_forward,
            },
            // No stale base, or detection failed (offline) — just create.
            _ => create_core(cx, &args),
        },
        Some(CreateDecision::Update) => match commands::new::update_stale_base(cx, &args) {
            Ok(()) => create_core(cx, &args),
            Err(e) => CreateOutcome::Failed(e.to_string()),
        },
        Some(CreateDecision::Proceed) => create_core(cx, &args),
    }
}

/// Creates the worktree (no staleness check) and maps the result to a
/// [`CreateOutcome`].
pub(super) fn create_core(cx: &mut Cx, args: &NewArgs) -> CreateOutcome {
    // `run_core` never inits submodules (see [`tui_new_args`]); the TUI schedules
    // any needed init as its own background job after this returns.
    match commands::new::run_core(cx, &CapturingHookRunner, args, false, false) {
        Ok(_) => match detect_pending_submodules(cx, &args.branch) {
            Some((dir, count, auto)) => CreateOutcome::CreatedNeedsSubmodules { dir, count, auto },
            None => CreateOutcome::Created,
        },
        Err(e) => CreateOutcome::Failed(e.to_string()),
    }
}

/// After a worktree is created/materialized, reports its directory, the number of
/// uninitialized submodules, and whether the `[submodules] init` policy wants them
/// initialized automatically (`always` → `auto = true`) or after a prompt
/// (`prompt` → `auto = false`). `None` when the policy is `never`, the worktree
/// can't be located, or there is nothing to initialize (issue #50).
pub(super) fn detect_pending_submodules(cx: &Cx, branch: &str) -> Option<(PathBuf, usize, bool)> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref()).ok()?;
    let auto = match session.config.submodules_init {
        SubmoduleInit::Never => return None,
        SubmoduleInit::Always => true,
        SubmoduleInit::Prompt => false,
    };
    let worktrees = enumerate_worktrees(&session.repo, git.as_ref()).ok()?;
    let dir = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch))
        .map(|w| w.path.clone())?;
    let pending = crate::git::submodule::uninitialized(git.as_ref(), &dir).ok()?;
    if pending.is_empty() {
        None
    } else {
        Some((dir, pending.len(), auto))
    }
}

/// Materializes a worktree for an existing branch — checks it out as-is via the
/// core create path (no staleness check; the branch already exists).
pub(super) fn run_materialize_command(
    cx: &mut Cx,
    branch: &str,
) -> std::result::Result<(), String> {
    let args = tui_new_args(branch, None);
    commands::new::run_core(cx, &CapturingHookRunner, &args, false, false)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Removes the worktree matched by `query` (force semantics, as the confirm
/// dialog is the guard).
pub(super) fn run_remove_command(cx: &mut Cx, query: &str) -> std::result::Result<(), String> {
    let opts = commands::remove::RemoveOptions {
        force_remove: true,
        force_branch: false,
        keep_branch: false,
        no_hooks: false,
    };
    commands::remove::remove_query(cx, &CapturingHookRunner, query, &opts, false)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Deletes the local branch of a worktree-less branch row (issue #53): a safe
/// `git branch -d` unless `force` (the unmerged re-prompt), which uses `-D`.
pub(super) fn run_delete_branch_command(
    cx: &mut Cx,
    branch: &str,
    force: bool,
) -> std::result::Result<(), String> {
    commands::remove::delete_branch_query(cx, branch, force, false)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Checks out a PR into a worktree, rebuilding the session from the job's `Cx`.
pub(super) fn run_checkout_pr_command(
    cx: &mut Cx,
    number: u64,
) -> std::result::Result<(PathBuf, bool), String> {
    let git = cx.git.clone();
    let gh = cx.gh.clone();
    let session = open_session(cx, git.as_ref()).map_err(|e| e.to_string())?;
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    commands::pr::checkout_pr_worktree(
        cx,
        git.as_ref(),
        gh.as_ref(),
        &CapturingHookRunner,
        &session,
        &dir,
        &number.to_string(),
        false,
        // No inline prompt on a TUI background job; the policy decides.
        false,
    )
    .map(|c| (c.path, c.existed))
    .map_err(|e| e.to_string())
}

/// Checks out `branch` in `worktree_dir` in place, rebuilding the session from
/// the job's `Cx`.
pub(super) fn run_checkout_branch_command(
    cx: &mut Cx,
    worktree_dir: &Path,
    branch: &str,
) -> std::result::Result<commands::checkout::SyncOutcome, String> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref()).map_err(|e| e.to_string())?;
    commands::checkout::checkout_branch_in_worktree(
        cx,
        git.as_ref(),
        &session,
        worktree_dir,
        branch,
        false,
        // No per-invocation override in the TUI; `[submodules] init` decides.
        None,
        // No inline prompt on a TUI background job.
        false,
    )
    .map_err(|e| e.to_string())
}

/// Syncs (pull then push) the branch in `worktree_dir` in place, rebuilding the
/// session from the job's `Cx`. The TUI cannot prompt on a background job, so the
/// submodule step follows the `[submodules] init` policy (no override, no prompt).
pub(super) fn run_sync_command(
    cx: &mut Cx,
    worktree_dir: &Path,
) -> std::result::Result<commands::sync::SyncOutcome, String> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref()).map_err(|e| e.to_string())?;
    commands::sync::sync_worktree(cx, git.as_ref(), &session, worktree_dir, None, false, false)
        .map_err(|e| e.to_string())
}

/// Syncs a worktree-less `branch` by moving its ref from the repo root, rebuilding
/// the session from the job's `Cx` (issue #47/#63). Mirrors [`run_sync_command`]
/// but for a branch with no checkout.
pub(super) fn run_sync_branch_command(
    cx: &mut Cx,
    branch: &str,
) -> std::result::Result<commands::sync::SyncOutcome, String> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref()).map_err(|e| e.to_string())?;
    commands::sync::sync_branch(cx, git.as_ref(), &session, branch, false)
        .map_err(|e| e.to_string())
}

/// Initializes the submodules in `worktree_dir` recursively (issue #50), after
/// the user confirmed the post-create modal. Best-effort: a failure is reported
/// as a status error, not propagated.
pub(super) fn run_init_submodules_command(
    cx: &mut Cx,
    worktree_dir: &Path,
) -> std::result::Result<(), String> {
    let git = cx.git.clone();
    crate::git::submodule::update_init(git.as_ref(), worktree_dir).map_err(|e| e.to_string())
}

/// Spawns a blocking task that builds the fully-enriched worktrees and sends
/// them to the loop.
pub(super) fn spawn_enrichment(
    root: PathBuf,
    git: Arc<dyn GitCli + Send + Sync>,
    tx: mpsc::Sender<Vec<Worktree>>,
) {
    tokio::task::spawn_blocking(move || {
        if let Ok(repo) = Repo::discover(&root)
            && let Ok(worktrees) = build_rows(&repo, git.as_ref())
        {
            let _ = tx.blocking_send(worktrees);
        }
    });
}

/// Replaces the app's worktrees and marks every row loaded.
pub(super) fn mark_all_loaded(app: &mut App, worktrees: Vec<Worktree>) {
    let paths: Vec<PathBuf> = worktrees.iter().map(|w| w.path.clone()).collect();
    app.set_worktrees(worktrees);
    for path in paths {
        app.mark_loaded(path);
    }
}

/// Rebuilds the worktree list (after a mutation), preserving selection.
pub(crate) fn do_refresh(cx: &Cx, app: &mut App, root: &Path) {
    let git = cx.git.clone();
    if let Ok(repo) = Repo::discover(root) {
        // Refresh the branch options/completion candidates so a just-created
        // branch becomes selectable (best-effort; keep the old list on failure).
        if let Ok(branches) = crate::git::all_branches(repo.gix()) {
            app.branches = branches;
        }
        if let Ok(worktrees) = build_rows(&repo, git.as_ref()) {
            mark_all_loaded(app, worktrees);
        }
    }
}

/// Fetches open PRs into the picker (best-effort; errors shown inline).
#[cfg(test)]
pub(crate) fn do_fetch_prs(cx: &Cx, session: &Session, app: &mut App) {
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    apply_prs(app, fetch_prs_result(cx.gh.as_ref(), &dir));
}

/// Lists open PRs and maps them to [`PrItem`]s, stringifying any error to ferry
/// across the async task boundary. Shared by the synchronous test helper and the
/// background fetch (issue #46 overhaul).
pub(super) fn fetch_prs_result(
    gh: &dyn crate::gh::GhClient,
    dir: &Path,
) -> std::result::Result<Vec<PrItem>, String> {
    gh.list_open_prs(dir)
        .map(|prs| {
            prs.into_iter()
                .map(|p| {
                    let pr_state = p.pr_state().as_str().to_string();
                    PrItem {
                        number: p.number,
                        title: p.title,
                        author: p.author.login,
                        state: pr_state,
                        created_at: p.created_at,
                    }
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// Folds a finished PR fetch into the picker, if it is still open (the user may
/// have closed it while the fetch ran).
pub(super) fn apply_prs(app: &mut App, result: std::result::Result<Vec<PrItem>, String>) {
    if let Mode::PrPicker(state) = &mut app.mode {
        state.loading = false;
        match result {
            Ok(prs) => state.prs = prs,
            Err(e) => state.error = Some(e),
        }
    }
}

// The `do_*` helpers below run the shell action synchronously (job then apply,
// no `spawn_blocking`), keeping the original handler surface for unit tests; the
// event loop instead spawns the job and applies the outcome asynchronously so it
// can animate the spinner overlay (issue #46). They are test-only — the loop
// never calls them.

/// Creates a worktree and refreshes; errors show inline in the create modal.
#[cfg(test)]
pub(crate) fn do_create(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    branch: String,
    base: Option<String>,
) {
    let outcome = run_job(
        JobCx::capture(cx),
        Job::Create {
            branch,
            base,
            decision: None,
        },
    );
    apply_outcome(cx, session, app, outcome);
}

/// Removes the worktree at `index` and refreshes. The confirm dialog is itself
/// the guard, so the worktree is removed even if dirty/unpushed; but an unmerged
/// branch is never force-deleted here (spec §10/§12) — only a fully-merged
/// wt-created branch is cleaned up.
#[cfg(test)]
pub(crate) fn do_remove(cx: &mut Cx, session: &Session, app: &mut App, index: usize) {
    let Some(query) = remove_query_of(app, index) else {
        return;
    };
    let outcome = run_job(JobCx::capture(cx), Job::Remove { query });
    apply_outcome(cx, session, app, outcome);
}

/// Deletes a worktree-less branch row's local branch and refreshes (issue #53).
/// The confirm dialog is the guard: a safe `git branch -d` unless `force`, which
/// `git branch -D`s an unmerged branch. A safe refusal leaves the app in the
/// force-delete re-prompt.
#[cfg(test)]
pub(crate) fn do_delete_branch(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    branch: String,
    force: bool,
) {
    let outcome = run_job(JobCx::capture(cx), Job::DeleteBranch { branch, force });
    apply_outcome(cx, session, app, outcome);
}

/// Materializes a worktree for an existing worktree-less branch and switches into
/// it (issue #47): creates the worktree via the `new` path (which checks out an
/// existing branch as-is), refreshes, then records the new worktree as `chosen`
/// so the loop exits and the wrapper `cd`s into it. Errors show in the status bar.
#[cfg(test)]
pub(crate) fn do_materialize_branch(cx: &mut Cx, session: &Session, app: &mut App, branch: String) {
    let outcome = run_job(JobCx::capture(cx), Job::Materialize { branch });
    apply_outcome(cx, session, app, outcome);
}

/// Checks out a PR into a worktree, then returns to the list and refreshes,
/// focusing the new worktree row (issue #46 overhaul — the checkout stays in the
/// TUI rather than switching-and-exiting).
#[cfg(test)]
pub(crate) fn do_checkout_pr(cx: &mut Cx, session: &Session, app: &mut App, number: u64) {
    let outcome = run_job(JobCx::capture(cx), Job::CheckoutPr { number });
    apply_outcome(cx, session, app, outcome);
}

/// Checks out `branch` in the worktree at `index` in place, syncing with origin,
/// then refreshes and stays in the list (the row updates in place). Errors show
/// inline in the checkout picker; a successful sync's note is in the status bar.
#[cfg(test)]
pub(crate) fn do_checkout_branch(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    index: usize,
    branch: String,
) {
    let Some(worktree_dir) = app.worktrees.get(index).map(|w| w.path.clone()) else {
        return;
    };
    let outcome = run_job(
        JobCx::capture(cx),
        Job::CheckoutBranch {
            worktree_dir,
            branch,
        },
    );
    apply_outcome(cx, session, app, outcome);
}

/// Syncs the row at `index` (a worktree in place, or a worktree-less branch by
/// ref), then refreshes and stays in the list. Outcomes (and errors) surface in
/// the status bar. Dispatches on `has_worktree` exactly as [`begin_job`] does.
#[cfg(test)]
pub(crate) fn do_sync(cx: &mut Cx, session: &Session, app: &mut App, index: usize) {
    let Some(worktree) = app.worktrees.get(index) else {
        return;
    };
    let label = worktree
        .branch
        .clone()
        .unwrap_or_else(|| "worktree".to_string());
    let job = if worktree.has_worktree {
        Job::Sync {
            worktree_dir: worktree.path.clone(),
            label,
        }
    } else {
        let Some(branch) = worktree.branch.clone() else {
            return;
        };
        Job::SyncBranch { branch, label }
    };
    let outcome = run_job(JobCx::capture(cx), job);
    apply_outcome(cx, session, app, outcome);
}

/// Applies a finished create: on success switch to the list with a status and
/// refresh; on failure surface the error inline in the create modal.
pub(super) fn apply_create(
    cx: &Cx,
    app: &mut App,
    branch: &str,
    base: Option<String>,
    outcome: CreateOutcome,
    root: &Path,
) {
    match outcome {
        CreateOutcome::Created => {
            app.set_status(format!("created {branch}"), StatusKind::Success);
            do_refresh(cx, app, root);
            // Focus the newly created worktree (issue #52). `do_refresh` restores
            // the prior selection by path, so this must run after it. If a filter
            // hides the new row, the selection is left as-is (the filter is not
            // cleared — that would be surprising).
            let _ = app.select_branch(branch);
            close_create_modal(app);
        }
        // Created, but the new worktree has uninitialized submodules (issue #50).
        // Under `always` (or when a modal can't be shown) start a background init
        // job on the new row and return to the list; under `prompt` open the
        // confirm modal over the row (issue #46 overhaul).
        CreateOutcome::CreatedNeedsSubmodules { dir, count, auto } => {
            app.set_status(format!("created {branch}"), StatusKind::Success);
            do_refresh(cx, app, root);
            let _ = app.select_branch(branch);
            if !auto && app.may_apply_mode(JobHome::Create) {
                app.mode = Mode::ConfirmInitSubmodules(InitSubmodulesState {
                    dir,
                    branch: branch.to_string(),
                    count,
                });
            } else {
                app.queue_job(Effect::InitSubmodules { dir, count });
                close_create_modal(app);
            }
        }
        // The base is behind origin: open the confirm modal carrying the pending
        // create's inputs so the user's choice can re-issue it (issue #56). If the
        // user navigated into another modal meanwhile, report it instead of
        // clobbering their screen (rare — the create modal stays open otherwise).
        CreateOutcome::NeedsStaleConfirm {
            behind,
            upstream_display,
            can_fast_forward,
        } => {
            if app.may_apply_mode(JobHome::Create) {
                app.mode = Mode::ConfirmStaleBase(StaleBaseState {
                    branch: branch.to_string(),
                    base,
                    behind,
                    upstream_display,
                    can_fast_forward,
                });
            } else {
                app.set_status(
                    format!("{branch}: base is behind {upstream_display}; not created"),
                    StatusKind::Error,
                );
            }
        }
        CreateOutcome::Failed(e) => match &mut app.mode {
            Mode::Create(state) => state.error = Some(e),
            _ => app.set_status(e, StatusKind::Error),
        },
    }
}

/// Returns to the list after a create-family job, unless the user has moved on to
/// an unrelated modal meanwhile (issue #46 overhaul): a background job must never
/// close a modal it does not own.
fn close_create_modal(app: &mut App) {
    if app.may_apply_mode(JobHome::Create) {
        app.mode = Mode::List;
    }
}

/// Applies a finished remove: report success/error in the status bar, return to
/// the list, and refresh.
pub(super) fn apply_remove(
    cx: &Cx,
    app: &mut App,
    query: &str,
    result: std::result::Result<(), String>,
    root: &Path,
) {
    match result {
        Ok(()) => app.set_status(format!("removed {query}"), StatusKind::Success),
        Err(e) => app.set_status(e, StatusKind::Error),
    }
    if app.may_apply_mode(JobHome::List) {
        app.mode = Mode::List;
    }
    do_refresh(cx, app, root);
}

/// Applies a finished branch deletion (issue #53): on success report it and
/// refresh; on a safe-delete refusal because the branch is unmerged, re-open the
/// confirm to offer a force-delete; any other failure shows in the status bar.
pub(super) fn apply_delete_branch(
    cx: &Cx,
    app: &mut App,
    branch: &str,
    force: bool,
    result: std::result::Result<(), String>,
    root: &Path,
) {
    match result {
        Ok(()) => {
            app.set_status(format!("deleted branch {branch}"), StatusKind::Success);
            do_refresh(cx, app, root);
            if app.may_apply_mode(JobHome::List) {
                app.mode = Mode::List;
            }
        }
        // A safe `git branch -d` refused an unmerged branch: re-prompt to force
        // (only when the user is idle — never clobbering an unrelated modal). The
        // delete failed, so the branch row still exists — re-find it by name.
        Err(e) if !force && e.contains("not fully merged") => {
            let index = app
                .worktrees
                .iter()
                .position(|w| !w.has_worktree && w.branch.as_deref() == Some(branch));
            match index {
                Some(index) if app.may_apply_mode(JobHome::List) => {
                    app.mode = Mode::ConfirmDeleteBranch { index, force: true };
                }
                _ => app.set_status(e, StatusKind::Error),
            }
        }
        Err(e) => {
            app.set_status(e, StatusKind::Error);
            if app.may_apply_mode(JobHome::List) {
                app.mode = Mode::List;
            }
        }
    }
}

/// Applies a finished materialize: on success refresh, focus the new worktree
/// row, and background any submodule init on it, staying in the TUI so the user
/// can keep working and switch into it with Enter when ready (issue #46
/// overhaul — no longer auto-switches-and-exits). Errors surface in the status
/// bar.
pub(super) fn apply_materialize(
    cx: &Cx,
    app: &mut App,
    branch: &str,
    result: std::result::Result<(), String>,
    root: &Path,
) {
    match result {
        Ok(()) => {
            app.set_status(format!("created {branch}"), StatusKind::Success);
            do_refresh(cx, app, root);
            let _ = app.select_branch(branch);
            // The worktree exists; initialize its submodules (if any) as a
            // background job on the new row rather than blocking the switch.
            if let Some((dir, count, _auto)) = detect_pending_submodules(cx, branch) {
                app.queue_job(Effect::InitSubmodules { dir, count });
            }
        }
        Err(e) => app.set_status(e, StatusKind::Error),
    }
    if app.may_apply_mode(JobHome::List) {
        app.mode = Mode::List;
    }
}

/// Applies a finished PR checkout: return to the list, refresh, and focus the new
/// worktree row, staying in the TUI so the user switches into it with Enter when
/// ready (issue #46 overhaul — no longer auto-switches-and-exits). Errors show
/// inline in the picker.
pub(super) fn apply_checkout_pr(
    cx: &Cx,
    app: &mut App,
    number: u64,
    result: std::result::Result<(PathBuf, bool), String>,
    root: &Path,
) {
    match result {
        Ok((path, _existed)) => {
            app.set_status(format!("checked out PR #{number}"), StatusKind::Success);
            do_refresh(cx, app, root);
            app.select_path(&path);
            if app.may_apply_mode(JobHome::PrPicker) {
                app.mode = Mode::List;
            }
        }
        Err(e) => match &mut app.mode {
            Mode::PrPicker(state) => state.error = Some(e),
            _ => app.set_status(e, StatusKind::Error),
        },
    }
}

/// Applies a finished in-place branch checkout: stay in the (refreshed) list
/// with a sync-annotated status; errors show inline in the checkout picker.
pub(super) fn apply_checkout_branch(
    cx: &Cx,
    app: &mut App,
    branch: &str,
    result: std::result::Result<commands::checkout::SyncOutcome, String>,
    root: &Path,
) {
    match result {
        Ok(outcome) => {
            app.set_status(
                format!(
                    "checked out {branch}{}",
                    commands::checkout::sync_suffix(outcome)
                ),
                StatusKind::Success,
            );
            do_refresh(cx, app, root);
            if app.may_apply_mode(JobHome::Checkout) {
                app.mode = Mode::List;
            }
        }
        Err(e) => match &mut app.mode {
            Mode::Checkout(state) => {
                state.error = Some(e);
                state.submitting = false;
            }
            _ => app.set_status(e, StatusKind::Error),
        },
    }
}

/// Applies a finished sync (issue #63): stay in the (refreshed) list with a
/// sync-annotated status. Refused/failed outcomes (diverged, dirty, push
/// rejected) read as errors; everything else as a success. Sync acts directly
/// from the list (no picker modal), so errors land in the status bar.
pub(super) fn apply_sync(
    cx: &Cx,
    app: &mut App,
    label: &str,
    result: std::result::Result<commands::sync::SyncOutcome, String>,
    root: &Path,
) {
    use commands::sync::SyncOutcome;
    match result {
        Ok(outcome) => {
            let kind = match outcome {
                SyncOutcome::Diverged
                | SyncOutcome::DivergedNoWorktree
                | SyncOutcome::Dirty
                | SyncOutcome::PushRejected => StatusKind::Error,
                _ => StatusKind::Success,
            };
            app.set_status(
                format!("synced {label}{}", commands::sync::sync_suffix(outcome)),
                kind,
            );
            do_refresh(cx, app, root);
        }
        Err(e) => app.set_status(e, StatusKind::Error),
    }
    if app.may_apply_mode(JobHome::List) {
        app.mode = Mode::List;
    }
}

/// Applies a finished submodule init (issue #50): report success/error in the
/// status bar, return to the list, and refresh (the submodule files now exist).
pub(super) fn apply_init_submodules(
    cx: &Cx,
    app: &mut App,
    count: usize,
    result: std::result::Result<(), String>,
    root: &Path,
) {
    match result {
        Ok(()) => {
            app.set_status(
                format!("initialized {count} submodule(s)"),
                StatusKind::Success,
            );
            do_refresh(cx, app, root);
        }
        Err(e) => app.set_status(
            format!("failed to initialize submodules: {e}"),
            StatusKind::Error,
        ),
    }
    if app.may_apply_mode(JobHome::List) {
        app.mode = Mode::List;
    }
}

/// Drafts the PR title/body with the code agent and seeds the compose form,
/// using the model/effort currently selected in the form (`Ctrl-M`/`Ctrl-E`).
/// Errors (including a missing agent) show inline in the form, which stays open.
pub(crate) fn do_draft_pr_ai(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    ctx: &sendit::PrContext,
) {
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    // Read the live model/effort from the form before borrowing it mutably.
    let opts = match &app.mode {
        Mode::PrCompose(state) => crate::agent::AgentOptions {
            model: state.model,
            effort: state.effort,
        },
        _ => crate::agent::AgentOptions::default(),
    };
    // The TUI is suspended during the (blocking) agent call, so a progress line
    // on stderr is visible while the user waits.
    let _ = cx.err.line(&format!(
        "Drafting PR with {} (effort {})…",
        opts.model.label(),
        opts.effort.id()
    ));
    let result = crate::commands::pr_open::draft_with_ai(cx.agent.as_ref(), ctx, &dir, &opts);
    if let Mode::PrCompose(state) = &mut app.mode {
        match result {
            Ok((title, body)) => {
                state.title = title;
                state.body = body;
                state.error = None;
            }
            Err(e) => state.error = Some(e.to_string()),
        }
        state.submitting = false;
    }
}

/// Submits the composed PR (push + create/update + metadata). On success stores
/// the outcome and returns `true` to exit the loop; on failure shows the error
/// inline and stays so the user can edit and retry.
#[allow(clippy::too_many_arguments)]
pub(crate) fn do_submit_pr(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    ctx: &sendit::PrContext,
    action: sendit::PrAction,
    title: String,
    body: String,
    draft: bool,
    outcome: &mut Option<(sendit::PrOutcome, sendit::PrSpec)>,
) -> bool {
    let git = cx.git.clone();
    let gh = cx.gh.clone();
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    let spec = sendit::PrSpec { title, body, draft };
    let result = crate::commands::pr_open::submit_pr(
        git.as_ref(),
        gh.as_ref(),
        &session.primary_root,
        &dir,
        &session.config.pr_default_remote,
        ctx,
        &spec,
        action,
    );
    match result {
        Ok(out) => {
            // Best-effort metadata so `wt list`/TUI show the new PR offline.
            let _ = crate::commands::pr_open::record_pr_metadata(
                git.as_ref(),
                &session.primary_root,
                &ctx.branch,
                &ctx.trunk,
                &out,
                &spec.title,
            );
            *outcome = Some((out, spec));
            true
        }
        Err(e) => {
            if let Mode::PrCompose(state) = &mut app.mode {
                state.error = Some(e.to_string());
                state.submitting = false;
            }
            false
        }
    }
}
