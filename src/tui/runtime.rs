//! TUI runtime (spec §10): the async event loop that drives [`App`], executes
//! [`Effect`]s, and loads async data. The loop and terminal handling are the
//! thin, untestable shell; the effect-executing `do_*` helpers are pure of the
//! terminal and are unit-tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossterm::event::EventStream;
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::cli::{NewArgs, PrArgs};
use crate::commands::{self, Session, open_session};
use crate::config::Config;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::hooks::RealHookRunner;
use crate::model::{SortSpec, Worktree};
use crate::tui::app::{App, AppConfig, Mode, PrItem, StatusKind};
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
    }
}

/// Runs the TUI, returning the chosen worktree path (if the user switched).
pub fn run_tui(cx: &mut Cx) -> Result<Option<PathBuf>> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let sync_worktrees = enumerate_worktrees(&session.repo, git.as_ref())?;
    let size = crossterm::terminal::size().unwrap_or((100, 30));
    // The TUI draws to the alternate screen on stderr, so resolve color against
    // stderr (stdout is reserved for the chosen path and is usually piped).
    let color = cx.color_enabled_err(session.config.ui_color);
    let mut app = App::new(sync_worktrees, app_config(&session.config, color), size);
    app.mark_loading();

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_loop(cx, &session, &mut app))?;

    if app.too_small {
        cx.err.line("terminal too small (need ≥5 rows)")?;
        return Err(Error::operation("terminal too small"));
    }
    Ok(app.chosen.clone())
}

/// The async event loop (terminal shell; not unit-tested).
async fn run_loop(cx: &mut Cx, session: &Session, app: &mut App) -> Result<()> {
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
            Ok(false)
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
    if let Ok(repo) = Repo::discover(root)
        && let Ok(worktrees) = build_worktrees(&repo, git.as_ref())
    {
        mark_all_loaded(app, worktrees);
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

/// Checks out a PR into a new worktree and refreshes.
pub(crate) fn do_checkout_pr(cx: &mut Cx, session: &Session, app: &mut App, number: u64) {
    let args = PrArgs {
        target: Some(number.to_string()),
        no_switch: true,
        no_hooks: false,
        sub: None,
    };
    match commands::pr::run(cx, &RealHookRunner, &args, false) {
        Ok(_) => {
            app.mode = Mode::List;
            app.set_status(format!("checked out PR #{number}"), StatusKind::Success);
            do_refresh(cx, app, &session.primary_root);
        }
        Err(e) => match &mut app.mode {
            Mode::PrPicker(state) => state.error = Some(e.to_string()),
            _ => app.set_status(e.to_string(), StatusKind::Error),
        },
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
}
