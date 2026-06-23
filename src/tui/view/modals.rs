//! Modal overlays for the TUI (spec §10): the help screen, the create and
//! checkout pickers, the PR compose form and picker, and the confirm dialogs.
//! Each is a pure render of [`App`] state plus its mode-specific state; the
//! parent [`render`](super::render) dispatches to these by [`Mode`](super::Mode).

use super::*;

/// Renders the help overlay: the full key-binding reference, generated from the
/// live [`Keymap`](crate::keys::Keymap) so every action is documented and the
/// list can never drift from the actual bindings (issue #39). One row per
/// [`KeyAction`], skipping any action the user has unbound.
pub(super) fn render_help(app: &App, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
    let lines: Vec<Line> = KeyAction::ALL
        .iter()
        .filter_map(|&action| {
            app.keymap
                .display_for(action)
                .map(|keys| (keys, action.label()))
        })
        .map(|(keys, desc)| {
            Line::from(vec![
                Span::styled(format!("{keys:<14}"), theme.hint_key()),
                Span::styled(desc.to_string(), theme.hint_label()),
            ])
        })
        .collect();
    let rect = centered(area, 56, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("help", theme.title(true)))),
        rect,
    );
}

/// The most option rows shown at once in an inline dropdown before it scrolls.
const OPTION_ROWS: usize = 6;

/// Builds the inline option-dropdown lines for an open [`OptionList`] (issue
/// #25): each match on its own line with the cursor row highlighted, windowed to
/// [`OPTION_ROWS`] and capped with a "N more" hint. Shared by the create and
/// compose modals so every pop-up field selects options the same way.
fn option_lines(theme: &Theme, options: &OptionList) -> Vec<Line<'static>> {
    let total = options.match_count();
    let cursor = options.cursor();
    // Window of up to OPTION_ROWS rows that keeps the cursor visible.
    let start = if cursor >= OPTION_ROWS {
        cursor - (OPTION_ROWS - 1)
    } else {
        0
    };
    let end = (start + OPTION_ROWS).min(total);
    let labels: Vec<&str> = options.match_labels().collect();
    let mut lines: Vec<Line<'static>> = labels[start..end]
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let highlighted = start + i == cursor;
            let (symbol, style) = if highlighted {
                (theme.selection_symbol(), theme.selection())
            } else {
                ("  ", theme.label())
            };
            Line::from(Span::styled(format!("{symbol}{label}"), style))
        })
        .collect();
    if total > end {
        lines.push(Line::from(Span::styled(
            format!("  … {} more", total - end),
            theme.hint_label(),
        )));
    }
    lines
}

/// The model/effort options dropdown for the active PR-compose field, seeded to
/// the current selection; `None` for the free-text title/body fields.
fn compose_dropdown(state: &PrComposeState) -> Option<OptionList> {
    let (labels, index) = match state.field {
        ComposeField::Model => (
            AgentModel::all()
                .iter()
                .map(|m| m.label().to_string())
                .collect(),
            AgentModel::all().iter().position(|m| *m == state.model)?,
        ),
        ComposeField::Effort => (
            Effort::all()
                .iter()
                .map(|e| e.label().to_string())
                .collect(),
            Effort::all().iter().position(|e| *e == state.effort)?,
        ),
        ComposeField::Title | ComposeField::Body => return None,
    };
    let mut options = OptionList::new(labels);
    options.open();
    options.set_cursor(index);
    Some(options)
}

/// Renders the create-worktree prompt.
pub(super) fn render_create(app: &App, state: &CreateState, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
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
    // Inline options dropdown for the active field (existing branches).
    if state.options.is_open() {
        lines.extend(option_lines(&theme, &state.options));
    }
    lines.push(Line::from(Span::styled(
        hints::format_hint_row(hints::create_hints()),
        theme.hint_label(),
    )));
    // Grow the modal to fit the fields, optional error, dropdown, and hint.
    let rect = centered(area, 60, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("new worktree", theme.title(true)))),
        rect,
    );
}

/// Renders the checkout branch-picker: a single query line over the known
/// branches with the type-ahead dropdown, the target worktree, and any error.
pub(super) fn render_checkout(app: &App, state: &CheckoutState, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
    let target = app
        .worktrees
        .get(state.worktree_index)
        .and_then(|w| w.branch.clone())
        .unwrap_or_else(|| "worktree".to_string());
    let mut lines = vec![
        Line::from(vec![
            Span::styled("worktree: ", theme.label()),
            Span::styled(target, theme.accent()),
        ]),
        Line::from(vec![
            Span::styled("> branch: ", theme.accent()),
            Span::raw(state.query.clone()),
        ]),
    ];
    if let Some(err) = &state.error {
        lines.push(Line::from(Span::styled(format!("! {err}"), theme.error())));
    }
    if state.options.is_open() {
        lines.extend(option_lines(&theme, &state.options));
    }
    lines.push(Line::from(Span::styled(
        hints::format_hint_row(hints::checkout_hints()),
        theme.hint_label(),
    )));
    let rect = centered(area, 60, lines.len() as u16 + 2);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("checkout branch", theme.title(true)))),
        rect,
    );
}

/// Renders the PR compose form (`wt pr open`): a title + multi-line body with a
/// header showing branch → trunk, the create/update action, and the draft state.
pub(super) fn render_pr_compose(app: &App, state: &PrComposeState, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);

    let title_active = state.field == ComposeField::Title;
    let body_active = state.field == ComposeField::Body;
    let model_active = state.field == ComposeField::Model;
    let effort_active = state.field == ComposeField::Effort;
    let draft_mark = if state.draft { "[x]" } else { "[ ]" };
    // The active option field gets the `>` marker, mirroring the text fields.
    let opt_label = |active: bool, label: &'static str| {
        Span::styled(
            label,
            if active {
                theme.accent()
            } else {
                theme.label()
            },
        )
    };

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
            opt_label(
                model_active,
                if model_active {
                    "> model: "
                } else {
                    "  model: "
                },
            ),
            Span::styled(state.model.label().to_string(), theme.accent()),
            Span::raw("   "),
            opt_label(effort_active, "effort: "),
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
    // When a model/effort field is active, show its options dropdown right under
    // the agent-settings line; the modal grows to keep the rest from clipping.
    let mut extra_rows = 0u16;
    if let Some(options) = compose_dropdown(state) {
        let dropdown = option_lines(&theme, &options);
        extra_rows = dropdown.len() as u16;
        lines.splice(2..2, dropdown);
    }
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
            hints::format_hint_row(hints::compose_ai_hints()),
            theme.hint_label(),
        )));
        lines.push(Line::from(Span::styled(
            hints::format_hint_row(hints::compose_edit_hints()),
            theme.hint_label(),
        )));
    }

    // Keep the established size, but grow for an open options dropdown.
    let rect = centered(area, 76, 20 + extra_rows);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("open pull request", theme.title(true))))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

/// Renders the PR picker overlay.
pub(super) fn render_pr_picker(app: &App, state: &PrPickerState, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
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
pub(super) fn render_confirm(app: &App, index: usize, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
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

/// Renders the confirm-create dialog for a worktree-less branch row (issue #47):
/// the branch, its base and ahead/behind, and the tip commit — enough to decide —
/// then a prompt to create a worktree and switch into it.
pub(super) fn render_confirm_create(app: &App, index: usize, frame: &mut Frame, area: Rect) {
    let theme = Theme::with_palette(app.color, app.palette);
    let Some(worktree) = app.worktrees.get(index) else {
        return;
    };
    let now = now_unix();
    let glyphs = Glyphs::new(app.nerd_fonts);
    let loaded = app.is_loaded(worktree);

    let branch_span = Span::styled(branch_display(worktree), theme.branch(false, false));
    let mut lines = vec![Line::from(vec![
        Span::styled("branch: ", theme.label()),
        branch_span,
    ])];
    if let Some(base) = &worktree.base_ref {
        lines.push(Line::from(vec![
            Span::styled("base:   ", theme.label()),
            Span::raw(base.clone()),
        ]));
    }
    if loaded {
        let mut spans = vec![Span::styled("vs base: ", theme.label())];
        spans.extend(ahead_behind_spans(worktree, &theme, true, &glyphs));
        lines.push(Line::from(spans));
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
    } else {
        lines.push(Line::from(vec![
            Span::styled("vs base: ", theme.label()),
            Span::styled("…", theme.spinner()),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::raw("Create a worktree and switch into it? ["),
        Span::styled("y", theme.success()),
        Span::raw("/N]"),
    ]));

    let height = lines.len() as u16 + 2;
    let rect = centered(area, 72, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("create worktree", theme.title(true))))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

/// Renders the confirm-delete dialog for a worktree-less branch row (issue #53):
/// the branch and its tip, then a prompt to delete the local branch. On the
/// `force` re-prompt — after a safe `git branch -d` refused an unmerged branch —
/// the prompt warns that deleting it may discard commits.
pub(super) fn render_confirm_delete_branch(
    app: &App,
    index: usize,
    force: bool,
    frame: &mut Frame,
    area: Rect,
) {
    let theme = Theme::with_palette(app.color, app.palette);
    let Some(worktree) = app.worktrees.get(index) else {
        return;
    };
    let now = now_unix();
    let glyphs = Glyphs::new(app.nerd_fonts);
    let loaded = app.is_loaded(worktree);

    let branch_span = Span::styled(branch_display(worktree), theme.branch(false, false));
    let mut lines = vec![Line::from(vec![
        Span::styled("branch: ", theme.label()),
        branch_span,
    ])];
    if let Some(base) = &worktree.base_ref {
        lines.push(Line::from(vec![
            Span::styled("base:   ", theme.label()),
            Span::raw(base.clone()),
        ]));
    }
    if loaded {
        let mut spans = vec![Span::styled("vs base: ", theme.label())];
        spans.extend(ahead_behind_spans(worktree, &theme, true, &glyphs));
        lines.push(Line::from(spans));
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
    } else {
        lines.push(Line::from(vec![
            Span::styled("vs base: ", theme.label()),
            Span::styled("…", theme.spinner()),
        ]));
    }

    lines.push(Line::from(""));
    if force {
        lines.push(Line::from(Span::styled(
            "(branch is not fully merged — deleting may discard commits)",
            theme.error(),
        )));
        lines.push(Line::from(vec![
            Span::raw("Force-delete this branch? ["),
            Span::styled("y", theme.warning()),
            Span::raw("/N]"),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::raw("Delete this branch? ["),
            Span::styled("y", theme.warning()),
            Span::raw("/N]"),
        ]));
    }

    let height = lines.len() as u16 + 2;
    let rect = centered(area, 72, height);
    frame.render_widget(Clear, rect);
    let title = if force {
        "force-delete branch"
    } else {
        "delete branch"
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled(title, theme.error())))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

/// Renders the stale-base confirm dialog (issue #56): the base a new worktree
/// would fork from is behind its upstream. Offers update / proceed / cancel; when
/// the base has diverged it notes that updating would fail.
pub(super) fn render_confirm_stale_base(
    app: &App,
    state: &StaleBaseState,
    frame: &mut Frame,
    area: Rect,
) {
    let theme = Theme::with_palette(app.color, app.palette);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("new branch: ", theme.label()),
            Span::styled(state.branch.clone(), theme.branch(false, false)),
        ]),
        Line::from(vec![
            Span::styled("base:       ", theme.label()),
            Span::raw(state.base.clone().unwrap_or_else(|| "(default)".into())),
        ]),
        Line::from(vec![
            Span::styled("status:     ", theme.label()),
            Span::styled(
                format!(
                    "{} commit(s) behind {}",
                    state.behind, state.upstream_display
                ),
                theme.warning(),
            ),
        ]),
    ];
    if !state.can_fast_forward {
        lines.push(Line::from(Span::styled(
            "(base has diverged — update will fail; proceed or cancel)",
            theme.error(),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("u", theme.success()),
        Span::raw("pdate the base, "),
        Span::styled("p", theme.warning()),
        Span::raw("roceed off it, or cancel?"),
    ]));

    let height = lines.len() as u16 + 2;
    let rect = centered(area, 72, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().title(Span::styled("base behind origin", theme.warning())))
            .wrap(Wrap { trim: false }),
        rect,
    );
}

/// Renders the submodule-init confirm dialog (issue #50): a freshly created
/// worktree has uninitialized submodules and the policy is left at its `prompt`
/// default. Offers to initialize them recursively, defaulting to yes.
pub(super) fn render_confirm_init_submodules(
    app: &App,
    state: &InitSubmodulesState,
    frame: &mut Frame,
    area: Rect,
) {
    let theme = Theme::with_palette(app.color, app.palette);
    let lines = vec![
        Line::from(vec![
            Span::styled("branch:     ", theme.label()),
            Span::styled(state.branch.clone(), theme.branch(false, false)),
        ]),
        Line::from(vec![
            Span::styled("submodules: ", theme.label()),
            Span::styled(format!("{} uninitialized", state.count), theme.warning()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("Initialize submodules recursively? ["),
            Span::styled("Y", theme.success()),
            Span::raw("/n]"),
        ]),
    ];

    let height = lines.len() as u16 + 2;
    let rect = centered(area, 72, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered().title(Span::styled("initialize submodules", theme.title(true))),
            )
            .wrap(Wrap { trim: false }),
        rect,
    );
}
