//! TUI rendering (spec §10): the list pane, detail pane, status bar, and modal
//! overlays. Rendering is a pure function of [`App`] state into a ratatui
//! [`Frame`], so it is testable with a `TestBackend`.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::output::render::{ahead_behind_cell, branch_display};
use crate::time::{now_unix, parse_iso8601, relative};
use crate::tui::app::{App, CreateState, CreateStep, Mode, PrPickerState};
use crate::tui::glyphs::Glyphs;

/// Renders the whole TUI for the current state.
pub fn render(app: &App, frame: &mut Frame) {
    let area = frame.area();
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    let (main, status) = (rows[0], rows[1]);

    if app.show_sidebar && app.detail_visible() {
        let cols = Layout::horizontal([Constraint::Length(app.sidebar_width), Constraint::Min(20)])
            .split(main);
        render_list(app, frame, cols[0]);
        render_detail(app, frame, cols[1]);
    } else if app.show_sidebar {
        render_list(app, frame, main);
    } else {
        render_detail(app, frame, main);
    }
    render_status_bar(app, frame, status);

    match &app.mode {
        Mode::Help => render_help(frame, area),
        Mode::Create(state) => render_create(state, frame, area),
        Mode::PrPicker(state) => render_pr_picker(state, frame, area),
        Mode::ConfirmRemove(index) => render_confirm(app, *index, frame, area),
        _ => {}
    }
}

/// Renders the worktree list pane.
fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let glyphs = Glyphs::new(app.nerd_fonts);
    let now = now_unix();
    let items: Vec<ListItem> = app
        .visible
        .iter()
        .map(|&i| {
            let worktree = &app.worktrees[i];
            let loaded = app.is_loaded(worktree);
            let item = ListItem::new(list_row(worktree, &glyphs, loaded, app.show_untracked, now));
            if worktree.is_missing {
                item.style(Style::default().add_modifier(Modifier::DIM))
            } else {
                item
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::bordered().title("worktrees"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Builds one list-pane row.
fn list_row(
    worktree: &crate::model::Worktree,
    glyphs: &Glyphs,
    loaded: bool,
    show_untracked: bool,
    now: i64,
) -> Line<'static> {
    let status = if worktree.is_current {
        glyphs.current()
    } else if worktree.is_missing {
        glyphs.missing()
    } else if worktree.is_detached {
        glyphs.detached()
    } else {
        " "
    };
    let dirty = if !loaded {
        glyphs.spinner().to_string()
    } else if worktree.dirty == Some(true) {
        glyphs.dirty().to_string()
    } else if show_untracked && worktree.has_untracked == Some(true) {
        glyphs.untracked().to_string()
    } else {
        " ".to_string()
    };
    let ahead_behind = if loaded {
        ahead_behind_cell(worktree)
    } else {
        glyphs.spinner().to_string()
    };
    let commit = match (&worktree.commit, loaded) {
        (_, false) => glyphs.spinner().to_string(),
        (Some(c), true) => {
            let rel = parse_iso8601(&c.timestamp)
                .map(|u| relative(now, u))
                .unwrap_or_default();
            format!("{} {} ({rel})", c.hash, c.subject)
        }
        (None, true) => String::new(),
    };
    let pr = match (&worktree.pr, loaded) {
        (_, false) => glyphs.spinner().to_string(),
        (Some(pr), true) => format!("#{} ({})", pr.number, pr.state.as_str()),
        (None, true) => String::new(),
    };
    Line::from(format!(
        "{status}{dirty} {}  {ahead_behind}  {commit}  {pr}",
        branch_display(worktree)
    ))
}

/// Renders the detail pane for the selected worktree.
fn render_detail(app: &App, frame: &mut Frame, area: Rect) {
    let block = Block::bordered().title("detail");
    let Some(worktree) = app.selected_worktree() else {
        frame.render_widget(Paragraph::new("no worktree selected").block(block), area);
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(format!("path:   {}", worktree.path.display())));
    let branch = branch_display(worktree);
    match &worktree.upstream {
        Some(up) => lines.push(Line::from(format!("branch: {branch} → {up}"))),
        None => lines.push(Line::from(format!("branch: {branch} (no upstream)"))),
    }
    if let Some(base) = &worktree.base_ref {
        lines.push(Line::from(format!("base:   {base}")));
    }
    if app.is_loaded(worktree) {
        lines.push(Line::from(format!(
            "status: {}  {}",
            ahead_behind_cell(worktree),
            dirty_label(worktree)
        )));
        if let Some(c) = &worktree.commit {
            lines.push(Line::from(format!("commit: {} {}", c.hash, c.subject)));
        }
        if let Some(pr) = &worktree.pr {
            lines.push(Line::from(format!(
                "pr:     #{} ({}) {}",
                pr.number,
                pr.state.as_str(),
                pr.title
            )));
        }
    } else {
        lines.push(Line::from("status: …"));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// A short dirty label for the detail pane.
fn dirty_label(worktree: &crate::model::Worktree) -> &'static str {
    match (worktree.dirty, worktree.has_untracked) {
        (Some(true), _) => "modified",
        (_, Some(true)) => "untracked",
        (Some(false), _) => "clean",
        _ => "",
    }
}

/// Renders the bottom status/help bar.
fn render_status_bar(app: &App, frame: &mut Frame, area: Rect) {
    let mode = match &app.mode {
        Mode::List => "LIST",
        Mode::Filter => "FILTER",
        Mode::Create(_) => "CREATE",
        Mode::PrPicker(_) => "PR",
        Mode::ConfirmRemove(_) => "REMOVE",
        Mode::Help => "HELP",
    };
    let left = if app.filter.is_empty() {
        format!(" {mode} ")
    } else {
        format!(" {mode}  /{} ", app.filter)
    };
    let hint = app
        .status_message
        .clone()
        .unwrap_or_else(|| "Enter switch  n new  d remove  p pr  / filter  ? help  q quit".into());
    let line = Line::from(vec![
        Span::styled(left, Style::default().add_modifier(Modifier::REVERSED)),
        Span::raw(format!(" {hint}")),
    ]);
    frame.render_widget(Paragraph::new(line), area);
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

/// Renders the help overlay (the full key-binding reference).
fn render_help(frame: &mut Frame, area: Rect) {
    let rect = centered(area, 56, 22);
    frame.render_widget(Clear, rect);
    let bindings = [
        "↑/k  ↓/j        navigate",
        "g/G            top / bottom",
        "Enter          switch (cd) and exit",
        "/              filter      Esc  clear / back",
        "n              new worktree",
        "d              remove worktree",
        "p              PR picker",
        "o              open in editor",
        "r              refresh",
        "s / S          sort cycle / reverse",
        "Tab            switch pane",
        "\\  + / -       sidebar toggle / resize",
        "?              this help     q  quit",
    ];
    let lines: Vec<Line> = bindings.iter().map(|b| Line::from(*b)).collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("help")),
        rect,
    );
}

/// Renders the create-worktree prompt.
fn render_create(state: &CreateState, frame: &mut Frame, area: Rect) {
    let rect = centered(area, 60, 8);
    frame.render_widget(Clear, rect);
    let branch_label = if state.step == CreateStep::Branch {
        "> branch:"
    } else {
        "  branch:"
    };
    let base_label = if state.step == CreateStep::Base {
        "> base:  "
    } else {
        "  base:  "
    };
    let mut lines = vec![
        Line::from(format!("{branch_label} {}", state.branch)),
        Line::from(format!("{base_label} {}", state.base)),
    ];
    if let Some(err) = &state.error {
        lines.push(Line::from(Span::styled(
            format!("! {err}"),
            Style::default().red(),
        )));
    }
    lines.push(Line::from("Enter: next/submit   Esc: cancel"));
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("new worktree")),
        rect,
    );
}

/// Renders the PR picker overlay.
fn render_pr_picker(state: &PrPickerState, frame: &mut Frame, area: Rect) {
    let rect = centered(area, 70, 20);
    frame.render_widget(Clear, rect);
    let block = Block::bordered().title("open pull requests");
    if let Some(err) = &state.error {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(err.clone(), Style::default().red())),
                Line::from("(run `gh auth login`)   Esc: close"),
            ])
            .block(block),
            rect,
        );
        return;
    }
    if state.loading {
        frame.render_widget(Paragraph::new("loading…").block(block), rect);
        return;
    }
    let items: Vec<ListItem> = state
        .prs
        .iter()
        .map(|pr| {
            ListItem::new(Line::from(format!(
                "#{}  {}  ({})  {}",
                pr.number, pr.title, pr.author, pr.state
            )))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    let mut list_state = ListState::default().with_selected(Some(state.selected));
    frame.render_stateful_widget(list, rect, &mut list_state);
}

/// Renders the confirm-remove dialog.
fn render_confirm(app: &App, index: usize, frame: &mut Frame, area: Rect) {
    let rect = centered(area, 60, 9);
    frame.render_widget(Clear, rect);
    let Some(worktree) = app.worktrees.get(index) else {
        return;
    };
    let mut lines = vec![
        Line::from(format!("branch: {}", branch_display(worktree))),
        Line::from(format!("path:   {}", worktree.path.display())),
    ];
    if worktree.is_missing {
        lines.push(Line::from("(directory already deleted)"));
    } else {
        let guard = crate::worktree_service::guard_status(worktree, app.show_untracked);
        if guard.dirty {
            lines.push(Line::from(Span::styled(
                "(has uncommitted changes — data may be lost)",
                Style::default().red(),
            )));
        }
        let ahead = worktree.ahead.unwrap_or(0);
        if ahead > 0 {
            lines.push(Line::from(format!("({ahead} unpushed commit(s))")));
        }
    }
    lines.push(Line::from("Remove this worktree? [y/N]"));
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("confirm remove")),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::testutil::{app, wt};
    use crate::tui::app::{CreateState, PrItem, PrPickerState};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Renders the app to a TestBackend and returns the buffer as text.
    fn render_to_text(app: &App, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render(app, f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer_text(&buffer)
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
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("help"));
        assert!(text.contains("navigate"));
        assert!(text.contains("quit"));
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
    fn pr_picker_states() {
        let mut a = app(&[("main", true)]);
        a.mode = Mode::PrPicker(PrPickerState {
            loading: true,
            ..Default::default()
        });
        assert!(render_to_text(&a, 100, 30).contains("loading"));

        a.mode = Mode::PrPicker(PrPickerState {
            loading: false,
            prs: vec![PrItem {
                number: 42,
                title: "Add login".into(),
                author: "alice".into(),
                state: "open".into(),
            }],
            ..Default::default()
        });
        let text = render_to_text(&a, 100, 30);
        assert!(text.contains("#42"));
        assert!(text.contains("Add login"));

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
    fn status_bar_shows_mode_and_filter() {
        let mut a = app(&[("main", true)]);
        a.filter = "feat".into();
        a.mode = Mode::Filter;
        let text = render_to_text(&a, 100, 20);
        assert!(text.contains("FILTER"));
        assert!(text.contains("/feat"));
    }
}
