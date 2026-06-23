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
use crate::tui::app::{App, AppConfig, Mode, PrComposeState, PrItem, StaleBaseState, StatusKind};
use crate::tui::event::{CreateDecision, Effect};
use crate::tui::terminal::{Tui, install_panic_hook};
use crate::util::editor::{editor_argv, resolve_editor};
use crate::worktree_service::{build_rows, enumerate_rows};

mod effects;
use effects::*;

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
    let opened_in = anchor_at_root(cx, &session);
    let mut app = build_app(cx, &session, git.as_ref())?;
    if let Some(filter) = initial_filter.filter(|f| !f.is_empty()) {
        app.apply_filter(filter.to_string());
    }
    drive_tui(cx, &session, app, Effect::None, &opened_in)
}

/// Runs the TUI directly in PR-picker mode (the `wt pr` no-argument entry).
/// Returns the chosen worktree path once a PR is checked out, or `None` if the
/// user cancels. The picker loads its PRs on open (via an initial `FetchPrs`),
/// and selecting a PR switches into the new worktree (spec §7).
pub fn run_pr_picker(cx: &mut Cx) -> Result<Option<PathBuf>> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let opened_in = anchor_at_root(cx, &session);
    let mut app = build_app(cx, &session, git.as_ref())?;
    app.exit_on_pr_checkout = true;
    app.mode = Mode::PrPicker(crate::tui::app::PrPickerState {
        loading: true,
        ..Default::default()
    });
    drive_tui(cx, &session, app, Effect::FetchPrs, &opened_in)
}

/// Centres the session's git operations on the primary worktree root (issue #68):
/// records the worktree the TUI was opened in, then repoints `cx.cwd` at the root
/// so every subsequent operation — background jobs, refreshes, session rebuilds —
/// anchors at the root rather than the opened-in worktree, which the user may
/// remove during the session (deleting its directory out from under us). The
/// returned path is the opened-in worktree root (the invocation directory for a
/// bare repo), used on exit to detect whether it survived ([`finish_exit`]).
fn anchor_at_root(cx: &mut Cx, session: &Session) -> PathBuf {
    let opened_in = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| cx.cwd.clone());
    cx.cwd = session.primary_root.clone();
    opened_in
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
    app.default_base = crate::git::default_base_ref(session.repo.gix());
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
    opened_in: &Path,
) -> Result<Option<PathBuf>> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_loop(cx, session, &mut app, initial))?;

    if app.too_small {
        cx.err.line("terminal too small (need ≥5 rows)")?;
        return Err(Error::operation("terminal too small"));
    }
    finish_exit(cx, opened_in, &session.primary_root, app.chosen.clone())
}

/// Resolves where the shell lands after a graceful TUI exit (issue #68).
///
/// An explicit switch (`chosen`) is always honoured. Otherwise, when the
/// directory the TUI was opened in was deleted during the session — typically by
/// removing the current worktree from the dashboard — the user must not be left
/// in a directory that no longer exists. In that case the shell is steered back
/// to the repository root (the returned path, printed to stdout) with a friendly
/// note on stderr; if the root is also gone, only the note is emitted and no
/// navigation occurs. When the opened-in directory survives, nothing is printed
/// and the user stays put.
fn finish_exit(
    cx: &mut Cx,
    opened_in: &Path,
    primary_root: &Path,
    chosen: Option<PathBuf>,
) -> Result<Option<PathBuf>> {
    if chosen.is_some() {
        return Ok(chosen);
    }
    if opened_in.exists() {
        return Ok(None);
    }
    if primary_root.exists() {
        cx.err.line(&format!(
            "worktree {} was removed during this session; returning to the repository root at {}",
            opened_in.display(),
            primary_root.display(),
        ))?;
        Ok(Some(primary_root.to_path_buf()))
    } else {
        cx.err.line(&format!(
            "worktree {} was removed during this session, and the repository root is no longer available",
            opened_in.display(),
        ))?;
        Ok(None)
    }
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
        | Effect::DeleteBranch { .. }
        | Effect::MaterializeBranch { .. }
        | Effect::CheckoutPr(_)
        | Effect::CheckoutBranch { .. }
        | Effect::Sync { .. } => Ok(false),
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
            | Effect::DeleteBranch { .. }
            | Effect::MaterializeBranch { .. }
            | Effect::CheckoutPr(_)
            | Effect::CheckoutBranch { .. }
            | Effect::Sync { .. }
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
    /// Create a worktree for a new `branch` based on `base`. `decision` is `None`
    /// to pre-flight the base for staleness (issue #56), else the user's choice.
    Create {
        /// The new branch name.
        branch: String,
        /// The base ref (or `None` for the default).
        base: Option<String>,
        /// The stale-base decision (`None` pre-flights, else update/proceed).
        decision: Option<CreateDecision>,
    },
    /// Remove the worktree matched by `query` (force semantics).
    Remove {
        /// The branch (or directory name) identifying the worktree to remove.
        query: String,
    },
    /// Delete the local branch `branch` of a worktree-less branch row (issue #53).
    DeleteBranch {
        /// The branch to delete.
        branch: String,
        /// Whether to force-delete an unmerged branch (`-D`).
        force: bool,
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
    /// Sync (pull then push) the branch in the worktree at `worktree_dir`.
    Sync {
        /// The target worktree directory.
        worktree_dir: PathBuf,
        /// The branch label for the status text, resolved on the foreground.
        label: String,
    },
}

/// The result of a background [`Job`], carrying the minimum its `apply_*` needs.
/// Errors are stringified inside the job (the typed `Error` is not `'static`-
/// friendly to ferry across the task boundary, and the UI only needs the text).
enum JobOutcome {
    /// A finished create attempt. `branch`/`base` echo back so the stale-base
    /// modal can re-issue the create with a decision (issue #56).
    Create {
        /// The branch that was (to be) created.
        branch: String,
        /// The base ref the create used (echoed for the modal's re-issue).
        base: Option<String>,
        /// Created, awaiting the stale-base confirmation, or failed.
        outcome: CreateOutcome,
    },
    /// A finished remove.
    Remove {
        /// The query that was removed (for the status text).
        query: String,
        /// Success, or the error message to surface.
        result: std::result::Result<(), String>,
    },
    /// A finished branch deletion (issue #53).
    DeleteBranch {
        /// The branch that was deleted (for the status text).
        branch: String,
        /// Whether this was the force-delete attempt; gates the unmerged re-prompt.
        force: bool,
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
    /// A finished sync (issue #63).
    Sync {
        /// The branch label for the status text.
        label: String,
        /// The sync outcome, or the error message.
        result: std::result::Result<commands::sync::SyncOutcome, String>,
    },
}

/// The result of a create job (issue #56). When the pre-flight finds the base
/// behind its upstream it returns `NeedsStaleConfirm` *without* creating, so the
/// loop can open the confirm modal; the modal then re-issues the create with a
/// concrete decision.
enum CreateOutcome {
    /// The worktree was created.
    Created,
    /// The base is behind its upstream; the create paused for confirmation.
    NeedsStaleConfirm {
        /// How many commits the base is behind its upstream.
        behind: u32,
        /// The upstream display name, e.g. `origin/main`.
        upstream_display: String,
        /// Whether the base can be fast-forwarded (else "update" would fail).
        can_fast_forward: bool,
    },
    /// The create failed; the message to surface.
    Failed(String),
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
        Effect::Create {
            branch,
            base,
            decision,
        } => {
            let label = format!("Creating {branch}");
            (
                Job::Create {
                    branch,
                    base,
                    decision,
                },
                label,
            )
        }
        Effect::Remove(index) => {
            let query = remove_query_of(app, index)?;
            let label = format!("Removing {query}");
            (Job::Remove { query }, label)
        }
        Effect::DeleteBranch { branch, force } => {
            let label = format!("Deleting branch {branch}");
            (Job::DeleteBranch { branch, force }, label)
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
        Effect::Sync { worktree_index } => {
            let worktree = app.worktrees.get(worktree_index)?;
            let worktree_dir = worktree.path.clone();
            let label = worktree
                .branch
                .clone()
                .unwrap_or_else(|| "worktree".to_string());
            let busy = format!("Syncing {label}");
            (
                Job::Sync {
                    worktree_dir,
                    label,
                },
                busy,
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
        Job::Create {
            branch,
            base,
            decision,
        } => {
            let outcome = run_create_command(&mut cx, &branch, base.clone(), decision);
            JobOutcome::Create {
                branch,
                base,
                outcome,
            }
        }
        Job::Remove { query } => {
            let result = run_remove_command(&mut cx, &query);
            JobOutcome::Remove { query, result }
        }
        Job::DeleteBranch { branch, force } => {
            let result = run_delete_branch_command(&mut cx, &branch, force);
            JobOutcome::DeleteBranch {
                branch,
                force,
                result,
            }
        }
        Job::Materialize { branch } => {
            let result = run_materialize_command(&mut cx, &branch);
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
        Job::Sync {
            worktree_dir,
            label,
        } => {
            let result = run_sync_command(&mut cx, &worktree_dir);
            JobOutcome::Sync { label, result }
        }
    }
}

/// Applies a finished [`JobOutcome`] to the app exactly as the inline handlers
/// did before (issue #46): status text, mode, refresh, and `chosen`.
fn apply_outcome(cx: &Cx, session: &Session, app: &mut App, outcome: JobOutcome) {
    let root = &session.primary_root;
    match outcome {
        JobOutcome::Create {
            branch,
            base,
            outcome,
        } => apply_create(cx, app, &branch, base, outcome, root),
        JobOutcome::Remove { query, result } => apply_remove(cx, app, &query, result, root),
        JobOutcome::DeleteBranch {
            branch,
            force,
            result,
        } => apply_delete_branch(cx, app, &branch, force, result, root),
        JobOutcome::Materialize { branch, result } => {
            apply_materialize(cx, app, &branch, result, root)
        }
        JobOutcome::CheckoutPr { number, result } => {
            apply_checkout_pr(cx, app, number, result, root)
        }
        JobOutcome::CheckoutBranch { branch, result } => {
            apply_checkout_branch(cx, app, &branch, result, root)
        }
        JobOutcome::Sync { label, result } => apply_sync(cx, app, &label, result, root),
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
pub(crate) fn run_pr_compose(
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
        // The newly created worktree is focused, not the prior selection (issue
        // #52). `main` is the initial `is_current` row, so a non-focused create
        // would leave it selected.
        assert_eq!(
            app.selected_worktree().unwrap().branch.as_deref(),
            Some("feature/new")
        );
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

    /// Leaves local `main` one commit behind `origin/main` (upstream configured,
    /// no fetchable remote so the check's fetch is skipped). Returns origin's tip.
    fn main_behind_origin(repo: &TestRepo) -> String {
        let c1 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("u.txt", "1\n");
        repo.commit_all("ahead on origin");
        let c2 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &c2]);
        repo.git(&["reset", "-q", "--hard", &c1]);
        repo.git(&["config", "branch.main.remote", "origin"]);
        repo.git(&["config", "branch.main.merge", "refs/heads/main"]);
        c2
    }

    #[test]
    fn do_create_stale_base_opens_confirm_modal() {
        // The default base (main) is behind origin/main, so the create pauses at
        // the stale-base confirm modal instead of creating (issue #56).
        let repo = TestRepo::init();
        main_behind_origin(&repo);
        let (mut t, session, mut app) = setup(&repo);
        app.mode = Mode::Create(Default::default());
        do_create(&mut t.cx, &session, &mut app, "feature".into(), None);
        match &app.mode {
            Mode::ConfirmStaleBase(s) => {
                assert_eq!(s.branch, "feature");
                assert_eq!(s.behind, 1);
                assert!(s.can_fast_forward);
            }
            other => panic!("expected ConfirmStaleBase, got {other:?}"),
        }
        // Nothing was created.
        assert!(
            !app.worktrees
                .iter()
                .any(|w| w.has_worktree && w.branch.as_deref() == Some("feature"))
        );
    }

    #[test]
    fn create_update_decision_fast_forwards_then_creates() {
        // Re-issuing the create with the Update decision (as the modal does)
        // fast-forwards the base and forks the new branch from it (issue #56).
        let repo = TestRepo::init();
        let c2 = main_behind_origin(&repo);
        let (t, session, mut app) = setup(&repo);
        let outcome = run_job(
            JobCx::capture(&t.cx),
            Job::Create {
                branch: "feature".into(),
                base: None,
                decision: Some(CreateDecision::Update),
            },
        );
        apply_outcome(&t.cx, &session, &mut app, outcome);
        assert_eq!(app.mode, Mode::List);
        assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]).trim(), c2);
        assert_eq!(repo.git(&["rev-parse", "refs/heads/feature"]).trim(), c2);
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
    fn do_delete_branch_removes_branch_row_and_refreshes() {
        // A worktree-less branch row (issue #53): deleting it removes the local
        // branch and refreshes so the row disappears.
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]); // a merged branch row, no worktree
        let (mut t, session, mut app) = setup(&repo);
        assert!(
            app.worktrees
                .iter()
                .any(|w| !w.has_worktree && w.branch.as_deref() == Some("topic"))
        );
        do_delete_branch(&mut t.cx, &session, &mut app, "topic".into(), false);
        assert_eq!(app.mode, Mode::List);
        assert!(
            !app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("topic"))
        );
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("deleted branch topic")
        );
    }

    #[test]
    fn do_delete_branch_unmerged_reprompts_then_force_deletes() {
        // Deleting an unmerged branch row is refused by the safe `-d` and re-opens
        // the confirm in force mode (issue #53); a forced delete then removes it.
        let repo = TestRepo::init();
        // An unmerged branch with no worktree: branch off in a temp worktree,
        // commit, then drop the worktree but keep the branch.
        repo.add_worktree("unmerged", "../wt-unmerged");
        let wt = repo.root().parent().unwrap().join("wt-unmerged");
        std::fs::write(wt.join("c.txt"), "x\n").unwrap();
        let dir = wt.to_string_lossy().into_owned();
        repo.git(&["-C", &dir, "add", "-A"]);
        repo.git(&["-C", &dir, "commit", "-q", "-m", "unmerged change"]);
        repo.git(&["worktree", "remove", "--force", &dir]);
        let (mut t, session, mut app) = setup(&repo);
        // A safe delete is refused -> re-prompt in force mode.
        do_delete_branch(&mut t.cx, &session, &mut app, "unmerged".into(), false);
        assert!(matches!(
            app.mode,
            Mode::ConfirmDeleteBranch { force: true, .. }
        ));
        assert!(
            app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("unmerged"))
        );
        // The forced delete removes it.
        do_delete_branch(&mut t.cx, &session, &mut app, "unmerged".into(), true);
        assert_eq!(app.mode, Mode::List);
        assert!(
            !app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("unmerged"))
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

    #[test]
    fn do_sync_fast_forwards_and_refreshes() {
        // The primary worktree's `main` is behind `origin/main` (no fetchable
        // remote, so the fetch is skipped): sync fast-forwards it in place.
        let repo = TestRepo::init();
        let c2 = main_behind_origin(&repo);
        let (mut t, session, mut app) = setup(&repo);
        do_sync(&mut t.cx, &session, &mut app, 0);
        assert_eq!(app.mode, Mode::List);
        assert_eq!(app.status_kind, StatusKind::Success);
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("fast-forwarded")
        );
        assert_eq!(repo.git(&["rev-parse", "main"]).trim(), c2);
    }

    #[test]
    fn do_sync_no_upstream_shows_status() {
        let repo = TestRepo::init();
        let (mut t, session, mut app) = setup(&repo);
        do_sync(&mut t.cx, &session, &mut app, 0); // `main`, no upstream
        assert_eq!(app.mode, Mode::List);
        assert!(
            app.status_message
                .as_deref()
                .unwrap()
                .contains("no upstream")
        );
    }

    #[test]
    fn do_sync_dirty_shows_error_status() {
        let repo = TestRepo::init();
        main_behind_origin(&repo);
        repo.write("README.md", "dirty\n"); // blocks the fast-forward
        let (mut t, session, mut app) = setup(&repo);
        do_sync(&mut t.cx, &session, &mut app, 0);
        assert_eq!(app.status_kind, StatusKind::Error);
        assert!(app.status_message.as_deref().unwrap().contains("dirty"));
    }

    #[test]
    fn do_sync_error_shows_in_status() {
        // A worktree whose directory is gone makes the sync core error out; the
        // message surfaces in the status bar.
        let repo = TestRepo::init();
        repo.add_worktree("feat", "../wt-feat");
        let (mut t, session, mut app) = setup(&repo);
        let index = app
            .worktrees
            .iter()
            .position(|w| w.branch.as_deref() == Some("feat"))
            .unwrap();
        std::fs::remove_dir_all(repo.root().parent().unwrap().join("wt-feat")).unwrap();
        do_sync(&mut t.cx, &session, &mut app, index);
        assert_eq!(app.status_kind, StatusKind::Error);
        assert!(app.status_message.is_some());
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
            base: None,
            decision: None,
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
        assert!(is_background_action(&Effect::Sync { worktree_index: 0 }));
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
                decision: None,
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

        // Sync resolves the worktree directory and labels with the branch.
        let job = begin_job(&mut a, Effect::Sync { worktree_index: 1 }).unwrap();
        assert!(matches!(job, Job::Sync { .. }));
        assert_eq!(a.busy.as_ref().unwrap().label, "Syncing feat/x");

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
        assert!(begin_job(&mut a, Effect::Sync { worktree_index: 99 }).is_none());
        // A non-background effect also yields no job.
        assert!(begin_job(&mut a, Effect::Refresh).is_none());
        // None of those marked the app busy.
        assert!(!a.is_busy());
    }

    #[test]
    fn anchor_at_root_repoints_cwd_and_returns_opened_worktree() {
        // The TUI opened in a linked worktree: git operations should re-anchor at
        // the primary root, while the opened-in worktree path is returned so a
        // later removal can be detected (issue #68).
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let linked = repo.root().parent().unwrap().join("wt-x");
        let mut t = test_cx(&[], linked.to_str().unwrap());
        let session = open_session(&t.cx, &crate::git::RealGit).unwrap();
        let opened_in = anchor_at_root(&mut t.cx, &session);
        assert_eq!(canon(&t.cx.cwd), canon(&session.primary_root));
        assert_eq!(canon(&opened_in), canon(&linked));
    }

    #[test]
    fn removing_opened_in_worktree_keeps_operations_working() {
        // Issue #68: the TUI is opened inside a linked worktree which is then
        // removed during the session. Because git operations are re-anchored at
        // the root, the removal succeeds and a later session-open still works
        // (instead of failing with the worktree's directory gone), and on exit
        // the shell is steered back to the root with a friendly note.
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let linked = repo.root().parent().unwrap().join("wt-x");

        // Open as if the TUI launched inside the linked worktree.
        let mut t = test_cx(&[], linked.to_str().unwrap());
        let session = open_session(&t.cx, &crate::git::RealGit).unwrap();
        let opened_in = anchor_at_root(&mut t.cx, &session);
        assert_eq!(canon(&opened_in), canon(&linked));
        assert_eq!(canon(&t.cx.cwd), canon(&session.primary_root));

        // Remove the worktree the TUI was opened in (the background-job path).
        run_remove_command(&mut t.cx, "feature/x").unwrap();
        assert!(!linked.exists());

        // The session still opens: operations anchor at the surviving root.
        let again = open_session(&t.cx, &crate::git::RealGit).unwrap();
        assert_eq!(canon(&again.primary_root), canon(&session.primary_root));

        // On graceful exit (no explicit switch), navigate back to the root.
        let nav = finish_exit(&mut t.cx, &opened_in, &session.primary_root, None).unwrap();
        assert_eq!(canon(&nav.unwrap()), canon(&session.primary_root));
        assert!(t.err.contents().contains("was removed"));
    }

    #[test]
    fn finish_exit_honors_explicit_switch() {
        // An explicit switch is passed through untouched, even if the opened-in
        // directory is gone.
        let mut t = test_cx(&[], "/work");
        let chosen = PathBuf::from("/somewhere/else");
        let out = finish_exit(
            &mut t.cx,
            Path::new("/deleted"),
            Path::new("/deleted-root"),
            Some(chosen.clone()),
        )
        .unwrap();
        assert_eq!(out, Some(chosen));
        assert!(t.err.contents().is_empty());
    }

    #[test]
    fn finish_exit_stays_put_when_opened_dir_survives() {
        // No switch and the opened-in directory still exists: nothing is printed
        // and the shell stays where it is.
        let dir = tempfile::tempdir().unwrap();
        let mut t = test_cx(&[], "/work");
        let out = finish_exit(&mut t.cx, dir.path(), dir.path(), None).unwrap();
        assert_eq!(out, None);
        assert!(t.err.contents().is_empty());
    }

    #[test]
    fn finish_exit_returns_to_root_when_opened_dir_deleted() {
        // The opened-in worktree was removed during the session but the root
        // survives: navigate to the root and explain the move on stderr.
        let root = tempfile::tempdir().unwrap();
        let gone = root.path().join("wt-x");
        let mut t = test_cx(&[], "/work");
        let out = finish_exit(&mut t.cx, &gone, root.path(), None).unwrap();
        assert_eq!(out.as_deref(), Some(root.path()));
        let err = t.err.contents();
        assert!(err.contains("was removed"));
        assert!(err.contains(&root.path().display().to_string()));
    }

    #[test]
    fn finish_exit_reports_when_root_also_gone() {
        // Both the opened-in worktree and the root are gone: explain the
        // situation and navigate nowhere.
        let scratch = tempfile::tempdir().unwrap();
        let gone = scratch.path().join("wt-x");
        let gone_root = scratch.path().join("root");
        let mut t = test_cx(&[], "/work");
        let out = finish_exit(&mut t.cx, &gone, &gone_root, None).unwrap();
        assert_eq!(out, None);
        assert!(t.err.contents().contains("no longer available"));
    }

    /// Canonicalizes a path so comparisons ignore `/private` symlink prefixes on
    /// macOS temp dirs.
    fn canon(p: &Path) -> PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    }
}
