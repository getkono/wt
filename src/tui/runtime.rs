//! TUI runtime (spec §10): the async event loop that drives [`App`], executes
//! [`Effect`]s, and loads async data. The loop and terminal handling are the
//! thin, untestable shell; the effect-executing `do_*` helpers are pure of the
//! terminal and are unit-tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::EventStream;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::cli::NewArgs;
use crate::commands::{self, Session, open_session};
use crate::config::Config;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::hooks::RealHookRunner;
use crate::model::{SortSpec, Worktree};
use crate::tui::app::{App, AppConfig, Mode, PrComposeState, PrItem, StatusKind};
use crate::tui::event::Effect;
use crate::tui::terminal::{Tui, install_panic_hook};
use crate::util::editor::{editor_argv, resolve_editor};
use crate::worktree_service::{build_worktrees, enumerate_worktrees};

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

/// Builds the [`App`] over the session's worktrees, seeding the branch list.
fn build_app(cx: &Cx, session: &Session, git: &dyn GitCli) -> Result<App> {
    let sync_worktrees = enumerate_worktrees(&session.repo, git)?;
    let size = crossterm::terminal::size().unwrap_or((100, 30));
    // The TUI draws to the alternate screen on stderr, so resolve color against
    // stderr (stdout is reserved for the chosen path and is usually piped).
    let color = cx.color_enabled_err(session.config.ui_color);
    let mut app = App::new(sync_worktrees, app_config(&session.config, color), size);
    app.branches = crate::git::local_branches(session.repo.gix()).unwrap_or_default();
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

    let mut events = EventStream::new();
    loop {
        tokio::select! {
            maybe = events.next() => {
                let Some(Ok(event)) = maybe else { continue };
                let effect = app.handle_event(event);
                if dispatch_effect(cx, session, app, &mut tui, effect)? {
                    break;
                }
                tui.draw(app)?;
            }
            Some(worktrees) = rx.recv() => {
                mark_all_loaded(app, worktrees);
                tui.draw(app)?;
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
        Effect::Create { branch, base } => {
            tui.suspend()?;
            do_create(cx, session, app, branch, base);
            tui.resume()?;
            Ok(false)
        }
        Effect::Remove(index) => {
            tui.suspend()?;
            do_remove(cx, session, app, index);
            tui.resume()?;
            Ok(false)
        }
        Effect::CheckoutPr(number) => {
            tui.suspend()?;
            do_checkout_pr(cx, session, app, number);
            tui.resume()?;
            // A switching checkout (the `wt pr` picker entry) sets `chosen`; exit
            // the loop so the wrapper `cd`s into the new worktree.
            Ok(app.chosen.is_some())
        }
        // Compose-only effects are driven by the dedicated compose loop
        // ([`run_pr_compose`]) and never reach the main loop.
        Effect::DraftPrAi | Effect::SubmitPr { .. } => Ok(false),
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
            && let Ok(worktrees) = build_worktrees(&repo, git.as_ref())
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
        // Refresh the base-ref completion candidates so a just-created branch
        // becomes completable (best-effort; keep the old list on failure).
        if let Ok(branches) = crate::git::local_branches(repo.gix()) {
            app.branches = branches;
        }
        if let Ok(worktrees) = build_worktrees(&repo, git.as_ref()) {
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

/// Creates a worktree and refreshes; errors show inline in the create modal.
pub(crate) fn do_create(
    cx: &mut Cx,
    session: &Session,
    app: &mut App,
    branch: String,
    base: Option<String>,
) {
    let args = NewArgs {
        branch: branch.clone(),
        from: base,
        no_switch: true,
        no_hooks: false,
        copy_from: None,
    };
    match commands::new::run(cx, &RealHookRunner, &args, false) {
        Ok(_) => {
            app.mode = Mode::List;
            app.set_status(format!("created {branch}"), StatusKind::Success);
            do_refresh(cx, app, &session.primary_root);
        }
        Err(e) => match &mut app.mode {
            Mode::Create(state) => state.error = Some(e.to_string()),
            _ => app.set_status(e.to_string(), StatusKind::Error),
        },
    }
}

/// Removes the worktree at `index` and refreshes. The confirm dialog is itself
/// the guard, so the worktree is removed even if dirty/unpushed; but an unmerged
/// branch is never force-deleted here (spec §10/§12) — only a fully-merged
/// wt-created branch is cleaned up.
pub(crate) fn do_remove(cx: &mut Cx, session: &Session, app: &mut App, index: usize) {
    let Some(worktree) = app.worktrees.get(index) else {
        return;
    };
    let query = worktree.branch.clone().unwrap_or_else(|| {
        worktree
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let opts = commands::remove::RemoveOptions {
        force_remove: true,
        force_branch: false,
        keep_branch: false,
        no_hooks: false,
    };
    match commands::remove::remove_query(cx, &RealHookRunner, &query, &opts, false) {
        Ok(_) => app.set_status(format!("removed {query}"), StatusKind::Success),
        Err(e) => app.set_status(e.to_string(), StatusKind::Error),
    }
    app.mode = Mode::List;
    do_refresh(cx, app, &session.primary_root);
}

/// Checks out a PR into a worktree. When `app.exit_on_pr_checkout` is set (the
/// `wt pr` picker entry), records the new worktree as `chosen` so the loop exits
/// and the wrapper `cd`s into it; otherwise returns to the list and refreshes
/// (the in-TUI `p`-key flow).
pub(crate) fn do_checkout_pr(cx: &mut Cx, session: &Session, app: &mut App, number: u64) {
    let git = cx.git.clone();
    let gh = cx.gh.clone();
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    match commands::pr::checkout_pr_worktree(
        cx,
        git.as_ref(),
        gh.as_ref(),
        &RealHookRunner,
        session,
        &dir,
        &number.to_string(),
        false,
    ) {
        Ok((path, _existed)) => {
            if app.exit_on_pr_checkout {
                app.chosen = Some(path);
            } else {
                app.mode = Mode::List;
                app.set_status(format!("checked out PR #{number}"), StatusKind::Success);
                do_refresh(cx, app, &session.primary_root);
            }
        }
        Err(e) => match &mut app.mode {
            Mode::PrPicker(state) => state.error = Some(e.to_string()),
            _ => app.set_status(e.to_string(), StatusKind::Error),
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
        let worktrees = build_worktrees(&session.repo, &crate::git::RealGit).unwrap();
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
        assert!(
            !app.worktrees
                .iter()
                .any(|w| w.branch.as_deref() == Some("feature/x"))
        );
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
}
