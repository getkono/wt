//! Single source of truth for the modal status-bar / overlay key hints (issue
//! #39). Each modal mode declares its keys exactly once here; [`view`] renders
//! both the bottom status bar and the in-overlay hint rows from these tables,
//! and the consistency test below drives every hinted key through the real
//! handler — so a hint can never claim a key the handler ignores, nor drift
//! from it during a refactor.
//!
//! The rebindable List-mode shortcuts are deliberately NOT here: they derive
//! straight from the [`Keymap`](crate::keys::Keymap) plus
//! [`KeyAction::label`](crate::keys::KeyAction::label), which is their own
//! single source of truth.
//!
//! [`view`]: crate::tui::view

/// One status-bar / overlay hint: the on-screen key text and what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hint {
    /// The key as shown to the user (e.g. `Enter`, `Ctrl-S`, `↑/↓`).
    pub key: &'static str,
    /// The action label (e.g. `submit`).
    pub label: &'static str,
}

/// Terse constructor for the static tables below.
const fn hint(key: &'static str, label: &'static str) -> Hint {
    Hint { key, label }
}

/// Filter-mode hints (typing narrows the list; `↑/↓` move within it).
pub fn filter_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[
        hint("type", "to filter"),
        hint("↑/↓", "move"),
        hint("Backspace", "delete"),
        hint("Enter", "apply"),
        hint("Esc", "clear"),
    ];
    HINTS
}

/// Create-worktree prompt hints.
pub fn create_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[
        hint("↑/↓", "options"),
        hint("Enter", "next / submit"),
        hint("Esc", "cancel"),
    ];
    HINTS
}

/// PR-picker hints.
pub fn pr_picker_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[
        hint("↑/↓", "select"),
        hint("Enter", "checkout"),
        hint("Esc", "close"),
    ];
    HINTS
}

/// PR-compose AI auto-fill controls (the first overlay hint row).
pub fn compose_ai_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[
        hint("Ctrl-A", "AI fill"),
        hint("Ctrl-M", "model"),
        hint("Ctrl-E", "effort"),
        hint("↑/↓", "pick"),
    ];
    HINTS
}

/// PR-compose editing controls (the second overlay hint row, and the status
/// bar). `Enter` advances the field (or inserts a newline in the body).
pub fn compose_edit_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[
        hint("Ctrl-S", "submit"),
        hint("Ctrl-D", "draft"),
        hint("Tab", "field"),
        hint("Shift+Tab", "prev field"),
        hint("Enter", "advance"),
        hint("Esc", "cancel"),
    ];
    HINTS
}

/// Checkout branch-picker hints.
pub fn checkout_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[
        hint("↑/↓", "branches"),
        hint("Enter", "checkout"),
        hint("Esc", "cancel"),
    ];
    HINTS
}

/// Confirm-remove dialog hints (any non-`y` key cancels; `Esc` is the prompt).
pub fn confirm_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[hint("y", "remove"), hint("Esc", "cancel")];
    HINTS
}

/// Confirm-create dialog hints: `y` creates a worktree for the branch row and
/// switches into it; any other key cancels (issue #47).
pub fn confirm_create_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[hint("y", "create & switch"), hint("Esc", "cancel")];
    HINTS
}

/// Confirm-delete-branch dialog hints: `y` deletes the branch row's local branch;
/// any other key cancels (issue #53).
pub fn confirm_delete_branch_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[hint("y", "delete"), hint("Esc", "cancel")];
    HINTS
}

/// Help-overlay hints.
pub fn help_hints() -> &'static [Hint] {
    const HINTS: &[Hint] = &[hint("any key", "close")];
    HINTS
}

/// Formats a hint slice into an overlay row, e.g.
/// `↑/↓: options   Enter: next / submit   Esc: cancel`.
pub fn format_hint_row(hints: &[Hint]) -> String {
    hints
        .iter()
        .map(|h| format!("{}: {}", h.key, h.label))
        .collect::<Vec<_>>()
        .join("   ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::testutil::app;
    use crate::tui::app::{
        App, CheckoutState, ComposeField, CreateState, CreateStep, Mode, PrComposeState, PrItem,
        PrPickerState,
    };
    use crate::tui::event::Effect;
    use crate::tui::options::OptionList;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn format_hint_row_joins_key_and_label() {
        let row = format_hint_row(&[hint("↑/↓", "options"), hint("Esc", "cancel")]);
        assert_eq!(row, "↑/↓: options   Esc: cancel");
    }

    #[test]
    fn every_hint_is_well_formed() {
        let tables = [
            filter_hints(),
            create_hints(),
            pr_picker_hints(),
            compose_ai_hints(),
            compose_edit_hints(),
            checkout_hints(),
            confirm_hints(),
            confirm_create_hints(),
            confirm_delete_branch_hints(),
            help_hints(),
        ];
        for table in tables {
            for h in table {
                assert!(!h.key.is_empty(), "empty key in {table:?}");
                assert!(!h.label.is_empty(), "empty label for {:?}", h.key);
            }
        }
    }

    /// Parses a displayed hint key back into the event a user would press. For a
    /// multi-key hint (`↑/↓`) it returns the first key. Panics on an unknown key
    /// so a newly added hint must be taught here too.
    fn key_event(key: &str) -> KeyEvent {
        let first = key.split('/').next().unwrap_or(key);
        let (code, mods) = match first {
            "type" | "any key" => (KeyCode::Char('x'), KeyModifiers::empty()),
            "↑" => (KeyCode::Up, KeyModifiers::empty()),
            "↓" => (KeyCode::Down, KeyModifiers::empty()),
            "Enter" => (KeyCode::Enter, KeyModifiers::empty()),
            "Esc" => (KeyCode::Esc, KeyModifiers::empty()),
            "Tab" => (KeyCode::Tab, KeyModifiers::empty()),
            "Shift+Tab" => (KeyCode::BackTab, KeyModifiers::empty()),
            "Backspace" => (KeyCode::Backspace, KeyModifiers::empty()),
            "y" => (KeyCode::Char('y'), KeyModifiers::empty()),
            "Ctrl-A" => (KeyCode::Char('a'), KeyModifiers::CONTROL),
            "Ctrl-S" => (KeyCode::Char('s'), KeyModifiers::CONTROL),
            "Ctrl-D" => (KeyCode::Char('d'), KeyModifiers::CONTROL),
            "Ctrl-M" => (KeyCode::Char('m'), KeyModifiers::CONTROL),
            "Ctrl-E" => (KeyCode::Char('e'), KeyModifiers::CONTROL),
            other => panic!("unrecognized hint key {other:?}; teach key_event()"),
        };
        KeyEvent::new(code, mods)
    }

    fn pr(number: u64) -> PrItem {
        PrItem {
            number,
            title: format!("pr {number}"),
            author: "x".into(),
            state: "OPEN".into(),
            created_at: "2024-01-15T10:30:00Z".into(),
        }
    }

    fn options(items: &[&str]) -> OptionList {
        let mut ol = OptionList::new(items.iter().map(|s| (*s).into()).collect());
        ol.open();
        ol
    }

    /// Builds an app in the named mode, arranged so every key in that mode's
    /// hint table is active (dropdowns open, fields populated, the selection off
    /// the top edge so `↑` has somewhere to go).
    fn arranged(mode_kind: &str) -> App {
        let mut a = app(&[("alpha", true), ("alpine", false), ("beta", false)]);
        match mode_kind {
            "filter" => {
                a.filter = "al".into();
                a.selected = 1;
                a.mode = Mode::Filter;
            }
            "create" => {
                a.mode = Mode::Create(CreateState {
                    step: CreateStep::Branch,
                    branch: "fe".into(),
                    options: options(&["main", "master"]),
                    ..Default::default()
                });
            }
            "pr_picker" => {
                a.mode = Mode::PrPicker(PrPickerState {
                    prs: vec![pr(1), pr(2)],
                    selected: 1,
                    ..Default::default()
                });
            }
            "compose" => {
                a.mode = Mode::PrCompose(PrComposeState {
                    field: ComposeField::Model,
                    title: "hi".into(),
                    ..Default::default()
                });
            }
            "checkout" => {
                a.mode = Mode::Checkout(CheckoutState {
                    worktree_index: 0,
                    query: "m".into(),
                    options: options(&["main", "master"]),
                    ..Default::default()
                });
            }
            "confirm" => a.mode = Mode::ConfirmRemove(0),
            "confirm_create" => a.mode = Mode::ConfirmCreate(0),
            "confirm_delete_branch" => {
                a.mode = Mode::ConfirmDeleteBranch {
                    index: 0,
                    force: false,
                }
            }
            "help" => a.mode = Mode::Help,
            other => panic!("unknown mode {other}"),
        }
        a
    }

    /// A fingerprint of the parts of the app a modal handler can change.
    fn fingerprint(a: &App) -> String {
        format!("{:?}|{}|{}", a.mode, a.filter, a.selected)
    }

    /// Asserts every hint in `hints`, pressed in a freshly `arranged` app, is
    /// handled — state changes or a non-`None` effect. This is the anti-drift
    /// guard: a hint for a key the handler ignores fails here.
    fn assert_hints_live(mode_kind: &str, hints: &[Hint]) {
        for h in hints {
            let mut a = arranged(mode_kind);
            let before = fingerprint(&a);
            let effect = a.handle_event(Event::Key(key_event(h.key)));
            let after = fingerprint(&a);
            assert!(
                effect != Effect::None || before != after,
                "{mode_kind} hint {:?} ({}) was ignored by the handler",
                h.key,
                h.label,
            );
        }
    }

    #[test]
    fn modal_hints_drive_real_handlers() {
        assert_hints_live("filter", filter_hints());
        assert_hints_live("create", create_hints());
        assert_hints_live("pr_picker", pr_picker_hints());
        assert_hints_live("compose", compose_ai_hints());
        assert_hints_live("compose", compose_edit_hints());
        assert_hints_live("checkout", checkout_hints());
        assert_hints_live("confirm", confirm_hints());
        assert_hints_live("confirm_create", confirm_create_hints());
        assert_hints_live("confirm_delete_branch", confirm_delete_branch_hints());
        assert_hints_live("help", help_hints());
    }
}
