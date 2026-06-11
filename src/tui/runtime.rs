//! TUI runtime (spec §10): the async event loop that drives [`App`], executes
//! [`Effect`]s, and loads async data. The loop and terminal handling are the
//! thin, untestable shell; the effect-executing helpers are pure of the terminal
//! and are unit-tested. Shell-based mutating actions run on a background task as
//! a `Job` and apply their `JobOutcome` to the app, so the loop can animate a
//! spinner overlay instead of freezing (issue #46).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::EventStream;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::cli::NewArgs;
use crate::commands::{self, Session, open_session};
use crate::config::Config;
use crate::cx::{Cx, SilentInput, Stream};
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::hooks::CapturingHookRunner;
use crate::model::{SortSpec, Worktree};
use crate::tui::app::{App, AppConfig, Mode, PrComposeState, PrItem, StatusKind};
use crate::tui::event::Effect;
use crate::tui::terminal::{Tui, install_panic_hook};
use crate::util::editor::{editor_argv, resolve_editor};
use crate::worktree_service::{build_rows, enumerate_rows};

/// Builds the [`AppConfig`] from the resolved configuration and the resolved
/// color decision (spec §11 precedence).
pub(crate) fn app_config(config: &Config, color: bool) -> AppConfig {
    AppConfig {
        keymap: config.keymap(),
        sort: SortSpec::default(),
        columns: config.list_columns.clone(),
        show_untracked: config.list_show_untracked,
        remove_untracked_blocks: config.remove_untracked_blocks,
        nerd_fonts: config.ui_nerd_fonts,
        mouse: config.ui_mouse,
        color,
        palette: config.palette(),
    }
}

/// Runs the TUI, returning the chosen worktree path (if the user switched).
/// When `initial_filter` is set, the picker opens pre-filtered to that query
/// (the ambiguous-query fallback uses this to surface the candidates).
pub fn run_tui(cx: &mut Cx, initial_filter: Option<&str>) -> Result<Option<PathBuf>> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let mut app = build_app(cx, &session, git.as_ref())?;
    if let Some(filter) = initial_filter.filter(|f| !f.is_empty()) {
        app.apply_filter(filter.to_string());
    }
    drive_tui(cx, &session, app, Effect::None)
}

/// Runs the TUI directly in PR-picker mode (the `wt pr` no-argument entry).
/// Returns the chosen worktree path once a PR is checked out, or `None` if the
/// user cancels. The picker loads its PRs on open (via an initial `FetchPrs`),
/// and selecting a PR switches into the new worktree (spec §7).
pub fn run_pr_picker(cx: &mut Cx) -> Result<Option<PathBuf>> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let mut app = build_app(cx, &session, git.as_ref())?;
    app.exit_on_pr_checkout = true;
    app.mode = Mode::PrPicker(crate::tui::app::PrPickerState {
        loading: true,
        ..Default::default()
    });
    drive_tui(cx, &session, app, Effect::FetchPrs)
}

/// Builds the [`App`] over the session's worktrees plus worktree-less branch
/// rows (issue #47), seeding the branch list.
fn build_app(cx: &Cx, session: &Session, git: &dyn GitCli) -> Result<App> {
    let sync_worktrees = enumerate_rows(&session.repo, git)?;
    let size = crossterm::terminal::size().unwrap_or((100, 30));
    // The TUI draws to the alternate screen on stderr, so resolve color against
    // stderr (stdout is reserved for the chosen path and is usually piped).
    let color = cx.color_enabled_err(session.config.ui_color);
    let mut app = App::new(sync_worktrees, app_config(&session.config, color), size);
    app.branches = crate::git::all_branches(session.repo.gix()).unwrap_or_default();
    app.mark_loading();
    Ok(app)
}

/// Drives the prepared app through the event loop and returns the chosen path
/// (terminal shell; not unit-tested).
fn drive_tui(
    cx: &mut Cx,
    session: &Session,
    mut app: App,
    initial: Effect,
) -> Result<Option<PathBuf>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_loop(cx, session, &mut app, initial))?;

    if app.too_small {
        cx.err.line("terminal too small (need ≥5 rows)")?;
        return Err(Error::operation("terminal too small"));
    }
    Ok(app.chosen.clone())
}

/// The async event loop (terminal shell; not unit-tested). `initial` is an
/// effect dispatched once after the first paint (e.g. `FetchPrs` to populate the
/// PR picker on open); pass `Effect::None` for no initial action.
async fn run_loop(cx: &mut Cx, session: &Session, app: &mut App, initial: Effect) -> Result<()> {
    install_panic_hook();
    let mut tui = Tui::enter(app.mouse)?;
    app.size = tui.size();
    // Refuse to drive a terminal that is already too short, before the first
    // paint (spec §10); the `Tui` guard restores the terminal on drop.
    if app.size.1 < crate::tui::app::MIN_HEIGHT {
        app.too_small = true;
        return Ok(());
    }
    tui.draw(app)?;

    if initial != Effect::None {
        if dispatch_effect(cx, session, app, &mut tui, initial)? {
            return Ok(());
        }
        tui.draw(app)?;
    }

    // Load async data in the background and stream the result in.
    let (tx, mut rx) = mpsc::channel::<Vec<Worktree>>(1);
    spawn_enrichment(session.primary_root.clone(), cx.git.clone(), tx);

    // Background shell-based actions (issue #46) run on a blocking task and send
    // their outcome here; a ticker animates the spinner overlay while one is in
    // flight.
    let (job_tx, mut job_rx) = mpsc::channel::<JobOutcome>(1);
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));

    let mut events = EventStream::new();
    loop {
        tokio::select! {
            // Animate the spinner while a background action runs (the guard
            // disables this branch — and the timer wakeups — when idle).
            _ = ticker.tick(), if app.is_busy() => {
                app.tick_busy();
                tui.draw(app)?;
            }
            // A background action finished: clear the overlay, apply its result,
            // and exit the loop if it set a worktree to switch into.
            Some(outcome) = job_rx.recv() => {
                app.end_busy();
                apply_outcome(cx, session, app, outcome);
                tui.draw(app)?;
                if app.chosen.is_some() {
                    break;
                }
            }
            maybe = events.next() => {
                let Some(Ok(event)) = maybe else { continue };
                // Ignore input while a background action is in flight.
                if app.is_busy() {
                    continue;
                }
                let effect = app.handle_event(event);
                if is_background_action(&effect) {
                    spawn_job(cx, app, effect, &job_tx);
                    tui.draw(app)?;
                } else if dispatch_effect(cx, session, app, &mut tui, effect)? {
                    break;
                } else {
                    tui.draw(app)?;
                }
            }
            Some(worktrees) = rx.recv() => {
                mark_all_loaded(app, worktrees);
                // Don't redraw mid-action so the spinner cadence stays smooth; the
                // post-completion draw shows the enriched rows.
                if !app.is_busy() {
                    tui.draw(app)?;
                }
            }
        }
    }
    Ok(())
}

/// Executes an effect, returning `true` when the loop should exit (terminal
/// shell; the operations it delegates to are tested).
fn dispatch_effect(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    tui: &mut Tui,
    effect: Effect,
) -> Result<bool> {
    match effect {
        Effect::None => Ok(false),
        Effect::Switch(_) | Effect::Quit => Ok(true),
        Effect::TooSmall => {
            app.too_small = true;
            Ok(true)
        }
        Effect::Refresh => {
            do_refresh(cx, app, &session.primary_root);
            Ok(false)
        }
        Effect::FetchPrs => {
            do_fetch_prs(cx, session, app);
            Ok(false)
        }
        Effect::OpenEditor(path) => {
            tui.suspend()?;
            run_editor(cx, session, &path);
            tui.resume()?;
            Ok(false)
        }
        // Shell-based mutating actions run on a background task with a spinner
        // overlay (issue #46), dispatched by [`spawn_job`] from the event loop;
        // they never reach `dispatch_effect`.
        Effect::Create { .. }
        | Effect::Remove(_)
        | Effect::MaterializeBranch { .. }
        | Effect::CheckoutPr(_)
        | Effect::CheckoutBranch { .. } => Ok(false),
        // Compose-only effects are driven by the dedicated compose loop
        // ([`run_pr_compose`]) and never reach the main loop.
        Effect::DraftPrAi | Effect::SubmitPr { .. } => Ok(false),
    }
}

/// Whether an effect is a shell-based mutating action that the event loop runs
/// on a background task (with a spinner overlay) rather than inline (issue #46).
fn is_background_action(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::Create { .. }
            | Effect::Remove(_)
            | Effect::MaterializeBranch { .. }
            | Effect::CheckoutPr(_)
            | Effect::CheckoutBranch { .. }
    )
}

/// The owned, `Send + 'static` pieces a background job needs to build its own
/// [`Cx`] (issue #46): discarded output buffers, a silent input, cloned handles,
/// the captured environment, and the working directory. Rebuilding the `Cx`
/// inside the job avoids moving the loop's borrowed `Cx`/`Session` into a
/// `'static` task (mirrors [`spawn_enrichment`]).
struct JobCx {
    env: crate::cx::Env,
    cwd: PathBuf,
    git: Arc<dyn GitCli + Send + Sync>,
    gh: Arc<dyn crate::gh::GhClient + Send + Sync>,
    agent: Arc<dyn crate::agent::AgentClient + Send + Sync>,
}

impl JobCx {
    /// Captures the handles needed to rebuild a `Cx` on a background thread. The
    /// working directory is the loop's `cx.cwd`, so the rebuilt session matches
    /// the one the foreground built at startup.
    fn capture(cx: &Cx) -> Self {
        JobCx {
            env: cx.env.clone(),
            cwd: cx.cwd.clone(),
            git: cx.git.clone(),
            gh: cx.gh.clone(),
            agent: cx.agent.clone(),
        }
    }

    /// Builds a fresh owned `Cx` inside the job: stdout/stderr are discarded
    /// buffers (subprocess/hook output is captured, not shown — the TUI stays on
    /// the alternate screen), and prompts are auto-declined.
    fn into_cx(self) -> Cx {
        let mut cx = Cx::new(
            Stream::new(Box::new(Vec::<u8>::new()), false),
            Stream::new(Box::new(Vec::<u8>::new()), false),
            self.env,
            self.cwd,
            self.git,
            self.gh,
            self.agent,
            Box::new(SilentInput),
        );
        cx.no_pager = true;
        cx
    }
}

/// A shell-based action to run on a background task, with its arguments already
/// resolved to owned values on the foreground thread (issue #46).
enum Job {
    /// Create a worktree for a new `branch` based on `base`.
    Create {
        /// The new branch name.
        branch: String,
        /// The base ref (or `None` for the default).
        base: Option<String>,
    },
    /// Remove the worktree matched by `query` (force semantics).
    Remove {
        /// The branch (or directory name) identifying the worktree to remove.
        query: String,
    },
    /// Materialize a worktree for an existing worktree-less `branch`.
    Materialize {
        /// The branch to create a worktree for.
        branch: String,
    },
    /// Check out the PR with the given number.
    CheckoutPr {
        /// The PR number.
        number: u64,
    },
    /// Check out `branch` in the worktree at `worktree_dir` (in place).
    CheckoutBranch {
        /// The target worktree directory.
        worktree_dir: PathBuf,
        /// The branch to check out.
        branch: String,
    },
}

/// The result of a background [`Job`], carrying the minimum its `apply_*` needs.
/// Errors are stringified inside the job (the typed `Error` is not `'static`-
/// friendly to ferry across the task boundary, and the UI only needs the text).
enum JobOutcome {
    /// A finished create; the branch echoes back for the status text.
    Create {
        /// The branch that was created.
        branch: String,
        /// Success, or the error message to surface.
        result: std::result::Result<(), String>,
    },
    /// A finished remove.
    Remove {
        /// The query that was removed (for the status text).
        query: String,
        /// Success, or the error message to surface.
        result: std::result::Result<(), String>,
    },
    /// A finished materialize; the new worktree is located on apply.
    Materialize {
        /// The branch that was materialized.
        branch: String,
        /// Success, or the error message to surface.
        result: std::result::Result<(), String>,
    },
    /// A finished PR checkout; the path is the new worktree to switch into.
    CheckoutPr {
        /// The PR number (for the status text).
        number: u64,
        /// The `(worktree path, already existed)` pair, or the error message.
        result: std::result::Result<(PathBuf, bool), String>,
    },
    /// A finished in-place branch checkout.
    CheckoutBranch {
        /// The branch that was checked out.
        branch: String,
        /// The sync outcome, or the error message.
        result: std::result::Result<commands::checkout::SyncOutcome, String>,
    },
}

/// Resolves the worktree-identifying query for the row at `index` (its branch,
/// or the directory name), mirroring the CLI's `remove` query.
fn remove_query_of(app: &App, index: usize) -> Option<String> {
    let worktree = app.worktrees.get(index)?;
    Some(worktree.branch.clone().unwrap_or_else(|| {
        worktree
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }))
}

/// Resolves a background effect into a [`Job`] (owning its arguments) and marks
/// the app busy with a display label. Returns `None` (and leaves the app idle)
/// when the effect's target row is gone.
fn begin_job(app: &mut App, effect: Effect) -> Option<Job> {
    let (job, label) = match effect {
        Effect::Create { branch, base } => {
            let label = format!("Creating {branch}");
            (Job::Create { branch, base }, label)
        }
        Effect::Remove(index) => {
            let query = remove_query_of(app, index)?;
            let label = format!("Removing {query}");
            (Job::Remove { query }, label)
        }
        Effect::MaterializeBranch { branch } => {
            let label = format!("Creating worktree for {branch}");
            (Job::Materialize { branch }, label)
        }
        Effect::CheckoutPr(number) => {
            let label = format!("Checking out PR #{number}");
            (Job::CheckoutPr { number }, label)
        }
        Effect::CheckoutBranch {
            worktree_index,
            branch,
        } => {
            let worktree_dir = app.worktrees.get(worktree_index)?.path.clone();
            let label = format!("Checking out {branch}");
            (
                Job::CheckoutBranch {
                    worktree_dir,
                    branch,
                },
                label,
            )
        }
        _ => return None,
    };
    app.begin_busy(label);
    Some(job)
}

/// Spawns a background task to run `effect`'s shell action, marking the app busy
/// so the spinner overlay shows; the outcome is sent to `tx` for the loop to
/// apply (issue #46).
fn spawn_job(cx: &Cx, app: &mut App, effect: Effect, tx: &mpsc::Sender<JobOutcome>) {
    let Some(job) = begin_job(app, effect) else {
        return;
    };
    let jobcx = JobCx::capture(cx);
    let tx = tx.clone();
    tokio::task::spawn_blocking(move || {
        let _ = tx.blocking_send(run_job(jobcx, job));
    });
}

/// Runs a [`Job`] on the blocking thread, building its own `Cx` (and `Session`
/// where needed) and capturing hook output. Returns the typed outcome.
fn run_job(jobcx: JobCx, job: Job) -> JobOutcome {
    let mut cx = jobcx.into_cx();
    match job {
        Job::Create { branch, base } => {
            let result = run_create_command(&mut cx, &branch, base);
            JobOutcome::Create { branch, result }
        }
        Job::Remove { query } => {
            let result = run_remove_command(&mut cx, &query);
            JobOutcome::Remove { query, result }
        }
        Job::Materialize { branch } => {
            let result = run_create_command(&mut cx, &branch, None);
            JobOutcome::Materialize { branch, result }
        }
        Job::CheckoutPr { number } => {
            let result = run_checkout_pr_command(&mut cx, number);
            JobOutcome::CheckoutPr { number, result }
        }
        Job::CheckoutBranch {
            worktree_dir,
            branch,
        } => {
            let result = run_checkout_branch_command(&mut cx, &worktree_dir, &branch);
            JobOutcome::CheckoutBranch { branch, result }
        }
    }
}

/// Creates a worktree (also the materialize path, which checks out an existing
/// branch as-is). Hooks are captured so their output never paints the TUI.
fn run_create_command(
    cx: &mut Cx,
    branch: &str,
    base: Option<String>,
) -> std::result::Result<(), String> {
    let args = NewArgs {
        branch: branch.to_string(),
        from: base,
        track: None,
        no_track: false,
        no_switch: true,
        no_hooks: false,
        copy_from: None,
    };
    commands::new::run(cx, &CapturingHookRunner, &args, false)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Removes the worktree matched by `query` (force semantics, as the confirm
/// dialog is the guard).
fn run_remove_command(cx: &mut Cx, query: &str) -> std::result::Result<(), String> {
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

/// Checks out a PR into a worktree, rebuilding the session from the job's `Cx`.
fn run_checkout_pr_command(
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
    )
    .map_err(|e| e.to_string())
}

/// Checks out `branch` in `worktree_dir` in place, rebuilding the session from
/// the job's `Cx`.
fn run_checkout_branch_command(
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
    )
    .map_err(|e| e.to_string())
}

/// Applies a finished [`JobOutcome`] to the app exactly as the inline handlers
/// did before (issue #46): status text, mode, refresh, and `chosen`.
fn apply_outcome(cx: &Cx, session: &Session, app: &mut App, outcome: JobOutcome) {
    let root = &session.primary_root;
    match outcome {
        JobOutcome::Create { branch, result } => apply_create(cx, app, &branch, result, root),
        JobOutcome::Remove { query, result } => apply_remove(cx, app, &query, result, root),
        JobOutcome::Materialize { branch, result } => {
            apply_materialize(cx, app, &branch, result, root)
        }
        JobOutcome::CheckoutPr { number, result } => {
            apply_checkout_pr(cx, app, number, result, root)
        }
        JobOutcome::CheckoutBranch { branch, result } => {
            apply_checkout_branch(cx, app, &branch, result, root)
        }
    }
}

/// Spawns a blocking task that builds the fully-enriched worktrees and sends
/// them to the loop.
fn spawn_enrichment(
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
fn mark_all_loaded(app: &mut App, worktrees: Vec<Worktree>) {
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
pub(crate) fn do_fetch_prs(cx: &Cx, session: &Session, app: &mut App) {
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    let result = cx.gh.list_open_prs(&dir);
    if let Mode::PrPicker(state) = &mut app.mode {
        state.loading = false;
        match result {
            Ok(prs) => {
                state.prs = prs
                    .into_iter()
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
                    .collect();
            }
            Err(e) => state.error = Some(e.to_string()),
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
    let outcome = run_job(JobCx::capture(cx), Job::Create { branch, base });
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

/// Materializes a worktree for an existing worktree-less branch and switches into
/// it (issue #47): creates the worktree via the `new` path (which checks out an
/// existing branch as-is), refreshes, then records the new worktree as `chosen`
/// so the loop exits and the wrapper `cd`s into it. Errors show in the status bar.
#[cfg(test)]
pub(crate) fn do_materialize_branch(cx: &mut Cx, session: &Session, app: &mut App, branch: String) {
    let outcome = run_job(JobCx::capture(cx), Job::Materialize { branch });
    apply_outcome(cx, session, app, outcome);
}

/// Checks out a PR into a worktree. When `app.exit_on_pr_checkout` is set (the
/// `wt pr` picker entry), records the new worktree as `chosen` so the loop exits
/// and the wrapper `cd`s into it; otherwise returns to the list and refreshes
/// (the in-TUI `p`-key flow).
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

/// Applies a finished create: on success switch to the list with a status and
/// refresh; on failure surface the error inline in the create modal.
fn apply_create(
    cx: &Cx,
    app: &mut App,
    branch: &str,
    result: std::result::Result<(), String>,
    root: &Path,
) {
    match result {
        Ok(()) => {
            app.mode = Mode::List;
            app.set_status(format!("created {branch}"), StatusKind::Success);
            do_refresh(cx, app, root);
        }
        Err(e) => match &mut app.mode {
            Mode::Create(state) => state.error = Some(e),
            _ => app.set_status(e, StatusKind::Error),
        },
    }
}

/// Applies a finished remove: report success/error in the status bar, return to
/// the list, and refresh.
fn apply_remove(
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
    app.mode = Mode::List;
    do_refresh(cx, app, root);
}

/// Applies a finished materialize: on success refresh and record the new
/// worktree as `chosen` (so the loop exits and the wrapper `cd`s into it); on
/// failure surface the error in the status bar.
fn apply_materialize(
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
            // Switch into the freshly created worktree for this branch.
            if let Some(path) = app
                .worktrees
                .iter()
                .find(|w| w.has_worktree && w.branch.as_deref() == Some(branch))
                .map(|w| w.path.clone())
            {
                app.chosen = Some(path);
            }
        }
        Err(e) => app.set_status(e, StatusKind::Error),
    }
    app.mode = Mode::List;
}

/// Applies a finished PR checkout: switch into the new worktree when
/// `exit_on_pr_checkout` is set, else return to the list and refresh; errors
/// show inline in the picker.
fn apply_checkout_pr(
    cx: &Cx,
    app: &mut App,
    number: u64,
    result: std::result::Result<(PathBuf, bool), String>,
    root: &Path,
) {
    match result {
        Ok((path, _existed)) => {
            if app.exit_on_pr_checkout {
                app.chosen = Some(path);
            } else {
                app.mode = Mode::List;
                app.set_status(format!("checked out PR #{number}"), StatusKind::Success);
                do_refresh(cx, app, root);
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
fn apply_checkout_branch(
    cx: &Cx,
    app: &mut App,
    branch: &str,
    result: std::result::Result<commands::checkout::SyncOutcome, String>,
    root: &Path,
) {
    match result {
        Ok(outcome) => {
            app.mode = Mode::List;
            app.set_status(
                format!(
                    "checked out {branch}{}",
                    commands::checkout::sync_suffix(outcome)
                ),
                StatusKind::Success,
            );
            do_refresh(cx, app, root);
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

/// The initial title/body/draft seed for the compose form (`wt pr open`).
#[derive(Debug, Clone, Default)]
pub struct ComposeSeed {
    /// Seed title (empty when not provided).
    pub title: String,
    /// Seed body (empty when not provided).
    pub body: String,
    /// Whether the draft toggle starts on.
    pub draft: bool,
    /// The model used for AI auto-fill (resolved from `--model`/config).
    pub model: crate::agent::AgentModel,
    /// The effort used for AI auto-fill (resolved from `--effort`/config).
    pub effort: crate::agent::Effort,
}

/// Runs the TUI directly in PR-compose mode (`wt pr open`). Seeds the form from
/// `seed`, optionally drafting the title/body with the code agent first
/// (`draft_ai`), then lets the user edit and submit. Returns the submit outcome,
/// or `None` if the user cancels (Esc/quit). The compose form uses its own event
/// loop so it can carry the gathered `ctx`, the resolved `action`, and the
/// resulting outcome.
pub fn run_pr_compose(
    cx: &mut Cx,
    session: &Session,
    ctx: sendit::PrContext,
    action: sendit::PrAction,
    seed: ComposeSeed,
    draft_ai: bool,
) -> Result<Option<(sendit::PrOutcome, sendit::PrSpec)>> {
    let git = cx.git.clone();
    let mut app = build_app(cx, session, git.as_ref())?;
    let action_label = match action {
        sendit::PrAction::Create => "create".to_string(),
        sendit::PrAction::Update { number } => format!("update #{number}"),
    };
    app.mode = Mode::PrCompose(PrComposeState {
        title: seed.title,
        body: seed.body,
        draft: seed.draft,
        branch: ctx.branch.clone(),
        trunk: ctx.trunk.clone(),
        action_label,
        model: seed.model,
        effort: seed.effort,
        ..Default::default()
    });

    let initial = if draft_ai {
        Effect::DraftPrAi
    } else {
        Effect::None
    };
    let mut outcome: Option<(sendit::PrOutcome, sendit::PrSpec)> = None;
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_compose_loop(
        cx,
        session,
        &mut app,
        &ctx,
        action,
        initial,
        &mut outcome,
    ))?;

    if app.too_small {
        cx.err.line("terminal too small (need ≥5 rows)")?;
        return Err(Error::operation("terminal too small"));
    }
    Ok(outcome)
}

/// The compose-mode event loop (terminal shell; not unit-tested). Mirrors
/// [`run_loop`] but carries the PR `ctx`/`action`/`outcome` the compose effects
/// need; the `do_*` helpers it delegates to are tested.
async fn run_compose_loop(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    ctx: &sendit::PrContext,
    action: sendit::PrAction,
    initial: Effect,
    outcome: &mut Option<(sendit::PrOutcome, sendit::PrSpec)>,
) -> Result<()> {
    install_panic_hook();
    let mut tui = Tui::enter(app.mouse)?;
    app.size = tui.size();
    if app.size.1 < crate::tui::app::MIN_HEIGHT {
        app.too_small = true;
        return Ok(());
    }
    tui.draw(app)?;

    if initial != Effect::None
        && compose_dispatch(cx, session, app, &mut tui, ctx, action, initial, outcome)?
    {
        return Ok(());
    }
    tui.draw(app)?;

    let mut events = EventStream::new();
    while let Some(maybe) = events.next().await {
        let Ok(event) = maybe else { continue };
        let effect = app.handle_event(event);
        if compose_dispatch(cx, session, app, &mut tui, ctx, action, effect, outcome)? {
            break;
        }
        tui.draw(app)?;
    }
    Ok(())
}

/// Executes a compose-mode effect, returning `true` when the loop should exit
/// (a successful submit, a quit, or a cancel — the user leaving compose mode via
/// Esc). Terminal shell; the `do_*` helpers it calls are tested.
#[allow(clippy::too_many_arguments)]
fn compose_dispatch(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    tui: &mut Tui,
    ctx: &sendit::PrContext,
    action: sendit::PrAction,
    effect: Effect,
    outcome: &mut Option<(sendit::PrOutcome, sendit::PrSpec)>,
) -> Result<bool> {
    match effect {
        Effect::Quit => Ok(true),
        Effect::TooSmall => {
            app.too_small = true;
            Ok(true)
        }
        Effect::DraftPrAi => {
            tui.suspend()?;
            do_draft_pr_ai(cx, session, app, ctx);
            tui.resume()?;
            Ok(false)
        }
        Effect::SubmitPr { title, body, draft } => {
            tui.suspend()?;
            let done = do_submit_pr(cx, session, app, ctx, action, title, body, draft, outcome);
            tui.resume()?;
            Ok(done)
        }
        // Any other effect (typically `None`): exit only if the user left compose
        // mode (Esc sets the mode back to List), which we treat as a cancel.
        _ => Ok(!matches!(app.mode, Mode::PrCompose(_))),
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

/// Launches the editor in the foreground (terminal already suspended).
fn run_editor(cx: &Cx, session: &Session, path: &Path) {
    let Ok(editor) = resolve_editor(session.config.editor.as_deref(), &cx.env) else {
        return;
    };
    let argv = editor_argv(&editor);
    if let Some((program, rest)) = argv.split_first() {
        let _ = std::process::Command::new(program)
            .args(rest)
            .arg(path)
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{FakeGh, TestRepo, test_cx};
    use crate::tui::app::Mode;
    use std::sync::Arc as StdArc;

    /// Builds a session + app over a real repo for testing the `do_*` helpers.
    fn setup(repo: &TestRepo) -> (crate::testutil::TestCx, Session, App) {
        let t = test_cx(&[], repo.root().to_str().unwrap());
        let session = open_session(&t.cx, &crate::git::RealGit).unwrap();
        let worktrees = build_rows(&session.repo, &crate::git::RealGit).unwrap();
        let app = App::new(worktrees, app_config(&session.config, true), (100, 30));
        (t, session, app)
    }

    #[test]
    fn app_config_maps_settings() {
        let config = Config {
            ui_nerd_fonts: true,
            ui_mouse: false,
            ..Config::default()
        };
        let cfg = app_config(&config, false);
        assert!(cfg.nerd_fonts);
        assert!(!cfg.mouse);
        assert!(!cfg.color);
        assert!(app_config(&config, true).color);
    }

    #[test]
    fn do_create_adds_a_worktree_and_refreshes() {
        let repo = TestRepo::init();
        let (mut t, session, mut app) = setup(&repo);
        app.mode = Mode::Create(Default::default());
        do_create(&mut t.cx, &session, &mut app, "feature/new".into(), None);
        assert_eq!(app.mode, Mode::List);
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("feature/new"))
        );
        assert!(app.status_message.as_deref().unwrap().contains("created"));
    }

    #[test]
    fn do_create_error_shows_in_modal() {
        let repo = TestRepo::init();
        let (mut t, session, mut app) = setup(&repo);
        app.mode = Mode::Create(Default::default());
        // A base ref that does not exist -> error surfaced in the modal.
        do_create(
            &mut t.cx,
            &session,
            &mut app,
            "x".into(),
            Some("nope-ref".into()),
        );
        if let Mode::Create(state) = &app.mode {
            assert!(state.error.is_some());
        } else {
            panic!("expected create mode with error");
        }
    }

    #[test]
    fn do_remove_removes_selected() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        // Give it an upstream so it is not "unpushed" (remove uses force anyway).
        let (mut t, session, mut app) = setup(&repo);
        let index = app
            .worktrees
            .iter()
            .position(|w| w.branch.as_deref() == Some("feature/x"))
            .unwrap();
        do_remove(&mut t.cx, &session, &mut app, index);
        // The worktree is gone; the branch itself survives (not wt-created) and now
        // shows as a worktree-less branch row, so assert on the worktree row only.
        assert!(
            !app.worktrees
                .iter()
                .any(|w| w.has_worktree && w.branch.as_deref() == Some("feature/x"))
        );
        assert!(
            app.worktrees
                .iter()
                .any(|w| !w.has_worktree && w.branch.as_deref() == Some("feature/x"))
        );
    }

    #[test]
    fn do_materialize_branch_creates_worktree_and_switches() {
        // A worktree-less branch (issue #47): materializing it creates a worktree,
        // refreshes so the row becomes a real worktree, and records it as `chosen`.
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        let (mut t, session, mut app) = setup(&repo);
        // Precondition: `topic` starts as a branch row with no worktree.
        assert!(
            app.worktrees
                .iter()
                .any(|w| !w.has_worktree && w.branch.as_deref() == Some("topic"))
        );
        app.mode = Mode::ConfirmCreate(0);
        do_materialize_branch(&mut t.cx, &session, &mut app, "topic".into());
        assert_eq!(app.mode, Mode::List);
        let chosen = app.chosen.clone().expect("chosen path set on materialize");
        assert!(chosen.is_dir());
        // `topic` is now a real worktree row, not a branch row.
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.has_worktree && w.branch.as_deref() == Some("topic"))
        );
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("created topic")
        );
    }

    #[test]
    fn do_materialize_branch_error_shows_in_status() {
        // Creating a worktree for a branch that is already checked out elsewhere
        // fails; the error surfaces in the status bar and nothing is chosen.
        let repo = TestRepo::init();
        repo.add_worktree("dup", "../manual-dup");
        let (mut t, session, mut app) = setup(&repo);
        do_materialize_branch(&mut t.cx, &session, &mut app, "dup".into());
        assert!(app.chosen.is_none());
        assert_eq!(app.status_kind, StatusKind::Error);
        assert!(app.status_message.is_some());
    }

    #[test]
    fn do_fetch_prs_populates_picker() {
        let repo = TestRepo::init();
        let (mut t, session, mut app) = setup(&repo);
        t.cx.gh = StdArc::new(FakeGh::with_list(vec![crate::gh::PrSummary {
            number: 5,
            title: "T".into(),
            author: crate::gh::Author {
                login: "alice".into(),
            },
            state: "OPEN".into(),
            is_draft: false,
            head_ref_name: "h".into(),
            created_at: String::new(),
        }]));
        app.mode = Mode::PrPicker(Default::default());
        do_fetch_prs(&t.cx, &session, &mut app);
        if let Mode::PrPicker(state) = &app.mode {
            assert!(!state.loading);
            assert_eq!(state.prs.len(), 1);
            assert_eq!(state.prs[0].number, 5);
        } else {
            panic!("expected pr picker");
        }
    }

    #[test]
    fn do_fetch_prs_surfaces_gh_error() {
        let repo = TestRepo::init();
        let (mut t, session, mut app) = setup(&repo);
        t.cx.gh = StdArc::new(FakeGh::unavailable());
        app.mode = Mode::PrPicker(Default::default());
        do_fetch_prs(&t.cx, &session, &mut app);
        if let Mode::PrPicker(state) = &app.mode {
            assert!(state.error.is_some());
        } else {
            panic!("expected pr picker");
        }
    }

    #[test]
    fn do_refresh_reloads_worktrees() {
        let repo = TestRepo::init();
        let (t, session, mut app) = setup(&repo);
        // Create a worktree out-of-band, then refresh.
        repo.add_worktree("added", "../wt-added");
        do_refresh(&t.cx, &mut app, &session.primary_root);
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("added"))
        );
    }

    /// Sets up a fetchable `pull/<n>/head` ref served by an `origin` remote
    /// pointing at the repo itself, so `do_checkout_pr` can fetch a real head.
    fn repo_with_pr(number: u64) -> TestRepo {
        let repo = TestRepo::init();
        repo.write("pr.txt", "from pr\n");
        repo.commit_all("pr commit");
        let pr_oid = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", &format!("refs/pull/{number}/head"), &pr_oid]);
        repo.git(&["reset", "-q", "--hard", "HEAD~1"]);
        repo.git(&["remote", "add", "origin", repo.root().to_str().unwrap()]);
        repo
    }

    fn pr_view(number: u64, head: &str, base: &str) -> crate::gh::PrView {
        crate::gh::PrView {
            number,
            title: "Add login".into(),
            state: "OPEN".into(),
            is_draft: false,
            head_ref_name: head.into(),
            base_ref_name: base.into(),
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    #[test]
    fn do_checkout_pr_switches_when_exit_flag_set() {
        // The `wt pr` picker entry: selecting a PR checks it out and exits the
        // loop with the new worktree as `chosen` (honoring the nav contract).
        let repo = repo_with_pr(123);
        let (mut t, session, mut app) = setup(&repo);
        t.cx.gh = StdArc::new(FakeGh::with_view(pr_view(123, "pr-feature", "main")));
        app.exit_on_pr_checkout = true;
        app.mode = Mode::PrPicker(Default::default());
        do_checkout_pr(&mut t.cx, &session, &mut app, 123);
        let path = app.chosen.clone().expect("chosen path set on checkout");
        assert!(path.to_string_lossy().ends_with("pr-feature"));
        assert!(path.is_dir());
    }

    #[test]
    fn do_checkout_pr_stays_in_list_without_exit_flag() {
        // The in-TUI `p`-key flow: checkout returns to the list and refreshes,
        // leaving `chosen` unset so the TUI keeps running.
        let repo = repo_with_pr(55);
        let (mut t, session, mut app) = setup(&repo);
        t.cx.gh = StdArc::new(FakeGh::with_view(pr_view(55, "pr-feature", "main")));
        app.mode = Mode::PrPicker(Default::default());
        do_checkout_pr(&mut t.cx, &session, &mut app, 55);
        assert!(app.chosen.is_none());
        assert_eq!(app.mode, Mode::List);
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("checked out")
        );
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("pr-feature"))
        );
    }

    #[test]
    fn do_checkout_branch_switches_and_stays_in_list() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        let (mut t, session, mut app) = setup(&repo);
        app.mode = Mode::Checkout(crate::tui::app::CheckoutState {
            worktree_index: 0,
            ..Default::default()
        });
        do_checkout_branch(&mut t.cx, &session, &mut app, 0, "topic".into());
        // Stays in the list (no `cd`), refreshed, with a success status.
        assert_eq!(app.mode, Mode::List);
        assert!(app.chosen.is_none());
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("checked out topic")
        );
        // The (primary) worktree now has `topic` checked out.
        assert_eq!(
            repo.git(&["rev-parse", "--abbrev-ref", "HEAD"]).trim(),
            "topic"
        );
    }

    #[test]
    fn do_checkout_branch_dirty_shows_error_in_picker() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        repo.write("README.md", "dirty\n"); // a tracked modification
        let (mut t, session, mut app) = setup(&repo);
        app.mode = Mode::Checkout(crate::tui::app::CheckoutState {
            worktree_index: 0,
            submitting: true,
            ..Default::default()
        });
        do_checkout_branch(&mut t.cx, &session, &mut app, 0, "topic".into());
        if let Mode::Checkout(state) = &app.mode {
            assert!(state.error.as_deref().unwrap().contains("uncommitted"));
            assert!(!state.submitting);
        } else {
            panic!("expected checkout picker with error");
        }
    }

    fn sendit_ctx(branch: &str, trunk: &str, has_upstream: bool) -> sendit::PrContext {
        sendit::PrContext {
            branch: branch.into(),
            trunk: trunk.into(),
            merge_base: "abc".into(),
            has_upstream,
            commits_ahead: 1,
            commit_log: vec![],
            diffstat: sendit::DiffStat {
                files: 1,
                insertions: 1,
                deletions: 0,
                raw: String::new(),
            },
            existing_pr: None,
        }
    }

    /// A feature repo (`feat`, one commit ahead of `main`) with a bare `origin`.
    fn feature_repo_with_remote() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "feat"]);
        repo.write("f.txt", "x\n");
        repo.commit_all("feat work");
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        (repo, bare)
    }

    #[test]
    fn do_draft_pr_ai_seeds_form() {
        let repo = TestRepo::init();
        let (mut t, session, mut app) = setup(&repo);
        t.cx.agent = StdArc::new(crate::testutil::FakeAgent::drafting(
            "Add login\n\nBody here",
        ));
        app.mode = Mode::PrCompose(crate::tui::app::PrComposeState::default());
        do_draft_pr_ai(
            &mut t.cx,
            &session,
            &mut app,
            &sendit_ctx("feat", "main", false),
        );
        if let Mode::PrCompose(s) = &app.mode {
            assert_eq!(s.title, "Add login");
            assert_eq!(s.body, "Body here");
            assert!(s.error.is_none());
        } else {
            panic!("expected compose mode");
        }
    }

    #[test]
    fn do_draft_pr_ai_shows_error_when_unavailable() {
        let repo = TestRepo::init();
        // The default test agent is `FakeAgent::unavailable()`.
        let (mut t, session, mut app) = setup(&repo);
        app.mode = Mode::PrCompose(crate::tui::app::PrComposeState::default());
        do_draft_pr_ai(
            &mut t.cx,
            &session,
            &mut app,
            &sendit_ctx("feat", "main", false),
        );
        if let Mode::PrCompose(s) = &app.mode {
            assert!(s.error.is_some());
        } else {
            panic!("expected compose mode");
        }
    }

    #[test]
    fn do_draft_pr_ai_uses_form_model_and_effort() {
        let repo = TestRepo::init();
        let (mut t, session, mut app) = setup(&repo);
        let agent = StdArc::new(crate::testutil::FakeAgent::drafting("T\n\nB"));
        t.cx.agent = agent.clone();
        app.mode = Mode::PrCompose(crate::tui::app::PrComposeState {
            model: crate::agent::AgentModel::Opus,
            effort: crate::agent::Effort::High,
            ..Default::default()
        });
        do_draft_pr_ai(
            &mut t.cx,
            &session,
            &mut app,
            &sendit_ctx("feat", "main", false),
        );
        // The model/effort selected in the form were passed to the agent.
        assert_eq!(
            agent.last_opts(),
            Some(crate::agent::AgentOptions {
                model: crate::agent::AgentModel::Opus,
                effort: crate::agent::Effort::High,
            })
        );
    }

    #[test]
    fn do_submit_pr_creates_records_and_exits() {
        let (repo, _bare) = feature_repo_with_remote();
        let (mut t, session, mut app) = setup(&repo);
        t.cx.gh = StdArc::new(FakeGh::sender("https://github.com/o/r/pull/77\n"));
        app.mode = Mode::PrCompose(crate::tui::app::PrComposeState::default());
        let mut outcome = None;
        let done = do_submit_pr(
            &mut t.cx,
            &session,
            &mut app,
            &sendit_ctx("feat", "main", false),
            sendit::PrAction::Create,
            "T".into(),
            "B".into(),
            false,
            &mut outcome,
        );
        assert!(done);
        assert_eq!(outcome.expect("outcome").0.number, Some(77));
        assert_eq!(
            repo.git(&["config", "--get", "wt.feat.prNumber"]).trim(),
            "77"
        );
    }

    #[test]
    fn do_submit_pr_error_stays_in_form() {
        let (repo, _bare) = feature_repo_with_remote();
        let (mut t, session, mut app) = setup(&repo);
        t.cx.gh = StdArc::new(FakeGh::unavailable());
        app.mode = Mode::PrCompose(crate::tui::app::PrComposeState {
            submitting: true,
            ..Default::default()
        });
        let mut outcome = None;
        let done = do_submit_pr(
            &mut t.cx,
            &session,
            &mut app,
            &sendit_ctx("feat", "main", false),
            sendit::PrAction::Create,
            "T".into(),
            "B".into(),
            false,
            &mut outcome,
        );
        assert!(!done);
        assert!(outcome.is_none());
        if let Mode::PrCompose(s) = &app.mode {
            assert!(s.error.is_some());
            assert!(!s.submitting);
        } else {
            panic!("expected compose mode");
        }
    }

    #[test]
    fn do_checkout_pr_surfaces_gh_error_in_picker() {
        let repo = TestRepo::init();
        let (mut t, session, mut app) = setup(&repo);
        t.cx.gh = StdArc::new(FakeGh::unavailable());
        app.mode = Mode::PrPicker(Default::default());
        do_checkout_pr(&mut t.cx, &session, &mut app, 1);
        if let Mode::PrPicker(state) = &app.mode {
            assert!(state.error.is_some());
        } else {
            panic!("expected pr picker with error");
        }
        assert!(app.chosen.is_none());
    }

    #[test]
    fn is_background_action_matches_mutations_only() {
        assert!(is_background_action(&Effect::Create {
            branch: "x".into(),
            base: None
        }));
        assert!(is_background_action(&Effect::Remove(0)));
        assert!(is_background_action(&Effect::MaterializeBranch {
            branch: "x".into()
        }));
        assert!(is_background_action(&Effect::CheckoutPr(1)));
        assert!(is_background_action(&Effect::CheckoutBranch {
            worktree_index: 0,
            branch: "x".into()
        }));
        // Non-mutating effects run inline, not on a background task.
        assert!(!is_background_action(&Effect::Refresh));
        assert!(!is_background_action(&Effect::FetchPrs));
        assert!(!is_background_action(&Effect::None));
        assert!(!is_background_action(&Effect::OpenEditor("/tmp".into())));
    }

    #[test]
    fn begin_job_sets_label_and_resolves_args() {
        use crate::tui::app::testutil::app as make_app;
        let mut a = make_app(&[("main", true), ("feat/x", false)]);

        let job = begin_job(
            &mut a,
            Effect::Create {
                branch: "feat/new".into(),
                base: Some("main".into()),
            },
        )
        .unwrap();
        assert!(matches!(job, Job::Create { .. }));
        assert_eq!(a.busy.as_ref().unwrap().label, "Creating feat/new");

        // Remove resolves the query from the target row's branch.
        let job = begin_job(&mut a, Effect::Remove(1)).unwrap();
        assert!(matches!(job, Job::Remove { query } if query == "feat/x"));
        assert_eq!(a.busy.as_ref().unwrap().label, "Removing feat/x");

        // CheckoutBranch resolves the worktree directory from the row.
        let job = begin_job(
            &mut a,
            Effect::CheckoutBranch {
                worktree_index: 0,
                branch: "feat/x".into(),
            },
        )
        .unwrap();
        assert!(matches!(job, Job::CheckoutBranch { .. }));
        assert_eq!(a.busy.as_ref().unwrap().label, "Checking out feat/x");

        let job = begin_job(&mut a, Effect::CheckoutPr(7)).unwrap();
        assert!(matches!(job, Job::CheckoutPr { number } if number == 7));
        assert_eq!(a.busy.as_ref().unwrap().label, "Checking out PR #7");
    }

    #[test]
    fn begin_job_returns_none_and_stays_idle_for_missing_row() {
        use crate::tui::app::testutil::app as make_app;
        let mut a = make_app(&[("main", true)]);
        assert!(begin_job(&mut a, Effect::Remove(99)).is_none());
        assert!(
            begin_job(
                &mut a,
                Effect::CheckoutBranch {
                    worktree_index: 99,
                    branch: "x".into()
                }
            )
            .is_none()
        );
        // A non-background effect also yields no job.
        assert!(begin_job(&mut a, Effect::Refresh).is_none());
        // None of those marked the app busy.
        assert!(!a.is_busy());
    }
}
