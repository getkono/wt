//! The worktree list pane (spec §10): the left-hand list of worktree rows with
//! the title bar, sort indicator, and per-row cells. The shared ahead/behind and
//! commit/PR span helpers live in the parent [`super`] module.

use super::*;

/// Renders the worktree list pane.
pub(super) fn render_list(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
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
            // Missing worktrees and worktree-less branch rows (issue #47) are both
            // secondary, so they render dimmed.
            if worktree.is_missing || !worktree.has_worktree {
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

/// The list-pane title: name, visible/total counts, the worktree-less branch
/// count (issue #47), and the active sort.
fn list_title(app: &App, theme: &Theme, focused: bool) -> Line<'static> {
    // Count worktrees and branch rows separately so the "worktrees (N)" tally
    // never silently folds in the branch rows beneath them.
    let wt_total = app.worktrees.iter().filter(|w| w.has_worktree).count();
    let wt_visible = app
        .visible
        .iter()
        .filter(|&&i| app.worktrees[i].has_worktree)
        .count();
    let count = if wt_visible == wt_total {
        format!(" ({wt_total})")
    } else {
        format!(" ({wt_visible}/{wt_total})")
    };
    let mut spans = vec![
        Span::styled("worktrees", theme.title(focused)),
        Span::styled(count, theme.label()),
    ];
    let br_total = app.worktrees.len() - wt_total;
    if br_total > 0 {
        let br_visible = app.visible.len() - wt_visible;
        let branches = if br_visible == br_total {
            format!(" · {br_total} branches")
        } else {
            format!(" · {br_visible}/{br_total} branches")
        };
        spans.push(Span::styled(branches, theme.label()));
    }
    spans.push(Span::styled(
        format!(" · {}", sort_label(&app.sort)),
        theme.label(),
    ));
    Line::from(spans)
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
    let (status, status_style) = if !worktree.has_worktree {
        // A worktree-less branch row (issue #47): a branch with no checkout.
        (glyphs.branchless(), theme.branchless())
    } else if worktree.is_current {
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
    } else if worktree.dirty.is_none() && !worktree.is_missing && worktree.has_worktree {
        // Loaded but the status read failed: the absent marker, not a blank that
        // would read as "clean" (spec §10). A branch row has no working tree, so
        // its `None` dirty is legitimate and stays blank (issue #47).
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
