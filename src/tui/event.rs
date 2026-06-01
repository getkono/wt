//! Pure event handling for the TUI (spec §10). [`App::handle_event`] maps a
//! terminal event to a state mutation and an [`Effect`] for the runtime to
//! execute (switch, create, remove, refresh, …). No terminal I/O happens here,
//! which is what makes the whole interaction testable.

use std::path::PathBuf;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};

use crate::keys::{KeyAction, KeyChord};
use crate::tui::app::{App, CreateState, CreateStep, MIN_HEIGHT, Mode, Pane};

/// The minimum/maximum list-pane width when resizing.
const MIN_SIDEBAR: u16 = 10;
const MAX_SIDEBAR: u16 = 100;
/// Rows above the first list row (border) for mouse hit-testing.
const LIST_TOP: u16 = 1;

/// An action for the runtime to perform after a state transition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Nothing to do.
    None,
    /// Switch to the given path (print it, exit).
    Switch(PathBuf),
    /// Quit without switching.
    Quit,
    /// The terminal is too small; exit with a message (spec §10).
    TooSmall,
    /// Create a worktree for `branch` based on `base`.
    Create {
        /// The new branch name.
        branch: String,
        /// The base ref (or `None` for the default).
        base: Option<String>,
    },
    /// Remove the worktree at the given index (confirmed; force semantics).
    Remove(usize),
    /// Open the PR picker — the runtime fetches PRs.
    FetchPrs,
    /// Check out the PR with the given number.
    CheckoutPr(u64),
    /// Open the given path in the editor.
    OpenEditor(PathBuf),
    /// Force a full async refresh.
    Refresh,
}

impl CreateState {
    /// The field currently being edited.
    fn field_mut(&mut self) -> &mut String {
        match self.step {
            CreateStep::Branch => &mut self.branch,
            CreateStep::Base => &mut self.base,
        }
    }
}

impl App {
    /// Handles a terminal event, returning the [`Effect`] to perform.
    pub fn handle_event(&mut self, event: Event) -> Effect {
        match event {
            Event::Resize(cols, rows) => self.on_resize(cols, rows),
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Mouse(mouse) if self.mouse => self.on_mouse(mouse),
            _ => Effect::None,
        }
    }

    /// Handles a resize, exiting if the terminal is too short.
    fn on_resize(&mut self, cols: u16, rows: u16) -> Effect {
        self.size = (cols, rows);
        if rows < MIN_HEIGHT {
            Effect::TooSmall
        } else {
            Effect::None
        }
    }

    /// Dispatches a key event by mode.
    fn on_key(&mut self, key: KeyEvent) -> Effect {
        match &self.mode {
            Mode::List => self.key_list(key),
            Mode::Filter => self.key_filter(key),
            Mode::Create(_) => self.key_create(key),
            Mode::PrPicker(_) => self.key_pr(key),
            Mode::ConfirmRemove(_) => self.key_confirm(key),
            Mode::Help => {
                self.mode = Mode::List;
                Effect::None
            }
        }
    }

    /// List-mode key handling (via the configurable keymap).
    fn key_list(&mut self, key: KeyEvent) -> Effect {
        let Some(action) = self.keymap.action_for(KeyChord::from_event(key)) else {
            return Effect::None;
        };
        let page = (self.size.1 as isize - 3).max(1);
        match action {
            KeyAction::NavigateUp => self.move_selection(-1),
            KeyAction::NavigateDown => self.move_selection(1),
            KeyAction::PageUp => self.move_selection(-page),
            KeyAction::PageDown => self.move_selection(page),
            KeyAction::GoToTop => self.select_edge(false),
            KeyAction::GoToBottom => self.select_edge(true),
            KeyAction::FocusNextPane | KeyAction::FocusPrevPane => self.toggle_focus(),
            KeyAction::Switch => {
                if let Some(wt) = self.selected_worktree() {
                    let path = wt.path.clone();
                    self.chosen = Some(path.clone());
                    return Effect::Switch(path);
                }
            }
            KeyAction::Filter => self.mode = Mode::Filter,
            KeyAction::ClearFilter => self.clear_filter(),
            KeyAction::New => self.mode = Mode::Create(CreateState::default()),
            KeyAction::Remove => {
                if let Some(&index) = self.visible.get(self.selected) {
                    self.mode = Mode::ConfirmRemove(index);
                }
            }
            KeyAction::PrCheckout => {
                self.mode = Mode::PrPicker(crate::tui::app::PrPickerState {
                    loading: true,
                    ..Default::default()
                });
                return Effect::FetchPrs;
            }
            KeyAction::OpenEditor => {
                if let Some(wt) = self.selected_worktree() {
                    return Effect::OpenEditor(wt.path.clone());
                }
            }
            KeyAction::Refresh => return Effect::Refresh,
            KeyAction::SortCycle => self.cycle_sort(),
            KeyAction::SortReverse => self.reverse_sort(),
            KeyAction::Help => self.mode = Mode::Help,
            KeyAction::Quit => {
                self.quit = true;
                return Effect::Quit;
            }
            KeyAction::ToggleSidebar => self.show_sidebar = !self.show_sidebar,
            KeyAction::ResizeSidebarGrow => {
                self.sidebar_width = (self.sidebar_width + 1).min(MAX_SIDEBAR);
            }
            KeyAction::ResizeSidebarShrink => {
                self.sidebar_width = self.sidebar_width.saturating_sub(1).max(MIN_SIDEBAR);
            }
        }
        Effect::None
    }

    /// Filter-mode key handling.
    fn key_filter(&mut self, key: KeyEvent) -> Effect {
        match key.code {
            KeyCode::Char(c) => self.filter_push(c),
            KeyCode::Backspace => self.filter_pop(),
            KeyCode::Enter => self.mode = Mode::List, // keep the filter active
            KeyCode::Esc => {
                self.clear_filter();
                self.mode = Mode::List;
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            _ => {}
        }
        Effect::None
    }

    /// Create-mode key handling.
    fn key_create(&mut self, key: KeyEvent) -> Effect {
        let Mode::Create(state) = &mut self.mode else {
            return Effect::None;
        };
        match key.code {
            KeyCode::Char(c) => {
                state.field_mut().push(c);
                state.error = None;
            }
            KeyCode::Backspace => {
                state.field_mut().pop();
            }
            KeyCode::Esc => self.mode = Mode::List,
            KeyCode::Enter => match state.step {
                CreateStep::Branch => {
                    if state.branch.trim().is_empty() {
                        state.error = Some("branch name is required".into());
                    } else {
                        state.step = CreateStep::Base;
                    }
                }
                CreateStep::Base => {
                    let branch = state.branch.clone();
                    let base = (!state.base.trim().is_empty()).then(|| state.base.clone());
                    return Effect::Create { branch, base };
                }
            },
            _ => {}
        }
        Effect::None
    }

    /// PR-picker key handling.
    fn key_pr(&mut self, key: KeyEvent) -> Effect {
        let Mode::PrPicker(state) = &mut self.mode else {
            return Effect::None;
        };
        match key.code {
            KeyCode::Up => state.selected = state.selected.saturating_sub(1),
            KeyCode::Down => {
                state.selected = (state.selected + 1).min(state.prs.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                if let Some(pr) = state.prs.get(state.selected) {
                    return Effect::CheckoutPr(pr.number);
                }
            }
            KeyCode::Esc => self.mode = Mode::List,
            _ => {}
        }
        Effect::None
    }

    /// Confirm-remove key handling.
    fn key_confirm(&mut self, key: KeyEvent) -> Effect {
        let Mode::ConfirmRemove(index) = self.mode else {
            return Effect::None;
        };
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.mode = Mode::List;
            Effect::Remove(index)
        } else {
            self.mode = Mode::List;
            Effect::None
        }
    }

    /// Mouse handling: row click selects, wheel scrolls, detail click focuses.
    fn on_mouse(&mut self, mouse: MouseEvent) -> Effect {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.show_sidebar && mouse.column < self.sidebar_width {
                    let row = mouse.row.saturating_sub(LIST_TOP) as usize;
                    self.select_row(row);
                    self.focus = Pane::List;
                } else {
                    self.focus = Pane::Detail;
                }
            }
            MouseEventKind::ScrollUp => self.move_selection(-1),
            MouseEventKind::ScrollDown => self.move_selection(1),
            _ => {}
        }
        Effect::None
    }

    /// Toggles pane focus.
    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Pane::List => Pane::Detail,
            Pane::Detail => Pane::List,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::app::testutil::app;
    use crossterm::event::{KeyModifiers, MouseButton};

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::empty()))
    }

    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    #[test]
    fn navigation_keys() {
        let mut a = app(&[("a", true), ("b", false), ("c", false)]);
        a.selected = 0;
        assert_eq!(a.handle_event(press(KeyCode::Char('j'))), Effect::None);
        assert_eq!(a.selected, 1);
        a.handle_event(press(KeyCode::Char('k')));
        assert_eq!(a.selected, 0);
        a.handle_event(press(KeyCode::Char('G')));
        assert_eq!(a.selected, 2);
        a.handle_event(press(KeyCode::Char('g')));
        assert_eq!(a.selected, 0);
        a.handle_event(ctrl('d')); // page down
        assert!(a.selected >= 1 || a.visible.len() == 1);
    }

    #[test]
    fn enter_switches_to_selected() {
        let mut a = app(&[("main", true), ("feat", false)]);
        a.selected = 1;
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(effect, Effect::Switch(std::path::PathBuf::from("/r/feat")));
        assert_eq!(a.chosen, Some(std::path::PathBuf::from("/r/feat")));
    }

    #[test]
    fn quit_returns_quit() {
        let mut a = app(&[("a", true)]);
        assert_eq!(a.handle_event(press(KeyCode::Char('q'))), Effect::Quit);
        assert!(a.quit);
    }

    #[test]
    fn filter_mode_typing_and_escape() {
        let mut a = app(&[("alpha", true), ("beta", false)]);
        a.handle_event(press(KeyCode::Char('/')));
        assert_eq!(a.mode, Mode::Filter);
        a.handle_event(press(KeyCode::Char('a')));
        a.handle_event(press(KeyCode::Char('l')));
        assert_eq!(a.filter, "al");
        assert_eq!(a.visible.len(), 1); // only alpha
        a.handle_event(press(KeyCode::Enter)); // confirm
        assert_eq!(a.mode, Mode::List);
        assert_eq!(a.filter, "al"); // stays active
        a.handle_event(press(KeyCode::Char('/')));
        a.handle_event(press(KeyCode::Esc)); // clears
        assert_eq!(a.mode, Mode::List);
        assert_eq!(a.filter, "");
    }

    #[test]
    fn create_mode_flow() {
        let mut a = app(&[("a", true)]);
        a.handle_event(press(KeyCode::Char('n')));
        assert!(matches!(a.mode, Mode::Create(_)));
        // Empty branch -> error, stays on branch step.
        a.handle_event(press(KeyCode::Enter));
        if let Mode::Create(s) = &a.mode {
            assert!(s.error.is_some());
        } else {
            panic!("expected create mode");
        }
        // Type a branch, advance to base.
        for c in "feature/x".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        a.handle_event(press(KeyCode::Enter));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.step, CreateStep::Base);
            assert_eq!(s.branch, "feature/x");
        }
        // Submit with empty base -> Create effect with base None.
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(
            effect,
            Effect::Create {
                branch: "feature/x".into(),
                base: None
            }
        );
    }

    #[test]
    fn create_mode_escape_cancels() {
        let mut a = app(&[("a", true)]);
        a.handle_event(press(KeyCode::Char('n')));
        a.handle_event(press(KeyCode::Esc));
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn confirm_remove_y_removes() {
        let mut a = app(&[("main", true), ("feat", false)]);
        a.selected = 1;
        a.handle_event(press(KeyCode::Char('d')));
        assert!(matches!(a.mode, Mode::ConfirmRemove(_)));
        let effect = a.handle_event(press(KeyCode::Char('y')));
        // index 1 = feat (default sort keeps order here).
        assert!(matches!(effect, Effect::Remove(_)));
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn confirm_remove_other_key_cancels() {
        let mut a = app(&[("main", true), ("feat", false)]);
        a.selected = 1;
        a.handle_event(press(KeyCode::Char('d')));
        let effect = a.handle_event(press(KeyCode::Char('n')));
        assert_eq!(effect, Effect::None);
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn pr_picker_opens_and_fetches() {
        let mut a = app(&[("a", true)]);
        let effect = a.handle_event(press(KeyCode::Char('p')));
        assert_eq!(effect, Effect::FetchPrs);
        assert!(matches!(a.mode, Mode::PrPicker(_)));
        // Populate PRs and check out one.
        if let Mode::PrPicker(s) = &mut a.mode {
            s.loading = false;
            s.prs = vec![
                crate::tui::app::PrItem {
                    number: 7,
                    title: "x".into(),
                    author: "a".into(),
                    state: "open".into(),
                },
                crate::tui::app::PrItem {
                    number: 9,
                    title: "y".into(),
                    author: "b".into(),
                    state: "open".into(),
                },
            ];
        }
        a.handle_event(press(KeyCode::Down));
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(effect, Effect::CheckoutPr(9));
    }

    #[test]
    fn help_dismisses_on_any_key() {
        let mut a = app(&[("a", true)]);
        a.handle_event(press(KeyCode::Char('?')));
        assert_eq!(a.mode, Mode::Help);
        a.handle_event(press(KeyCode::Char('x')));
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn sort_and_sidebar_keys() {
        let mut a = app(&[("a", true)]);
        a.handle_event(press(KeyCode::Char('s')));
        assert_eq!(a.sort.key, crate::model::SortKey::Dirty);
        a.handle_event(press(KeyCode::Char('S')));
        assert!(a.sort.descending);
        let w0 = a.sidebar_width;
        a.handle_event(press(KeyCode::Char('+')));
        assert_eq!(a.sidebar_width, w0 + 1);
        a.handle_event(press(KeyCode::Char('-')));
        assert_eq!(a.sidebar_width, w0);
        a.handle_event(press(KeyCode::Char('\\')));
        assert!(!a.show_sidebar);
    }

    #[test]
    fn resize_too_small_exits() {
        let mut a = app(&[("a", true)]);
        assert_eq!(a.handle_event(Event::Resize(100, 4)), Effect::TooSmall);
        assert_eq!(a.handle_event(Event::Resize(100, 20)), Effect::None);
        assert_eq!(a.size, (100, 20));
    }

    #[test]
    fn open_editor_and_refresh() {
        let mut a = app(&[("a", true)]);
        assert_eq!(
            a.handle_event(press(KeyCode::Char('o'))),
            Effect::OpenEditor(std::path::PathBuf::from("/r/a"))
        );
        assert_eq!(a.handle_event(press(KeyCode::Char('r'))), Effect::Refresh);
    }

    #[test]
    fn mouse_click_selects_and_wheel_scrolls() {
        let mut a = app(&[("a", true), ("b", false), ("c", false)]);
        // Click row 2 (1-based with border offset) within sidebar.
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
        a.handle_event(click);
        assert_eq!(a.selected, 2);
        a.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        }));
        assert_eq!(a.selected, 1);
    }

    #[test]
    fn mouse_ignored_when_disabled() {
        let mut a = app(&[("a", true), ("b", false)]);
        a.mouse = false;
        a.selected = 0;
        a.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        }));
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn tab_toggles_focus() {
        let mut a = app(&[("a", true)]);
        assert_eq!(a.focus, Pane::List);
        a.handle_event(press(KeyCode::Tab));
        assert_eq!(a.focus, Pane::Detail);
    }
}
