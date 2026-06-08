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

use crate::model::{MergeState, PrState, SortKey, SortSpec, Worktree};
use crate::output::render::branch_display;
use crate::time::{now_unix, parse_iso8601, relative};
use crate::tui::app::{
    App, ComposeField, CreateState, CreateStep, Mode, Pane, PrComposeState, PrPickerState,
};
use crate::tui::glyphs::Glyphs;
use crate::tui::theme::Theme;

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
        Mode::Help => render_help(app, frame, area),
        Mode::Create(state) => render_create(app, state, frame, area),
        Mode::PrPicker(state) => render_pr_picker(app, state, frame, area),
        Mode::PrCompose(state) => render_pr_compose(app, state, frame, area),
        Mode::ConfirmRemove(index) => render_confirm(app, *index, frame, area),
        _ => {}
    }
}

/// Renders the worktree list pane.
fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::new(app.color);
    let focused = app.focus == Pane::List;
    let block = Block::bordered()
        .title(list_title(app, &theme, focused))
        .border_style(theme.border(focused));

    // Empty / no-matches state: a helpful hint rather than a blank pane.
    if app.visible.is_empty() {
        let msg = if app.filter.is_empty() {
            "no worktrees".to_string()
        } else {
            format!("no matches for /{}", app.filter)
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, theme.hint_label()))).block(block),
            area,
        );
        return;
    }

    let glyphs = Glyphs::new(app.nerd_fonts);
    let now = now_unix();
    let items: Vec<ListItem> = app
        .visible
        .iter()
        .map(|&i| {
            let worktree = &app.worktrees[i];
            let loaded = app.is_loaded(worktree);
            let item = ListItem::new(list_row(
                worktree,
                &glyphs,
                &theme,
                loaded,
                app.show_untracked,
                now,
            ));
            if worktree.is_missing {
                item.style(Style::default().add_modifier(Modifier::DIM))
            } else {
                item
            }
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(theme.selection())
        .highlight_symbol(theme.selection_symbol())
        .highlight_spacing(HighlightSpacing::Always);
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The list-pane title: name, visible/total counts, and the active sort.
fn list_title(app: &App, theme: &Theme, focused: bool) -> Line<'static> {
    let count = if app.visible.len() == app.worktrees.len() {
        format!(" ({})", app.worktrees.len())
    } else {
        format!(" ({}/{})", app.visible.len(), app.worktrees.len())
    };
    Line::from(vec![
        Span::styled("worktrees", theme.title(focused)),
        Span::styled(count, theme.label()),
        Span::styled(format!(" · {}", sort_label(&app.sort)), theme.label()),
    ])
}

/// A short label for the active sort field and direction (e.g. `branch ↑`).
fn sort_label(sort: &SortSpec) -> String {
    let key = match sort.key {
        SortKey::Branch => "branch",
        SortKey::Dirty => "dirty",
        SortKey::Ahead => "ahead",
        SortKey::Behind => "behind",
        SortKey::Activity => "activity",
        SortKey::Path => "path",
    };
    let arrow = if sort.descending { "↓" } else { "↑" };
    format!("{key} {arrow}")
}

/// Builds one list-pane row as colored spans.
fn list_row(
    worktree: &Worktree,
    glyphs: &Glyphs,
    theme: &Theme,
    loaded: bool,
    show_untracked: bool,
    now: i64,
) -> Line<'static> {
    let (status, status_style) = if worktree.is_current {
        (glyphs.current(), theme.current())
    } else if worktree.is_missing {
        (glyphs.missing(), theme.missing())
    } else if worktree.is_detached {
        (glyphs.detached(), theme.detached())
    } else {
        (" ", Style::default())
    };
    let (dirty, dirty_style) = if !loaded {
        (glyphs.spinner().to_string(), theme.spinner())
    } else if worktree.dirty == Some(true) {
        (glyphs.dirty().to_string(), theme.dirty())
    } else if show_untracked && worktree.has_untracked == Some(true) {
        (glyphs.untracked().to_string(), theme.untracked())
    } else if worktree.dirty.is_none() && !worktree.is_missing {
        // Loaded but the status read failed: the absent marker, not a blank that
        // would read as "clean" (spec §10).
        (glyphs.absent().to_string(), theme.absent())
    } else {
        (" ".to_string(), Style::default())
    };

    let mut spans = vec![
        Span::styled(status.to_string(), status_style),
        Span::styled(dirty, dirty_style),
        Span::raw(" "),
        Span::styled(
            branch_display(worktree),
            theme.branch(worktree.is_current, worktree.is_detached),
        ),
        Span::raw("  "),
    ];
    spans.extend(ahead_behind_spans(worktree, theme, loaded, glyphs));
    spans.push(Span::raw("  "));
    spans.extend(commit_spans(worktree, theme, loaded, glyphs, now));
    spans.push(Span::raw("  "));
    spans.extend(pr_spans(worktree, theme, loaded, glyphs));
    Line::from(spans)
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

/// Renders the detail pane for the selected worktree (spec §10).
fn render_detail(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::new(app.color);
    let focused = app.focus == Pane::Detail;
    let block = Block::bordered()
        .title(Span::styled("detail", theme.title(focused)))
        .border_style(theme.border(focused));
    let Some(worktree) = app.selected_worktree() else {
        frame.render_widget(
            Paragraph::new(Span::styled("no worktree selected", theme.hint_label())).block(block),
            area,
        );
        return;
    };
    let now = now_unix();
    let glyphs = Glyphs::new(app.nerd_fonts);
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("path:   ", theme.label()),
        Span::raw(worktree.path.display().to_string()),
    ]));
    let branch_span = Span::styled(
        branch_display(worktree),
        theme.branch(worktree.is_current, worktree.is_detached),
    );
    match &worktree.upstream {
        Some(up) => lines.push(Line::from(vec![
            Span::styled("branch: ", theme.label()),
            branch_span,
            Span::raw(" → "),
            Span::styled(up.clone(), theme.accent()),
        ])),
        None => lines.push(Line::from(vec![
            Span::styled("branch: ", theme.label()),
            branch_span,
            Span::styled(" (no upstream)", theme.label()),
        ])),
    }
    if let Some(base) = &worktree.base_ref {
        lines.push(Line::from(vec![
            Span::styled("base:   ", theme.label()),
            Span::raw(base.clone()),
        ]));
    }
    if app.is_loaded(worktree) {
        let mut status_spans = vec![Span::styled("status: ", theme.label())];
        status_spans.extend(ahead_behind_spans(worktree, &theme, true, &glyphs));
        status_spans.push(Span::raw("  "));
        status_spans.push(dirty_label_span(worktree, &theme));
        lines.push(Line::from(status_spans));
        // Reassuring/informative merge note only (no destructive-flow warnings).
        if let Some(line) = merge_state_note(worktree, &theme, false) {
            lines.push(line);
        }
        detail_commits(&mut lines, worktree, &theme, now);
        if let Some(pr) = &worktree.pr {
            lines.push(Line::from(vec![
                Span::styled("pr:     ", theme.label()),
                Span::styled(
                    format!("#{} ({}) ", pr.number, pr.state.as_str()),
                    theme.pr_state(pr.state),
                ),
                Span::raw(pr.title.clone()),
            ]));
            if let Some(url) = worktree.pr_url.as_deref().filter(|u| !u.is_empty()) {
                lines.push(Line::from(vec![
                    Span::raw("        "),
                    Span::styled(url.to_string(), theme.url()),
                ]));
            }
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("status: ", theme.label()),
            Span::styled("…", theme.spinner()),
        ]));
    }

    let content_height = lines.len() as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        area,
    );

    // A scrollbar when the content overflows the viewport — a "more below" hint.
    let viewport = area.height.saturating_sub(2);
    if content_height > viewport {
        let mut sb_state = ScrollbarState::new(content_height as usize)
            .viewport_content_length(viewport as usize)
            .position(app.detail_scroll as usize);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        frame.render_stateful_widget(scrollbar, area.inner(Margin::new(0, 1)), &mut sb_state);
    }
}

/// Appends the "Last 5 commits" lines (short hash, subject, relative time) to
/// the detail pane (spec §10), falling back to the single tip commit.
fn detail_commits(lines: &mut Vec<Line<'static>>, worktree: &Worktree, theme: &Theme, now: i64) {
    let rel = |ts: &str| {
        parse_iso8601(ts)
            .map(|u| relative(now, u))
            .unwrap_or_default()
    };
    if !worktree.recent_commits.is_empty() {
        lines.push(Line::from(Span::styled("commits:", theme.label())));
        for c in &worktree.recent_commits {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(c.hash.clone(), theme.commit_hash()),
                Span::raw(" "),
                Span::raw(c.subject.clone()),
                Span::raw(" "),
                Span::styled(format!("({})", rel(&c.timestamp)), theme.time()),
            ]));
        }
    } else if let Some(c) = &worktree.commit {
        lines.push(Line::from(vec![
            Span::styled("commit: ", theme.label()),
            Span::styled(c.hash.clone(), theme.commit_hash()),
            Span::raw(" "),
            Span::raw(c.subject.clone()),
            Span::raw(" "),
            Span::styled(format!("({})", rel(&c.timestamp)), theme.time()),
        ]));
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
    let theme = Theme::new(app.color);
    let mut spans = vec![Span::styled(
        format!(" {} ", mode_label(&app.mode)),
        theme.mode_chip(&app.mode),
    )];
    if !app.filter.is_empty() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(format!("/{}", app.filter), theme.accent()));
    }
    spans.push(Span::raw("  "));
    if let Some(message) = &app.status_message {
        spans.push(Span::styled(message.clone(), theme.status(app.status_kind)));
    } else {
        for (i, (key, label)) in mode_hints(&app.mode).iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(*key, theme.hint_key()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(*label, theme.hint_label()));
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
        Mode::ConfirmRemove(_) => "REMOVE",
        Mode::Help => "HELP",
    }
}

/// The right-side key hints for the current mode (spec §10 bottom bar), as
/// `(key, description)` pairs so the keys can be colored.
fn mode_hints(mode: &Mode) -> &'static [(&'static str, &'static str)] {
    match mode {
        Mode::List => &[
            ("Enter", "switch"),
            ("n", "new"),
            ("d", "remove"),
            ("p", "pr"),
            ("/", "filter"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Mode::Filter => &[("type", "to filter"), ("Enter", "apply"), ("Esc", "clear")],
        Mode::Create(_) => &[("Enter", "next/submit"), ("Esc", "cancel")],
        Mode::PrPicker(_) => &[("↑/↓", "select"), ("Enter", "checkout"), ("Esc", "close")],
        Mode::PrCompose(_) => &[
            ("Ctrl-S", "submit"),
            ("Ctrl-D", "draft"),
            ("Tab", "field"),
            ("Esc", "cancel"),
        ],
        Mode::ConfirmRemove(_) => &[("y", "remove"), ("any other key", "cancels")],
        Mode::Help => &[("any key", "closes help")],
    }
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
fn render_help(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::new(app.color);
    let rect = centered(area, 56, 22);
    frame.render_widget(Clear, rect);
    let bindings = [
        ("↑/k  ↓/j", "navigate"),
        ("g/G", "top / bottom"),
        ("Enter", "switch (cd) and exit"),
        ("/", "filter"),
        ("Esc", "clear / back"),
        ("n", "new worktree"),
        ("d", "remove worktree"),
        ("p", "PR picker"),
        ("o", "open in editor"),
        ("r", "refresh"),
        ("s / S", "sort cycle / reverse"),
        ("Tab", "switch pane"),
        ("\\  + / -", "sidebar toggle / resize"),
        ("?", "this help"),
        ("q", "quit"),
    ];
    let lines: Vec<Line> = bindings
        .iter()
        .map(|(keys, desc)| {
            Line::from(vec![
                Span::styled(format!("{keys:<14}"), theme.hint_key()),
                Span::styled((*desc).to_string(), theme.hint_label()),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("help", theme.title(true)))),
        rect,
    );
}

/// Renders the create-worktree prompt.
fn render_create(app: &App, state: &CreateState, frame: &mut Frame, area: Rect) {
    let theme = Theme::new(app.color);
    let rect = centered(area, 60, 8);
    frame.render_widget(Clear, rect);
    let field = |active: bool, label: &'static str, value: &str| {
        let (marker, style) = if active {
            (label, theme.accent())
        } else {
            (label, theme.label())
        };
        Line::from(vec![
            Span::styled(marker.to_string(), style),
            Span::raw(format!(" {value}")),
        ])
    };
    let mut lines = vec![
        field(
            state.step == CreateStep::Branch,
            // Leading marker shows which field is active.
            if state.step == CreateStep::Branch {
                "> branch:"
            } else {
                "  branch:"
            },
            &state.branch,
        ),
        field(
            state.step == CreateStep::Base,
            if state.step == CreateStep::Base {
                "> base:  "
            } else {
                "  base:  "
            },
            &state.base,
        ),
    ];
    if let Some(err) = &state.error {
        lines.push(Line::from(Span::styled(format!("! {err}"), theme.error())));
    }
    lines.push(Line::from(Span::styled(
        "Enter: next/submit   Esc: cancel",
        theme.hint_label(),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("new worktree", theme.title(true)))),
        rect,
    );
}

/// Renders the PR compose form (`wt pr open`): a title + multi-line body with a
/// header showing branch → trunk, the create/update action, and the draft state.
fn render_pr_compose(app: &App, state: &PrComposeState, frame: &mut Frame, area: Rect) {
    let theme = Theme::new(app.color);
    let rect = centered(area, 76, 20);
    frame.render_widget(Clear, rect);

    let title_active = state.field == ComposeField::Title;
    let body_active = state.field == ComposeField::Body;
    let draft_mark = if state.draft { "[x]" } else { "[ ]" };

    let mut lines = vec![
        // Header: branch → trunk   [action]   draft [x]/[ ]
        Line::from(vec![
            Span::styled(format!("{} ", state.branch), theme.accent()),
            Span::raw("→ "),
            Span::styled(state.trunk.clone(), theme.label()),
            Span::raw("   "),
            Span::styled(format!("[{}]", state.action_label), theme.label()),
            Span::raw("   "),
            Span::styled(format!("draft {draft_mark}"), theme.label()),
        ]),
        // Agent settings used for `Ctrl-A` auto-fill (model + effort).
        Line::from(vec![
            Span::styled("model: ", theme.label()),
            Span::styled(state.model.label().to_string(), theme.accent()),
            Span::raw("   "),
            Span::styled("effort: ", theme.label()),
            Span::styled(state.effort.label().to_string(), theme.accent()),
        ]),
        Line::raw(""),
        // Title field.
        Line::from(vec![
            Span::styled(
                if title_active {
                    "> title: "
                } else {
                    "  title: "
                },
                if title_active {
                    theme.accent()
                } else {
                    theme.label()
                },
            ),
            Span::raw(state.title.clone()),
        ]),
        Line::raw(""),
        // Body field label.
        Line::from(Span::styled(
            if body_active { "> body:" } else { "  body:" },
            if body_active {
                theme.accent()
            } else {
                theme.label()
            },
        )),
    ];
    // Body content (multi-line); show at least one (blank) line.
    if state.body.is_empty() {
        lines.push(Line::raw(""));
    } else {
        for line in state.body.split('\n') {
            lines.push(Line::raw(format!("  {line}")));
        }
    }
    if let Some(err) = &state.error {
        lines.push(Line::from(Span::styled(format!("! {err}"), theme.error())));
    }
    lines.push(Line::raw(""));
    if state.submitting {
        lines.push(Line::from(Span::styled("working…", theme.hint_label())));
    } else {
        // Two hint rows: AI auto-fill controls, then the edit/submit controls.
        lines.push(Line::from(Span::styled(
            "Ctrl-A: AI fill   Ctrl-M: model   Ctrl-E: effort",
            theme.hint_label(),
        )));
        lines.push(Line::from(Span::styled(
            "Ctrl-S: submit   Ctrl-D: draft   Tab: field   Enter: newline   Esc: cancel",
            theme.hint_label(),
        )));
    }

    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("open pull request", theme.title(true))))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

/// Renders the PR picker overlay.
fn render_pr_picker(app: &App, state: &PrPickerState, frame: &mut Frame, area: Rect) {
    let theme = Theme::new(app.color);
    let rect = centered(area, 70, 20);
    frame.render_widget(Clear, rect);
    let block = Block::bordered().title(Span::styled("open pull requests", theme.title(true)));
    if let Some(err) = &state.error {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(err.clone(), theme.error())),
                Line::from(Span::styled(
                    "(run `gh auth login`)   Esc: close",
                    theme.hint_label(),
                )),
            ])
            .block(block),
            rect,
        );
        return;
    }
    if state.loading {
        frame.render_widget(
            Paragraph::new(Span::styled("loading…", theme.spinner())).block(block),
            rect,
        );
        return;
    }
    let now = now_unix();
    let items: Vec<ListItem> = state
        .prs
        .iter()
        .map(|pr| {
            let state_style = PrState::parse(&pr.state)
                .map(|s| theme.pr_state(s))
                .unwrap_or_default();
            let age = parse_iso8601(&pr.created_at)
                .map(|u| relative(now, u))
                .unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::styled(format!("#{}", pr.number), theme.commit_hash()),
                Span::raw("  "),
                Span::raw(pr.title.clone()),
                Span::raw("  "),
                Span::styled(format!("({})", pr.author), theme.hint_label()),
                Span::raw("  "),
                Span::styled(pr.state.clone(), state_style),
                Span::raw("  "),
                Span::styled(age, theme.time()),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(theme.selection())
        .highlight_symbol(theme.selection_symbol())
        .highlight_spacing(HighlightSpacing::Always);
    let mut list_state = ListState::default().with_selected(Some(state.selected));
    frame.render_stateful_widget(list, rect, &mut list_state);
}

/// Renders the confirm-remove dialog.
///
/// Beyond the branch, path, and safety warnings, this surfaces the same
/// glanceable context as the detail pane — upstream/base, ahead/behind and
/// working-tree state, the tip commit with its age, and any recorded PR — so a
/// deletion decision can be made without leaving the dialog (issue #7). All of
/// this is already on the [`crate::model::Worktree`] row; nothing new is read
/// from git. The dialog grows to fit its content.
fn render_confirm(app: &App, index: usize, frame: &mut Frame, area: Rect) {
    let theme = Theme::new(app.color);
    let Some(worktree) = app.worktrees.get(index) else {
        return;
    };
    let now = now_unix();
    let glyphs = Glyphs::new(app.nerd_fonts);
    let loaded = app.is_loaded(worktree);

    // Branch + upstream (or "(no upstream)"), mirroring the detail pane.
    let branch_span = Span::styled(
        branch_display(worktree),
        theme.branch(worktree.is_current, worktree.is_detached),
    );
    let mut lines = match &worktree.upstream {
        Some(up) => vec![Line::from(vec![
            Span::styled("branch: ", theme.label()),
            branch_span,
            Span::raw(" → "),
            Span::styled(up.clone(), theme.accent()),
        ])],
        None => vec![Line::from(vec![
            Span::styled("branch: ", theme.label()),
            branch_span,
            Span::styled(" (no upstream)", theme.label()),
        ])],
    };
    lines.push(Line::from(vec![
        Span::styled("path:   ", theme.label()),
        Span::raw(worktree.path.display().to_string()),
    ]));
    if let Some(base) = &worktree.base_ref {
        lines.push(Line::from(vec![
            Span::styled("base:   ", theme.label()),
            Span::raw(base.clone()),
        ]));
    }

    if worktree.is_missing {
        // No working tree to read — the safety marker is all the context there is.
        lines.push(Line::from(Span::styled(
            "(directory already deleted)",
            theme.hint_label(),
        )));
    } else {
        // Glanceable status / commit / PR, gated on the async load like the
        // detail pane: a spinner until the row's fields are available.
        if loaded {
            let mut status_spans = vec![Span::styled("status: ", theme.label())];
            status_spans.extend(ahead_behind_spans(worktree, &theme, true, &glyphs));
            status_spans.push(Span::raw("  "));
            status_spans.push(dirty_label_span(worktree, &theme));
            lines.push(Line::from(status_spans));
            if let Some(c) = &worktree.commit {
                let rel = parse_iso8601(&c.timestamp)
                    .map(|u| relative(now, u))
                    .unwrap_or_default();
                lines.push(Line::from(vec![
                    Span::styled("commit: ", theme.label()),
                    Span::styled(c.hash.clone(), theme.commit_hash()),
                    Span::raw(" "),
                    Span::raw(c.subject.clone()),
                    Span::raw(" "),
                    Span::styled(format!("({rel})"), theme.time()),
                ]));
            }
            if let Some(pr) = &worktree.pr {
                lines.push(Line::from(vec![
                    Span::styled("pr:     ", theme.label()),
                    Span::styled(
                        format!("#{} ({}) ", pr.number, pr.state.as_str()),
                        theme.pr_state(pr.state),
                    ),
                    Span::raw(pr.title.clone()),
                ]));
                if let Some(url) = worktree.pr_url.as_deref().filter(|u| !u.is_empty()) {
                    lines.push(Line::from(vec![
                        Span::raw("        "),
                        Span::styled(url.to_string(), theme.url()),
                    ]));
                }
            }
        } else {
            lines.push(Line::from(vec![
                Span::styled("status: ", theme.label()),
                Span::styled("…", theme.spinner()),
            ]));
        }

        // Safety warnings, layered on top of the neutral context above for
        // emphasis. The dirty warning mirrors the remove guard; dirtiness is
        // orthogonal to mergedness, so a merged-but-dirty tree still warns.
        let guard = crate::worktree_service::guard_status(worktree, app.remove_untracked_blocks);
        if guard.dirty {
            lines.push(Line::from(Span::styled(
                "(has uncommitted changes — data may be lost)",
                theme.error(),
            )));
        }
        // The unpushed message is driven by the offline merge state so a branch
        // that was simply merged (into its base/default, or via a PR) is not
        // flagged as alarming "unpushed work" (spec §10). A confirmed merge
        // suppresses the unpushed warning entirely.
        if let Some(line) = merge_state_note(worktree, &theme, true) {
            lines.push(line);
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("Remove this worktree? ["),
        Span::styled("y", theme.warning()),
        Span::raw("/N]"),
    ]));

    // Size to the content (plus the top/bottom border); `centered` clamps to the
    // available area, and `Wrap` keeps long paths/subjects/titles from overflowing.
    let height = lines.len() as u16 + 2;
    let rect = centered(area, 72, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("confirm remove", theme.error())))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::testutil::{app, wt};
    use crate::tui::app::{
        ComposeField, CreateState, PrComposeState, PrItem, PrPickerState, StatusKind,
    };
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
}
