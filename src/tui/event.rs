//! Pure event handling for the TUI (spec §10). [`App::handle_event`] maps a
//! terminal event to a state mutation and an [`Effect`] for the runtime to
//! execute (switch, create, remove, refresh, …). No terminal I/O happens here,
//! which is what makes the whole interaction testable.

use std::path::PathBuf;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::keys::{KeyAction, KeyChord};
use crate::tui::app::{App, ComposeField, CreateState, CreateStep, MIN_HEIGHT, Mode, Pane};

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
    /// Draft the PR title/body with the code agent (seeds the compose form).
    DraftPrAi,
    /// Submit the composed PR (push + create/update).
    SubmitPr {
        /// The PR title.
        title: String,
        /// The PR body.
        body: String,
        /// Whether to open as a draft (create only).
        draft: bool,
    },
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

/// Extends the create-prompt base ref to the longest common prefix of the local
/// branches that start with it (a no-op when nothing matches or there is no
/// progress to make). Best-effort completion: an empty candidate list — e.g.
/// when branch enumeration failed — simply does nothing.
fn complete_base_ref(state: &mut CreateState, branches: &[String]) {
    let matches: Vec<&str> = branches
        .iter()
        .map(String::as_str)
        .filter(|b| b.starts_with(&state.base))
        .collect();
    if let Some(common) = longest_common_prefix(&matches)
        && common.len() > state.base.len()
    {
        state.base = common;
    }
}

/// The longest common prefix shared by all `items`, or `None` when `items` is
/// empty. A single item yields the whole item.
fn longest_common_prefix(items: &[&str]) -> Option<String> {
    let (first, rest) = items.split_first()?;
    let mut prefix: &str = first;
    for item in rest {
        // Trim `prefix` to the shared leading chars with `item`, ending on a
        // `prefix` char boundary so the slice is always valid UTF-8.
        let shared = prefix
            .char_indices()
            .zip(item.chars())
            .take_while(|&((_, a), b)| a == b)
            .map(|((i, a), _)| i + a.len_utf8())
            .last()
            .unwrap_or(0);
        prefix = &prefix[..shared];
        if prefix.is_empty() {
            break;
        }
    }
    Some(prefix.to_string())
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
            Mode::PrCompose(_) => self.key_compose(key),
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
            KeyAction::NavigateUp => self.nav_or_scroll(-1),
            KeyAction::NavigateDown => self.nav_or_scroll(1),
            KeyAction::PageUp => self.nav_or_scroll(-page),
            KeyAction::PageDown => self.nav_or_scroll(page),
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
            KeyCode::Tab => {
                if state.step == CreateStep::Base {
                    complete_base_ref(state, &self.branches);
                }
            }
            KeyCode::Esc => self.mode = Mode::List,
            KeyCode::Enter => match state.step {
                CreateStep::Branch => {
                    let branch = state.branch.trim();
                    if branch.is_empty() {
                        state.error = Some("branch name is required".into());
                    } else if let Err(msg) = crate::git::validate_branch_name(branch) {
                        state.error = Some(msg);
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

    /// PR-compose key handling. Fixed keys (overlay convention): typing edits
    /// the active field, Tab switches fields, Enter advances (title) or inserts a
    /// newline (body), Ctrl-S submits, Ctrl-D toggles draft, Esc cancels.
    fn key_compose(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let Mode::PrCompose(state) = &mut self.mode else {
            return Effect::None;
        };
        // Ignore input while a submit/draft is in flight.
        if state.submitting {
            return Effect::None;
        }
        match key.code {
            KeyCode::Char('s') if ctrl => {
                if state.title.trim().is_empty() {
                    state.error = Some("a PR title is required".into());
                } else {
                    state.submitting = true;
                    return Effect::SubmitPr {
                        title: state.title.clone(),
                        body: state.body.clone(),
                        draft: state.draft,
                    };
                }
            }
            KeyCode::Char('d') if ctrl => state.draft = !state.draft,
            // Plain character input (Ctrl chords are handled above / ignored).
            KeyCode::Char(c) if !ctrl => {
                match state.field {
                    ComposeField::Title => state.title.push(c),
                    ComposeField::Body => state.body.push(c),
                }
                state.error = None;
            }
            KeyCode::Backspace => {
                match state.field {
                    ComposeField::Title => state.title.pop(),
                    ComposeField::Body => state.body.pop(),
                };
                state.error = None;
            }
            KeyCode::Tab | KeyCode::BackTab => {
                state.field = match state.field {
                    ComposeField::Title => ComposeField::Body,
                    ComposeField::Body => ComposeField::Title,
                };
            }
            KeyCode::Enter => match state.field {
                ComposeField::Title => state.field = ComposeField::Body,
                ComposeField::Body => state.body.push('\n'),
            },
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
                // The bottom row is the status/help bar; clicks there select nothing.
                let status_row = self.size.1.saturating_sub(1);
                if mouse.row >= status_row {
                    return Effect::None;
                }
                if self.show_sidebar && mouse.column < self.sidebar_width {
                    // Only content rows select; the top border/title row (row 0)
                    // does not.
                    if mouse.row >= LIST_TOP {
                        self.select_row((mouse.row - LIST_TOP) as usize);
                    }
                    self.focus = Pane::List;
                } else {
                    self.focus = Pane::Detail;
                }
            }
            MouseEventKind::ScrollUp => self.nav_or_scroll(-1),
            MouseEventKind::ScrollDown => self.nav_or_scroll(1),
            _ => {}
        }
        Effect::None
    }

    /// Routes a vertical movement to the detail-pane scroll when that pane has
    /// focus, else to the list selection (spec §10).
    fn nav_or_scroll(&mut self, delta: isize) {
        if self.focus == Pane::Detail {
            self.scroll_detail(delta);
        } else {
            self.move_selection(delta);
        }
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
    fn create_mode_rejects_invalid_branch_name() {
        let mut a = app(&[("a", true)]);
        a.handle_event(press(KeyCode::Char('n')));
        for c in "feat..x".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        // Invalid ref name -> inline error, stays on the branch step.
        a.handle_event(press(KeyCode::Enter));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.step, CreateStep::Branch);
            assert!(s.error.as_deref().unwrap().contains("invalid branch name"));
        } else {
            panic!("expected create mode");
        }
        // Typing clears the error (existing char arm).
        a.handle_event(press(KeyCode::Char('y')));
        if let Mode::Create(s) = &a.mode {
            assert!(s.error.is_none());
        }
        // A legal name then advances to the base step.
        if let Mode::Create(s) = &mut a.mode {
            s.branch = "feature/x".into();
        }
        a.handle_event(press(KeyCode::Enter));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.step, CreateStep::Base);
        } else {
            panic!("expected create mode");
        }
    }

    #[test]
    fn create_mode_tab_completes_base_ref() {
        let mut a = app(&[("a", true)]);
        a.branches = vec!["feature/alpha".into(), "feature/beta".into(), "main".into()];
        a.handle_event(press(KeyCode::Char('n')));
        for c in "topic".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        a.handle_event(press(KeyCode::Enter)); // advance to base step
        // Ambiguous prefix extends to the longest common prefix.
        for c in "feat".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        a.handle_event(press(KeyCode::Tab));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.base, "feature/");
        } else {
            panic!("expected create mode");
        }
        // Disambiguate, then Tab completes the unique branch fully.
        a.handle_event(press(KeyCode::Char('a')));
        a.handle_event(press(KeyCode::Tab));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.base, "feature/alpha");
        }
    }

    #[test]
    fn create_mode_tab_noop_without_candidates() {
        let mut a = app(&[("a", true)]);
        a.handle_event(press(KeyCode::Char('n')));
        for c in "feature/x".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        a.handle_event(press(KeyCode::Enter)); // base step
        for c in "xyz".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        a.handle_event(press(KeyCode::Tab)); // no branches -> no change
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.base, "xyz");
        }
        // Tab on the branch step is a no-op (completion is base-only).
        let mut b = app(&[("a", true)]);
        b.branches = vec!["main".into()];
        b.handle_event(press(KeyCode::Char('n')));
        b.handle_event(press(KeyCode::Tab));
        if let Mode::Create(s) = &b.mode {
            assert!(s.branch.is_empty());
        }
    }

    #[test]
    fn longest_common_prefix_cases() {
        assert_eq!(longest_common_prefix(&[]), None);
        assert_eq!(longest_common_prefix(&["solo"]).as_deref(), Some("solo"));
        assert_eq!(
            longest_common_prefix(&["feature/a", "feature/b"]).as_deref(),
            Some("feature/")
        );
        assert_eq!(longest_common_prefix(&["abc", "xyz"]).as_deref(), Some(""));
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
    fn compose_typing_field_switch_and_newline() {
        use crate::tui::app::PrComposeState;
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrCompose(PrComposeState::default());
        a.handle_event(press(KeyCode::Char('h')));
        a.handle_event(press(KeyCode::Char('i')));
        if let Mode::PrCompose(s) = &a.mode {
            assert_eq!(s.title, "hi");
            assert_eq!(s.field, ComposeField::Title);
        } else {
            panic!("expected compose mode");
        }
        // Enter in the title advances to the body.
        a.handle_event(press(KeyCode::Enter));
        if let Mode::PrCompose(s) = &a.mode {
            assert_eq!(s.field, ComposeField::Body);
        }
        // Typing + Enter in the body inserts a newline.
        a.handle_event(press(KeyCode::Char('x')));
        a.handle_event(press(KeyCode::Enter));
        a.handle_event(press(KeyCode::Char('y')));
        if let Mode::PrCompose(s) = &a.mode {
            assert_eq!(s.body, "x\ny");
        }
        // Tab switches back to the title; Backspace pops the active field.
        a.handle_event(press(KeyCode::Tab));
        a.handle_event(press(KeyCode::Backspace));
        if let Mode::PrCompose(s) = &a.mode {
            assert_eq!(s.field, ComposeField::Title);
            assert_eq!(s.title, "h");
        }
    }

    #[test]
    fn compose_ctrl_s_requires_title_and_is_not_typed() {
        use crate::tui::app::PrComposeState;
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrCompose(PrComposeState::default());
        let effect = a.handle_event(ctrl('s'));
        assert_eq!(effect, Effect::None);
        if let Mode::PrCompose(s) = &a.mode {
            assert!(s.error.is_some());
            // Ctrl-S must not be inserted as a literal 's'.
            assert_eq!(s.title, "");
        } else {
            panic!("expected compose mode");
        }
    }

    #[test]
    fn compose_ctrl_s_submits_when_title_present() {
        use crate::tui::app::PrComposeState;
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrCompose(PrComposeState {
            title: "T".into(),
            body: "B".into(),
            ..Default::default()
        });
        let effect = a.handle_event(ctrl('s'));
        assert_eq!(
            effect,
            Effect::SubmitPr {
                title: "T".into(),
                body: "B".into(),
                draft: false
            }
        );
        if let Mode::PrCompose(s) = &a.mode {
            assert!(s.submitting);
        }
    }

    #[test]
    fn compose_ctrl_d_toggles_draft_and_esc_cancels() {
        use crate::tui::app::PrComposeState;
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrCompose(PrComposeState::default());
        a.handle_event(ctrl('d'));
        if let Mode::PrCompose(s) = &a.mode {
            assert!(s.draft);
        }
        a.handle_event(press(KeyCode::Esc));
        assert_eq!(a.mode, Mode::List);
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

    #[test]
    fn navigation_scrolls_detail_when_focused() {
        let mut a = app(&[("a", true), ("b", false)]);
        a.worktrees[0].recent_commits = vec![crate::model::Commit {
            hash: "h".into(),
            subject: "s".into(),
            author: "x".into(),
            timestamp: "2024-01-15T10:30:00Z".into(),
        }];
        a.handle_event(press(KeyCode::Tab)); // focus the detail pane
        a.handle_event(press(KeyCode::Char('j'))); // scrolls detail, not list
        assert_eq!(a.detail_scroll, 1);
        assert_eq!(a.selected, 0);
        a.handle_event(press(KeyCode::Char('k')));
        assert_eq!(a.detail_scroll, 0);
        // Back on the list, navigation moves the selection and resets scroll.
        a.handle_event(press(KeyCode::Tab));
        a.detail_scroll = 3;
        a.handle_event(press(KeyCode::Char('j')));
        assert_eq!(a.selected, 1);
        assert_eq!(a.detail_scroll, 0);
    }

    #[test]
    fn mouse_click_on_status_bar_and_title_row_select_nothing() {
        let mut a = app(&[("a", true), ("b", false), ("c", false)]);
        a.size = (100, 30);
        a.selected = 1;
        // Click the bottom status bar row (row 29): no selection change.
        let click = |row: u16| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 5,
                row,
                modifiers: KeyModifiers::empty(),
            })
        };
        a.handle_event(click(29));
        assert_eq!(a.selected, 1);
        // Click the list title/border row (row 0): no selection change.
        a.handle_event(click(0));
        assert_eq!(a.selected, 1);
    }
}
