//! The detail pane (spec §10): the expanded view of the selected worktree —
//! branch/PR/upstream summary and the recent-commit list. Row-level span helpers
//! live in the parent [`super`] module.

use super::*;

/// Renders the detail pane for the selected worktree (spec §10).
pub(super) fn render_detail(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
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
    // A worktree-less branch row has its own (path-less) detail layout (issue #47).
    if !worktree.has_worktree {
        render_branch_detail(app, worktree, &theme, block, frame, area);
        return;
    }
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

/// Renders the detail pane for a worktree-less branch row (issue #47): the
/// worktree detail layout, but path-less and without working-tree status, with
/// the ahead/behind labeled relative to the branch's base and a hint that Enter
/// creates a worktree.
fn render_branch_detail(
    app: &App,
    worktree: &Worktree,
    theme: &Theme,
    block: Block<'static>,
    frame: &mut Frame,
    area: Rect,
) {
    let now = now_unix();
    let glyphs = Glyphs::new(app.nerd_fonts);
    let loaded = app.is_loaded(worktree);
    let branch_span = Span::styled(branch_display(worktree), theme.branch(false, false));
    let mut lines: Vec<Line> = match &worktree.upstream {
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
    lines.push(Line::from(Span::styled(
        "(no worktree — press Enter to create one)",
        theme.hint_label(),
    )));
    if let Some(base) = &worktree.base_ref {
        lines.push(Line::from(vec![
            Span::styled("base:   ", theme.label()),
            Span::raw(base.clone()),
        ]));
    }
    // Ahead/behind relative to the base — the point of a branch row.
    if loaded {
        let mut spans = vec![Span::styled("vs base: ", theme.label())];
        spans.extend(ahead_behind_spans(worktree, theme, true, &glyphs));
        lines.push(Line::from(spans));
    } else {
        lines.push(Line::from(vec![
            Span::styled("vs base: ", theme.label()),
            Span::styled("…", theme.spinner()),
        ]));
    }
    detail_commits(&mut lines, worktree, theme, now);
    if let Some(pr) = &worktree.pr {
        lines.push(Line::from(vec![
            Span::styled("pr:     ", theme.label()),
            Span::styled(
                format!("#{} ({}) ", pr.number, pr.state.as_str()),
                theme.pr_state(pr.state),
            ),
            Span::raw(pr.title.clone()),
        ]));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        area,
    );
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
