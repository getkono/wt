//! TUI rendering (spec §10): the list pane, detail pane, status bar, and modal
//! overlays. Rendering is a pure function of [`App`] state into a ratatui
//! [`Frame`], so it is testable with a `TestBackend`. Color comes from the
//! resolved [`Theme`] (spec §11); when color is disabled the styles collapse to
//! the monochrome look (dim/bold/reversed only).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState, Wrap,
};

use crate::agent::{AgentModel, Effort};
use crate::keys::KeyAction;
use crate::model::{MergeState, PrState, SortKey, SortSpec, Worktree};
use crate::output::render::branch_display;
use crate::time::{now_unix, parse_iso8601, relative};
use crate::tui::app::{
    App, CheckoutState, ComposeField, CreateState, CreateStep, ExitBlockedState, ExitIntent,
    InitSubmodulesState, Mode, Pane, PrComposeState, PrPickerState, StaleBaseState,
};
use crate::tui::glyphs::Glyphs;
use crate::tui::hints::{self, Hint};
use crate::tui::options::OptionList;
use crate::tui::theme::Theme;

mod detail;
mod list;
mod modals;

/// Renders the whole TUI for the current state.
pub fn render(app: &App, frame: &mut Frame) {
    let area = frame.area();
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let (main, status) = (rows[0], rows[1]);

    if app.show_sidebar && app.detail_visible() {
        let cols = Layout::horizontal([Constraint::Length(app.sidebar_width), Constraint::Min(20)])
            .split(main);
        list::render_list(app, frame, cols[0]);
        detail::render_detail(app, frame, cols[1]);
    } else if app.show_sidebar {
        list::render_list(app, frame, main);
    } else {
        detail::render_detail(app, frame, main);
    }
    render_status_bar(app, frame, status);

    match &app.mode {
        Mode::Help => modals::render_help(app, frame, area),
        Mode::Create(state) => modals::render_create(app, state, frame, area),
        Mode::PrPicker(state) => modals::render_pr_picker(app, state, frame, area),
        Mode::PrCompose(state) => modals::render_pr_compose(app, state, frame, area),
        Mode::Checkout(state) => modals::render_checkout(app, state, frame, area),
        Mode::ConfirmRemove(index) => modals::render_confirm(app, *index, frame, area),
        Mode::ConfirmCreate(index) => modals::render_confirm_create(app, *index, frame, area),
        Mode::ConfirmDeleteBranch { index, force } => {
            modals::render_confirm_delete_branch(app, *index, *force, frame, area)
        }
        Mode::ConfirmStaleBase(state) => modals::render_confirm_stale_base(app, state, frame, area),
        Mode::ConfirmInitSubmodules(state) => {
            modals::render_confirm_init_submodules(app, state, frame, area)
        }
        Mode::ExitBlocked(state) => modals::render_exit_blocked(app, state, frame, area),
        _ => {}
    }
}

/// The ahead/behind cell as spans: green `↑N`, red `↓M`, or the absent marker
/// (spinner while loading).
fn ahead_behind_spans(
    worktree: &Worktree,
    theme: &Theme,
    loaded: bool,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    if !loaded {
        return vec![Span::styled(glyphs.spinner().to_string(), theme.spinner())];
    }
    match (worktree.ahead, worktree.behind) {
        (Some(ahead), Some(behind)) => vec![
            Span::styled(format!("↑{ahead}"), theme.ahead(ahead)),
            Span::raw(" "),
            Span::styled(format!("↓{behind}"), theme.behind(behind)),
        ],
        _ => vec![Span::styled(glyphs.absent().to_string(), theme.absent())],
    }
}

/// The commit cell as spans: orange hash, plain subject, dim relative time.
fn commit_spans(
    worktree: &Worktree,
    theme: &Theme,
    loaded: bool,
    glyphs: &Glyphs,
    now: i64,
) -> Vec<Span<'static>> {
    match (&worktree.commit, loaded) {
        (_, false) => vec![Span::styled(glyphs.spinner().to_string(), theme.spinner())],
        (Some(c), true) => {
            let rel = parse_iso8601(&c.timestamp)
                .map(|u| relative(now, u))
                .unwrap_or_default();
            vec![
                Span::styled(c.hash.clone(), theme.commit_hash()),
                Span::raw(" "),
                Span::raw(c.subject.clone()),
                Span::raw(" "),
                Span::styled(format!("({rel})"), theme.time()),
            ]
        }
        // Loaded, present, but no commit read: failed fetch → absent marker.
        (None, true) if !worktree.is_missing => {
            vec![Span::styled(glyphs.absent().to_string(), theme.absent())]
        }
        (None, true) => Vec::new(),
    }
}

/// The PR cell as a span, colored by PR state (spinner while loading).
fn pr_spans(
    worktree: &Worktree,
    theme: &Theme,
    loaded: bool,
    glyphs: &Glyphs,
) -> Vec<Span<'static>> {
    match (&worktree.pr, loaded) {
        (_, false) => vec![Span::styled(glyphs.spinner().to_string(), theme.spinner())],
        (Some(pr), true) => vec![Span::styled(
            format!("#{} ({})", pr.number, pr.state.as_str()),
            theme.pr_state(pr.state),
        )],
        (None, true) => Vec::new(),
    }
}

/// The dirty label span for the detail pane, colored by state.
fn dirty_label_span(worktree: &Worktree, theme: &Theme) -> Span<'static> {
    match (worktree.dirty, worktree.has_untracked) {
        (Some(true), _) => Span::styled("modified", theme.dirty()),
        (_, Some(true)) => Span::styled("untracked", theme.untracked()),
        (Some(false), _) => Span::styled("clean", theme.hint_label()),
        _ => Span::raw(""),
    }
}

/// The single-line note describing a worktree's merge / unpushed state, shared by
/// the confirm dialog and the detail pane. The reassuring/informative states
/// (merged, upstream-gone) are always returned; the alarming "unpushed work"
/// states (no-upstream-local, and a real ahead count when tracked) are returned
/// only when `include_warnings` is set — i.e. in the destructive confirm flow —
/// so the passive detail pane stays calm. Returns `None` when there is nothing to
/// say (or the row is not yet loaded).
fn merge_state_note(
    worktree: &Worktree,
    theme: &Theme,
    include_warnings: bool,
) -> Option<Line<'static>> {
    match &worktree.merge_state {
        Some(MergeState::Merged { into: Some(base) }) => Some(Line::from(Span::styled(
            format!("(merged into {base} — safe to delete)"),
            theme.success(),
        ))),
        Some(MergeState::Merged { into: None }) => Some(Line::from(Span::styled(
            "(merged via PR — safe to delete)",
            theme.success(),
        ))),
        Some(MergeState::UpstreamGone) => Some(Line::from(Span::styled(
            "(upstream branch deleted — likely merged)",
            theme.label(),
        ))),
        Some(MergeState::NoUpstreamLocal) if include_warnings => Some(Line::from(Span::styled(
            "(no upstream — local-only, unpushed work)",
            theme.warning(),
        ))),
        Some(MergeState::Tracked) | None if include_warnings => match worktree.ahead {
            Some(ahead) if ahead > 0 => Some(Line::from(Span::styled(
                format!("({ahead} unpushed commit(s))"),
                theme.warning(),
            ))),
            _ => None,
        },
        _ => None,
    }
}

/// Renders the bottom status/help bar.
fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
    let mut spans = vec![Span::styled(
        format!(" {} ", mode_label(&app.mode)),
        theme.mode_chip(&app.mode),
    )];
    if !app.filter.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("/{}", app.filter), theme.accent()));
    }
    spans.push(Span::raw("  "));
    // While background jobs run, the status bar leads with an animated spinner and
    // a compact summary of the in-flight work (issue #46 overhaul) so it stays
    // visible even when a job's row is scrolled off; a transient status message
    // (e.g. "created feat/x") follows it, otherwise the key hints do.
    if let Some(summary) = app.job_summary() {
        let glyphs = Glyphs::new(app.nerd_fonts);
        spans.push(Span::styled(
            glyphs.spinner_frame(app.spinner_frame).to_string(),
            theme.spinner(),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(summary, theme.label()));
        if let Some(message) = &app.status_message {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(message.clone(), theme.status(app.status_kind)));
        }
    } else if let Some(message) = &app.status_message {
        spans.push(Span::styled(message.clone(), theme.status(app.status_kind)));
    } else {
        for (i, (key, label)) in mode_hints(app).into_iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(key, theme.hint_key()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(label, theme.hint_label()));
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The short mode name shown in the status-bar chip.
fn mode_label(mode: &Mode) -> &'static str {
    match mode {
        Mode::List => "LIST",
        Mode::Filter => "FILTER",
        Mode::Create(_) => "CREATE",
        Mode::PrPicker(_) => "PR",
        Mode::PrCompose(_) => "COMPOSE",
        Mode::Checkout(_) => "CHECKOUT",
        Mode::ConfirmRemove(_) => "REMOVE",
        Mode::ConfirmCreate(_) => "CREATE",
        Mode::ConfirmDeleteBranch { .. } => "DELETE",
        Mode::ConfirmStaleBase(_) => "CREATE",
        Mode::ConfirmInitSubmodules(_) => "SUBMODULES",
        Mode::ExitBlocked(state) => match state.intent {
            ExitIntent::Quit => "QUIT",
            ExitIntent::Switch(_) => "SWITCH",
        },
        Mode::Help => "HELP",
    }
}

/// The curated subset of rebindable actions shown in the List-mode status bar
/// (the full reference lives in the help overlay). Their key text comes from the
/// live [`Keymap`](crate::keys::Keymap) and their labels from
/// [`KeyAction::label`], so the bar can never drift from the bindings (issue #39).
const LIST_BAR: [KeyAction; 8] = [
    KeyAction::Switch,
    KeyAction::New,
    KeyAction::Remove,
    KeyAction::PrCheckout,
    KeyAction::Checkout,
    KeyAction::Filter,
    KeyAction::Help,
    KeyAction::Quit,
];

/// The right-side key hints for the current mode (spec §10 bottom bar), as
/// `(key, description)` pairs so the keys can be colored. List-mode hints derive
/// from the keymap; modal hints come from the shared [`hints`] tables.
fn mode_hints(app: &App) -> Vec<(String, String)> {
    match &app.mode {
        Mode::List => LIST_BAR
            .iter()
            .filter_map(|&action| {
                app.keymap
                    .display_for(action)
                    .map(|keys| (keys, action.label().to_string()))
            })
            .collect(),
        Mode::Filter => hint_pairs(hints::filter_hints()),
        Mode::Create(_) => hint_pairs(hints::create_hints()),
        Mode::PrPicker(_) => hint_pairs(hints::pr_picker_hints()),
        Mode::PrCompose(_) => hint_pairs(hints::compose_edit_hints()),
        Mode::Checkout(_) => hint_pairs(hints::checkout_hints()),
        Mode::ConfirmRemove(_) => hint_pairs(hints::confirm_hints()),
        Mode::ConfirmCreate(_) => hint_pairs(hints::confirm_create_hints()),
        Mode::ConfirmDeleteBranch { .. } => hint_pairs(hints::confirm_delete_branch_hints()),
        Mode::ConfirmStaleBase(_) => hint_pairs(hints::confirm_stale_base_hints()),
        Mode::ConfirmInitSubmodules(_) => hint_pairs(hints::confirm_init_submodules_hints()),
        Mode::ExitBlocked(_) => hint_pairs(hints::exit_blocked_hints()),
        Mode::Help => hint_pairs(hints::help_hints()),
    }
}

/// Converts a static hint table into owned `(key, label)` pairs for the bar.
fn hint_pairs(table: &[Hint]) -> Vec<(String, String)> {
    table
        .iter()
        .map(|h| (h.key.to_string(), h.label.to_string()))
        .collect()
}

/// Centers a popup `width`×`height` within `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyChord;
    use crate::tui::app::testutil::{app, wt};
    use crate::tui::app::{
        ComposeField, CreateState, PrComposeState, PrItem, PrPickerState, StatusKind,
    };
    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::style::Color;

    /// Renders the app to a TestBackend and returns the buffer as text.
    fn render_to_text(app: &App, w: u16, h: u16) -> String {
        buffer_text(&render_to_buffer(app, w, h))
    }

    /// Renders the app to a TestBackend and returns the raw (styled) buffer.
    fn render_to_buffer(app: &App, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(app, f)).unwrap();
        terminal.backend().buffer().clone()
    }

    /// The foreground color of the first cell rendering `symbol`.
    fn cell_fg(buffer: &Buffer, symbol: &str) -> Color {
        let area = buffer.area;
        for y in 0..area.height {
            for x in 0..area.width {
                if buffer[(x, y)].symbol() == symbol {
                    return buffer[(x, y)].fg;
                }
            }
        }
        panic!("symbol {symbol:?} not found");
    }

    fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
        let area = buffer.area;
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_list_and_detail() {
        let a = app(&[("main", true), ("feature/x", false)]);
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("worktrees"));
        assert!(text.contains("detail"));
        assert!(text.contains("main"));
        assert!(text.contains("feature/x"));
        // The current worktree shows the '*' marker.
        assert!(text.contains('*'));
    }

    #[test]
    fn list_shows_branch_rows_with_marker_and_count() {
        use crate::tui::app::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        let mut br = branch_row("feature/lonely");
        br.ahead = Some(3);
        br.behind = Some(1);
        a.worktrees.push(br);
        a.apply_filter(String::new());
        a.mark_loaded(a.worktrees[1].path.clone());
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("feature/lonely"));
        assert!(text.contains('○')); // the worktree-less marker
        assert!(text.contains("↑3"));
        assert!(text.contains("↓1"));
        // The title tallies branch rows separately from worktrees.
        assert!(text.contains("branches"));
    }

    #[test]
    fn detail_pane_for_branch_row_is_pathless() {
        use crate::tui::app::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        let mut br = branch_row("topic");
        br.ahead = Some(2);
        br.behind = Some(0);
        br.base_ref = Some("main".into());
        a.worktrees.push(br);
        a.apply_filter(String::new());
        a.mark_loaded(a.worktrees[1].path.clone());
        a.selected = 1; // select the branch row
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("no worktree"));
        assert!(text.contains("vs base"));
        assert!(text.contains("base:"));
        // The virtual path key is never surfaced to the user.
        assert!(!text.contains("branch://"));
    }

    #[test]
    fn confirm_create_dialog_renders() {
        use crate::tui::app::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        let mut br = branch_row("topic");
        br.base_ref = Some("main".into());
        br.ahead = Some(2);
        br.behind = Some(0);
        a.worktrees.push(br);
        a.apply_filter(String::new());
        a.mark_loaded(a.worktrees[1].path.clone());
        a.mode = Mode::ConfirmCreate(1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("create worktree"));
        assert!(text.contains("topic"));
        assert!(text.contains("switch into it"));
        assert!(text.contains("[y/N]"));
    }

    #[test]
    fn renders_confirm_init_submodules_modal() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::ConfirmInitSubmodules(InitSubmodulesState {
            dir: std::path::PathBuf::from("/wt/feature"),
            branch: "feature".into(),
            count: 3,
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("initialize submodules"));
        assert!(text.contains("feature"));
        assert!(text.contains("3 uninitialized"));
        // Default-yes prompt (capital Y).
        assert!(text.contains("[Y/n]"));
    }

    #[test]
    fn narrow_terminal_hides_detail() {
        let a = app(&[("main", true)]);
        let mut a = a;
        a.size = (50, 20);
        let text = render_to_text(&a, 50, 20);
        assert!(text.contains("worktrees"));
        assert!(!text.contains("detail")); // detail hidden < 60 cols
    }

    #[test]
    fn pending_rows_show_spinner() {
        let mut a = app(&[("main", true)]);
        a.mark_loading(); // nothing loaded -> spinners
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains('…'));
    }

    #[test]
    fn loaded_no_upstream_shows_absent_marker() {
        // A loaded worktree with no upstream shows the ahead/behind "–".
        let a = app(&[("main", true)]);
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains('–'));
    }

    #[test]
    fn help_overlay_renders() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::Help;
        let text = render_to_text(&a, 100, 40);
        assert!(text.contains("help"));
        assert!(text.contains("navigate"));
        assert!(text.contains("quit"));
        // Regression for #39: checkout (`c`) was bound but undocumented.
        assert!(text.contains("checkout"));
    }

    #[test]
    fn help_overlay_documents_every_action() {
        // The help overlay is generated from the keymap, so every action must
        // appear with its label — the structural guard against hint drift (#39).
        let mut a = app(&[("main", true)]);
        a.mode = Mode::Help;
        let text = render_to_text(&a, 100, 40);
        for action in KeyAction::ALL {
            assert!(
                text.contains(action.label()),
                "help overlay missing label for {action:?}: {:?}",
                action.label()
            );
        }
    }

    #[test]
    fn list_bar_includes_checkout() {
        // The List status bar derives from the keymap; checkout must show with
        // its default `c` binding (the visible half of the #39 fix).
        let a = app(&[("main", true)]);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("checkout"));
        assert!(text.contains(" c "));
    }

    #[test]
    fn list_bar_follows_rebind() {
        // Rebinding an action flows through to the hint: the bar is sourced from
        // the live keymap, not a hardcoded string.
        let mut a = app(&[("main", true)]);
        a.keymap
            .rebind(KeyAction::Checkout, KeyChord::key(KeyCode::Char('x')));
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains(" x "));
    }

    #[test]
    fn create_overlay_shows_fields_and_error() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::Create(CreateState {
            branch: "feat".into(),
            error: Some("branch name is required".into()),
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("new worktree"));
        assert!(text.contains("feat"));
        assert!(text.contains("required"));
    }

    #[test]
    fn create_overlay_shows_open_branch_options() {
        use crate::tui::options::OptionList;
        let mut a = app(&[("main", true)]);
        let mut options = OptionList::new(vec![
            "main".into(),
            "origin/main".into(),
            "origin/dev".into(),
        ]);
        options.open();
        a.mode = Mode::Create(CreateState {
            options,
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        // The dropdown lists the existing branches and marks the cursor row.
        assert!(text.contains("origin/main"));
        assert!(text.contains("origin/dev"));
        assert!(text.contains('▌')); // selection bar on the highlighted row
        assert!(text.contains("options")); // status hint mentions ↑/↓ options
    }

    #[test]
    fn checkout_overlay_renders_branches_and_target() {
        use crate::tui::app::CheckoutState;
        use crate::tui::options::OptionList;
        let mut a = app(&[("main", true), ("feature/x", false)]);
        let mut options = OptionList::new(vec!["main".into(), "feature/x".into()]);
        options.open();
        a.mode = Mode::Checkout(CheckoutState {
            worktree_index: 0,
            query: "feat".into(),
            options,
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("checkout branch"));
        assert!(text.contains("feature/x"));
        assert!(text.contains("branches")); // the hint row
    }

    #[test]
    fn pr_compose_model_field_shows_options_dropdown() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::PrCompose(PrComposeState {
            field: ComposeField::Model,
            branch: "feat".into(),
            trunk: "main".into(),
            action_label: "create".into(),
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        // Every model option is listed, the active field marked with `>`.
        assert!(text.contains("Opus 4.8"));
        assert!(text.contains("Sonnet 4.6"));
        assert!(text.contains("Haiku 4.5"));
        assert!(text.contains("> model:"));
    }

    #[test]
    fn pr_compose_effort_field_shows_options_dropdown() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::PrCompose(PrComposeState {
            field: ComposeField::Effort,
            branch: "feat".into(),
            trunk: "main".into(),
            action_label: "create".into(),
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("low"));
        assert!(text.contains("medium"));
        assert!(text.contains("high"));
    }

    #[test]
    fn pr_compose_overlay_shows_header_fields_and_hints() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::PrCompose(PrComposeState {
            field: ComposeField::Body,
            title: "Add login".into(),
            body: "Summary line".into(),
            draft: true,
            branch: "feat/login".into(),
            trunk: "main".into(),
            action_label: "create".into(),
            error: Some("boom".into()),
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("open pull request"));
        assert!(text.contains("feat/login"));
        assert!(text.contains("Add login"));
        assert!(text.contains("Summary line"));
        assert!(text.contains("[create]"));
        assert!(text.contains("draft [x]"));
        assert!(text.contains("boom"));
        assert!(text.contains("Ctrl-S"));
        // The AI-fill controls and the selected model/effort are shown.
        assert!(text.contains("Ctrl-A"));
        assert!(text.contains("model:"));
        assert!(text.contains("Sonnet 4.6")); // the default model label
        assert!(text.contains("effort:"));
    }

    #[test]
    fn pr_compose_shows_selected_model_and_effort() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::PrCompose(PrComposeState {
            title: "T".into(),
            branch: "feat".into(),
            trunk: "main".into(),
            action_label: "create".into(),
            model: crate::agent::AgentModel::Opus,
            effort: crate::agent::Effort::High,
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("Opus 4.8"));
        assert!(text.contains("high"));
    }

    #[test]
    fn pr_compose_submitting_shows_status() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::PrCompose(PrComposeState {
            title: "T".into(),
            branch: "feat".into(),
            trunk: "main".into(),
            action_label: "update #5".into(),
            submitting: true,
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("working"));
        assert!(text.contains("[update #5]"));
    }

    #[test]
    fn pr_picker_states() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::PrPicker(PrPickerState {
            loading: true,
            ..Default::default()
        });
        assert!(render_to_text(&a, 100, 30).contains("loading"));

        a.mode = Mode::PrPicker(PrPickerState {
            loading: false,
            prs: vec![
                PrItem {
                    number: 42,
                    title: "Add login".into(),
                    author: "alice".into(),
                    state: "open".into(),
                    created_at: "2020-01-01T00:00:00Z".into(),
                },
                // An unparseable timestamp renders an empty age without panicking.
                PrItem {
                    number: 7,
                    title: "No date".into(),
                    author: "bob".into(),
                    state: "open".into(),
                    created_at: String::new(),
                },
            ],
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("#42"));
        assert!(text.contains("Add login"));
        // The age column renders a relative time; the fixed far-past date is
        // always years before "now", so the unit is deterministically years.
        assert!(text.contains("ago"));

        a.mode = Mode::PrPicker(PrPickerState {
            error: Some("gh unavailable".into()),
            ..Default::default()
        });
        assert!(render_to_text(&a, 100, 30).contains("gh auth login"));
    }

    #[test]
    fn confirm_remove_overlay_shows_safety() {
        let mut dirty = wt("topic", false);
        dirty.dirty = Some(true);
        let mut a = app(&[("main", true)]);
        a.worktrees.push(dirty);
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("confirm remove"));
        assert!(text.contains("data may be lost"));
        assert!(text.contains("[y/N]"));
    }

    #[test]
    fn confirm_remove_flags_no_upstream_as_unpushed() {
        // A clean, local-only branch (no upstream, not merged) is still flagged as
        // unpushed work, matching the remove guard (spec §10/§12).
        let mut clean = wt("topic", false);
        clean.dirty = Some(false);
        clean.has_untracked = Some(false);
        clean.ahead = None; // no upstream
        clean.merge_state = Some(MergeState::NoUpstreamLocal);
        let mut a = app(&[("main", true)]);
        a.worktrees.push(clean);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("no upstream"));
        assert!(text.contains("local-only"));
        assert!(!text.contains("data may be lost")); // not dirty
    }

    #[test]
    fn confirm_remove_merged_into_base_is_safe() {
        // A branch merged into its base: reassuring note, no unpushed alarm.
        let mut w = wt("feature/done", false);
        w.dirty = Some(false);
        w.has_untracked = Some(false);
        w.ahead = None;
        w.merge_state = Some(MergeState::Merged {
            into: Some("main".into()),
        });
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("merged into main"));
        assert!(text.contains("safe to delete"));
        // The alarming unpushed warning is suppressed (the branch header may
        // still note the absence of an upstream — that is not the warning).
        assert!(!text.contains("unpushed"));
    }

    #[test]
    fn confirm_remove_merged_via_pr_is_safe() {
        // A squash/rebase PR merge (ancestry can't prove it) → "merged via PR".
        let mut w = wt("feature/squashed", false);
        w.dirty = Some(false);
        w.has_untracked = Some(false);
        w.ahead = None;
        w.merge_state = Some(MergeState::Merged { into: None });
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("merged via PR"));
        assert!(!text.contains("unpushed"));
    }

    #[test]
    fn confirm_remove_upstream_gone_is_soft() {
        // Upstream configured but gone: softened "likely merged", not an alarm.
        let mut w = wt("feature/pushed", false);
        w.dirty = Some(false);
        w.has_untracked = Some(false);
        w.ahead = None;
        w.merge_state = Some(MergeState::UpstreamGone);
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("upstream branch deleted"));
        assert!(text.contains("likely merged"));
        assert!(!text.contains("unpushed work"));
    }

    #[test]
    fn confirm_remove_merged_but_dirty_still_warns() {
        // Mergedness is orthogonal to dirtiness: a merged-but-dirty tree shows
        // both the reassuring merge note AND the data-loss warning.
        let mut w = wt("feature/dirty-merged", false);
        w.dirty = Some(true);
        w.ahead = None;
        w.merge_state = Some(MergeState::Merged {
            into: Some("main".into()),
        });
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("safe to delete"));
        assert!(text.contains("data may be lost"));
    }

    #[test]
    fn confirm_remove_tracked_ahead_still_warns() {
        // A tracked branch with real unpushed commits keeps the unpushed warning.
        let mut w = wt("feature/ahead", false);
        w.dirty = Some(false);
        w.ahead = Some(2);
        w.upstream = Some("origin/feature/ahead".into());
        w.merge_state = Some(MergeState::Tracked);
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("2 unpushed commit(s)"));
    }

    #[test]
    fn confirm_remove_honors_remove_untracked_blocks() {
        // Untracked-only is NOT dirty by default (remove.untracked_blocks=false),
        // so the dialog must not claim data loss — even though show_untracked is on.
        let mut wt_un = wt("topic", false);
        wt_un.dirty = Some(false);
        wt_un.has_untracked = Some(true);
        wt_un.ahead = Some(0);
        let mut a = app(&[("main", true)]);
        assert!(a.show_untracked && !a.remove_untracked_blocks);
        a.worktrees.push(wt_un);
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        assert!(!render_to_text(&a, 100, 30).contains("data may be lost"));
    }

    #[test]
    fn confirm_remove_shows_glanceable_context() {
        // A clean, fully-merged branch: the dialog surfaces upstream, base, the
        // tip commit, ahead/behind, and the PR — and raises no data-loss alarm.
        use crate::model::{Commit, Pr, PrState};
        let mut w = wt("feature/login", false);
        w.dirty = Some(false);
        w.has_untracked = Some(false);
        w.ahead = Some(0);
        w.behind = Some(0);
        w.upstream = Some("origin/feature/login".into());
        w.base_ref = Some("main".into());
        w.commit = Some(Commit {
            hash: "abc1234".into(),
            subject: "Add login page".into(),
            author: "Alice".into(),
            timestamp: "2024-01-15T10:30:00Z".into(),
        });
        w.pr = Some(Pr {
            number: 42,
            state: PrState::Merged,
            title: "Add login page".into(),
        });
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("origin/feature/login"));
        assert!(text.contains("base:"));
        assert!(text.contains("abc1234"));
        assert!(text.contains("Add login page"));
        assert!(text.contains("#42 (merged)"));
        assert!(text.contains("↑0"));
        assert!(!text.contains("data may be lost"));
        assert!(text.contains("[y/N]"));
    }

    #[test]
    fn confirm_remove_layers_warnings_over_context() {
        // Dirty + ahead + an open PR: the neutral context lines AND both safety
        // warnings appear together (the ahead/unpushed overlap is intentional).
        use crate::model::{Commit, Pr, PrState};
        let mut w = wt("feature/x", false);
        w.dirty = Some(true);
        w.ahead = Some(2);
        w.behind = Some(0);
        w.upstream = Some("origin/feature/x".into());
        w.commit = Some(Commit {
            hash: "def5678".into(),
            subject: "WIP work".into(),
            author: "Bob".into(),
            timestamp: "2024-02-20T08:00:00Z".into(),
        });
        w.pr = Some(Pr {
            number: 7,
            state: PrState::Open,
            title: "Feature x".into(),
        });
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("def5678"));
        assert!(text.contains("#7 (open)"));
        assert!(text.contains("data may be lost"));
        assert!(text.contains("2 unpushed commit(s)"));
    }

    #[test]
    fn confirm_remove_missing_skips_status_lines() {
        // A missing worktree has no working tree to read: the dialog shows only
        // the deletion marker, never the commit context (even if a tip commit
        // happens to be recorded on the row).
        use crate::model::Commit;
        let mut w = wt("feature/gone", false);
        w.is_missing = true;
        w.base_ref = Some("main".into());
        w.commit = Some(Commit {
            hash: "ccc9999".into(),
            subject: "Gone branch tip".into(),
            author: "Dan".into(),
            timestamp: "2024-04-01T00:00:00Z".into(),
        });
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loaded(a.worktrees[a.worktrees.len() - 1].path.clone());
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("directory already deleted"));
        assert!(!text.contains("ccc9999"));
        assert!(!text.contains("Gone branch tip"));
    }

    #[test]
    fn confirm_remove_shows_spinner_until_loaded() {
        // Before the row's async fields load, the dialog shows a spinner and must
        // not leak commit content (matching the detail pane).
        use crate::model::Commit;
        let mut w = wt("feature/loading", false);
        w.commit = Some(Commit {
            hash: "aaa0000".into(),
            subject: "Secret subject".into(),
            author: "Carol".into(),
            timestamp: "2024-03-01T00:00:00Z".into(),
        });
        let mut a = app(&[("main", true)]);
        a.worktrees.push(w);
        a.mark_loading(); // pushed row is unloaded anyway, but be explicit
        a.mode = Mode::ConfirmRemove(a.worktrees.len() - 1);
        let text = render_to_text(&a, 100, 30);
        // The dialog rendered (its branch is shown) but the commit is withheld.
        assert!(text.contains("feature/loading"));
        assert!(!text.contains("Secret subject"));
        assert!(!text.contains("aaa0000"));
    }

    #[test]
    fn failed_dirty_read_shows_absent_marker_not_blank() {
        // A loaded, present worktree whose dirty state is unknown renders "–"
        // (not a blank that would read as clean).
        let mut unknown = wt("topic", false);
        unknown.dirty = None; // status read failed
        unknown.ahead = Some(0);
        unknown.behind = Some(0); // so ahead/behind is not the only "–"
        let mut a = app(&[("main", true)]);
        a.worktrees.push(unknown);
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains('–'));
    }

    #[test]
    fn status_bar_hints_are_per_mode() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::PrPicker(PrPickerState {
            loading: false,
            ..Default::default()
        });
        // The PR-picker overlay is empty here, so the bottom bar hint shows.
        assert!(render_to_text(&a, 100, 30).contains("checkout"));
    }

    #[test]
    fn detail_pane_shows_recent_commits_and_pr_url() {
        use crate::model::{Commit, Pr, PrState};
        let mut a = app(&[("main", true)]);
        let c = |hash: &str, subject: &str| Commit {
            hash: hash.into(),
            subject: subject.into(),
            author: "x".into(),
            timestamp: "2024-01-15T10:30:00Z".into(),
        };
        a.worktrees[0].recent_commits = vec![c("aaaaaaa", "newest"), c("bbbbbbb", "older")];
        a.worktrees[0].pr = Some(Pr {
            number: 42,
            state: PrState::Open,
            title: "Add login".into(),
        });
        a.worktrees[0].pr_url = Some("https://github.com/o/r/pull/42".into());
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("commits:"));
        assert!(text.contains("newest"));
        assert!(text.contains("older"));
        assert!(text.contains("pull/42"));
    }

    #[test]
    fn detail_pane_shows_merged_note() {
        // The detail pane mirrors the reassuring merge note (but not the
        // destructive-flow "unpushed work" warnings).
        let mut a = app(&[("main", true)]);
        a.worktrees[0].merge_state = Some(MergeState::Merged {
            into: Some("main".into()),
        });
        a.mark_loaded(a.worktrees[0].path.clone());
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("merged into main"));
    }

    #[test]
    fn detail_pane_omits_no_upstream_warning() {
        // The passive detail pane stays calm: the no-upstream-local warning is
        // confined to the destructive confirm flow.
        let mut a = app(&[("main", true)]);
        a.worktrees[0].merge_state = Some(MergeState::NoUpstreamLocal);
        a.mark_loaded(a.worktrees[0].path.clone());
        let text = render_to_text(&a, 100, 30);
        assert!(!text.contains("local-only"));
    }

    #[test]
    fn status_bar_shows_mode_and_filter() {
        let mut a = app(&[("main", true)]);
        a.filter = "feat".into();
        a.mode = Mode::Filter;
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("FILTER"));
        assert!(text.contains("/feat"));
    }

    #[test]
    fn list_markers_are_colored_and_gate_on_color_flag() {
        let mut a = app(&[("main", true)]);
        a.worktrees[0].ahead = Some(1);
        a.worktrees[0].behind = Some(2);
        a.worktrees[0].dirty = Some(true);
        // Colored: status/dirty/ahead/behind cells carry a foreground color.
        let buf = render_to_buffer(&a, 100, 20);
        assert_ne!(cell_fg(&buf, "*"), Color::Reset); // current marker (green)
        assert_ne!(cell_fg(&buf, "M"), Color::Reset); // dirty (yellow)
        assert_ne!(cell_fg(&buf, "↑"), Color::Reset); // ahead (green)
        assert_ne!(cell_fg(&buf, "↓"), Color::Reset); // behind (red)
        assert_ne!(cell_fg(&buf, "↑"), cell_fg(&buf, "↓")); // distinct hues
        // Monochrome: the same cells fall back to the default foreground.
        a.color = false;
        let mono = render_to_buffer(&a, 100, 20);
        assert_eq!(cell_fg(&mono, "*"), Color::Reset);
        assert_eq!(cell_fg(&mono, "M"), Color::Reset);
        assert_eq!(cell_fg(&mono, "↑"), Color::Reset);
    }

    #[test]
    fn custom_palette_recolors_cells() {
        let mut a = app(&[("main", true)]);
        // Overriding the "current" (green) slot recolors the current marker,
        // proving the resolved palette threads through rendering.
        a.palette.green = Color::Rgb(1, 2, 3);
        let buf = render_to_buffer(&a, 100, 20);
        assert_eq!(cell_fg(&buf, "*"), Color::Rgb(1, 2, 3));
    }

    #[test]
    fn pr_state_cell_is_colored() {
        use crate::model::{Pr, PrState};
        let mut a = app(&[("main", true)]);
        a.worktrees[0].pr = Some(Pr {
            number: 7,
            state: PrState::Open,
            title: "t".into(),
        });
        let buf = render_to_buffer(&a, 120, 20);
        // The PR cell '#' is colored by state when color is on.
        assert_ne!(cell_fg(&buf, "#"), Color::Reset);
    }

    #[test]
    fn focused_pane_border_differs_from_unfocused() {
        let mut a = app(&[("main", true)]);
        a.focus = Pane::List;
        let list_focused = render_to_buffer(&a, 100, 20);
        a.focus = Pane::Detail;
        let detail_focused = render_to_buffer(&a, 100, 20);
        // (0,0) is the list pane's top-left border corner.
        assert_ne!(list_focused[(0, 0)].fg, detail_focused[(0, 0)].fg);
    }

    #[test]
    fn list_title_shows_count_and_sort() {
        let a = app(&[("main", true), ("feature/x", false)]);
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("(2)"));
        assert!(text.contains("branch ↑"));
    }

    #[test]
    fn filtered_title_shows_visible_over_total() {
        let mut a = app(&[("alpha", true), ("beta", false)]);
        a.filter_push('a');
        a.filter_push('l');
        a.filter_push('p'); // matches only "alpha"
        assert_eq!(a.visible.len(), 1);
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("(1/2)"));
    }

    #[test]
    fn empty_filter_shows_no_matches_hint() {
        let mut a = app(&[("alpha", true)]);
        a.filter_push('z');
        a.filter_push('z');
        a.filter_push('z');
        assert!(a.visible.is_empty());
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("no matches for /zzz"));
    }

    #[test]
    fn detail_scrollbar_appears_when_content_overflows() {
        use crate::model::Commit;
        let mut a = app(&[("main", true)]);
        a.worktrees[0].recent_commits = (0..40)
            .map(|i| Commit {
                hash: format!("h{i:05}"),
                subject: "s".into(),
                author: "a".into(),
                timestamp: "2024-01-15T10:30:00Z".into(),
            })
            .collect();
        // A short pane forces overflow; the scrollbar thumb glyph appears.
        let text = render_to_text(&a, 100, 12);
        assert!(text.contains('█'));
    }

    #[test]
    fn status_message_colored_by_kind() {
        let mut a = app(&[("main", true)]);
        a.set_status("ZEBRA", StatusKind::Success);
        let ok = render_to_buffer(&a, 100, 20);
        a.set_status("ZEBRA", StatusKind::Error);
        let err = render_to_buffer(&a, 100, 20);
        a.set_status("ZEBRA", StatusKind::Info);
        let info = render_to_buffer(&a, 100, 20);
        // 'Z' only appears in the status message, so it locates the cell.
        assert_ne!(cell_fg(&ok, "Z"), Color::Reset); // success colored
        assert_ne!(cell_fg(&err, "Z"), Color::Reset); // error colored
        assert_eq!(cell_fg(&info, "Z"), Color::Reset); // info uncolored
        assert_ne!(cell_fg(&ok, "Z"), cell_fg(&err, "Z")); // success != error
    }

    #[test]
    fn per_row_job_shows_spinner_and_label() {
        // A background job attached to a row replaces its status marker with an
        // animated spinner and appends the job label inline — no blocking overlay
        // (issue #46 overhaul).
        use crate::tui::app::JobKey;
        let mut a = app(&[("main", true), ("feat/foo", false)]);
        a.begin_job(JobKey::Path("/r/feat/foo".into()), "Removing feat/foo");
        let text = render_to_text(&a, 120, 20);
        assert!(text.contains("Removing feat/foo"));
        // The first ASCII spinner frame animates the row's status marker.
        assert!(text.contains(Glyphs::new(false).spinner_frame(0)));
    }

    #[test]
    fn status_bar_summarizes_running_jobs() {
        use crate::tui::app::JobKey;
        let mut a = app(&[("main", true)]);
        a.begin_job(JobKey::New("feat/a".into()), "Creating feat/a");
        let at0 = render_to_text(&a, 120, 20);
        assert!(at0.contains("Creating feat/a"));
        // The shared spinner frame animates the status-bar summary too.
        a.spinner_frame = 1;
        let at1 = render_to_text(&a, 120, 20);
        assert!(at1.contains(Glyphs::new(false).spinner_frame(1)));
        assert_ne!(at0, at1);
    }

    #[test]
    fn exit_blocked_modal_renders_over_mode() {
        use crate::tui::app::{ExitBlockedState, ExitIntent, JobKey};
        let mut a = app(&[("main", true)]);
        a.begin_job(JobKey::New("feat".into()), "Creating feat");
        a.begin_job(JobKey::New("other".into()), "Creating other");
        a.mode = crate::tui::app::Mode::ExitBlocked(ExitBlockedState {
            intent: ExitIntent::Quit,
        });
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("finishing up"));
        assert!(text.contains("2 background jobs"));
        assert!(text.contains("abandon"));
        assert!(text.contains("keep working"));
    }
}
