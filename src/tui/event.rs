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

/// The user's answer to the stale-base confirm modal (issue #56): update the
/// base branch (fast-forward it) before creating, or proceed off it as-is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateDecision {
    /// Fast-forward the base to its upstream, then create off the updated base.
    Update,
    /// Create off the (stale) base as-is.
    Proceed,
}

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
    /// Create a worktree for `branch` based on `base`. `decision` is `None` for
    /// the initial attempt — the runtime then pre-flights the base for staleness
    /// (issue #56) and may bounce to the confirm modal — or a concrete choice when
    /// re-issued from that modal (update the base first, or proceed as-is).
    Create {
        /// The new branch name.
        branch: String,
        /// The base ref (or `None` for the default).
        base: Option<String>,
        /// The stale-base decision: `None` to pre-flight, else the chosen action.
        decision: Option<CreateDecision>,
    },
    /// Remove the worktree at the given index (confirmed; force semantics).
    Remove(usize),
    /// Delete the local branch `branch` of a worktree-less branch row (confirmed;
    /// issue #53). `force` uses `git branch -D` to delete an unmerged branch.
    DeleteBranch {
        /// The branch to delete.
        branch: String,
        /// Whether to force-delete an unmerged branch (`-D` vs `-d`).
        force: bool,
    },
    /// Create a worktree for an existing worktree-less branch and switch into it
    /// (confirmed). The runtime materializes the branch via the `new` path and
    /// records the new worktree as the chosen path (issue #47).
    MaterializeBranch {
        /// The branch to create a worktree for.
        branch: String,
    },
    /// Open the PR picker — the runtime fetches PRs.
    FetchPrs,
    /// Check out the PR with the given number.
    CheckoutPr(u64),
    /// Check out `branch` in the worktree at `worktree_index` (in place).
    CheckoutBranch {
        /// Index into `App::worktrees` of the target worktree.
        worktree_index: usize,
        /// The branch to check out.
        branch: String,
    },
    /// Sync (pull then push) the worktree at `worktree_index` in place (issue #63).
    Sync {
        /// Index into `App::worktrees` of the target worktree.
        worktree_index: usize,
    },
    /// Initialize the submodules in `dir` recursively (confirmed; issue #50).
    InitSubmodules {
        /// The worktree directory whose submodules to initialize.
        dir: PathBuf,
        /// How many uninitialized submodules were detected (for the status text).
        count: usize,
    },
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

    /// The text of the field currently being edited.
    fn field_value(&self) -> &str {
        match self.step {
            CreateStep::Branch => &self.branch,
            CreateStep::Base => &self.base,
        }
    }

    /// Re-filters the options dropdown against the active field and shows it.
    fn refresh_options(&mut self) {
        let query = self.field_value().to_owned();
        self.options.refilter(&query);
        self.options.open();
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

/// The next PR-compose field in Tab order (title → body → model → effort → wrap).
fn compose_next_field(field: ComposeField) -> ComposeField {
    match field {
        ComposeField::Title => ComposeField::Body,
        ComposeField::Body => ComposeField::Model,
        ComposeField::Model => ComposeField::Effort,
        ComposeField::Effort => ComposeField::Title,
    }
}

/// The previous PR-compose field in Tab order (the inverse of
/// [`compose_next_field`], for Shift-Tab).
fn compose_prev_field(field: ComposeField) -> ComposeField {
    match field {
        ComposeField::Title => ComposeField::Effort,
        ComposeField::Effort => ComposeField::Model,
        ComposeField::Model => ComposeField::Body,
        ComposeField::Body => ComposeField::Title,
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
            Mode::PrCompose(_) => self.key_compose(key),
            Mode::Checkout(_) => self.key_checkout_picker(key),
            Mode::ConfirmRemove(_) => self.key_confirm(key),
            Mode::ConfirmCreate(_) => self.key_confirm_create(key),
            Mode::ConfirmDeleteBranch { .. } => self.key_confirm_delete_branch(key),
            Mode::ConfirmStaleBase(_) => self.key_confirm_stale_base(key),
            Mode::ConfirmInitSubmodules(_) => self.key_confirm_init_submodules(key),
            Mode::ConfirmQuit { .. } => self.key_confirm_quit(key),
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
                if let Some(&index) = self.visible.get(self.selected) {
                    let wt = &self.worktrees[index];
                    if wt.has_worktree {
                        let path = wt.path.clone();
                        self.chosen = Some(path.clone());
                        return Effect::Switch(path);
                    }
                    // A worktree-less branch row: confirm before creating a
                    // worktree and switching into it (issue #47).
                    self.mode = Mode::ConfirmCreate(index);
                }
            }
            KeyAction::Filter => self.mode = Mode::Filter,
            KeyAction::ClearFilter => self.clear_filter(),
            KeyAction::New => {
                // Seed the branch-options dropdown with existing local + remote
                // branches; it opens once the user types or reaches the base field.
                let options = crate::tui::OptionList::new(self.branches.clone());
                // Default the base to the upstream default branch (e.g.
                // origin/main) so new worktrees fork off the up-to-date remote
                // tip (issue #70); empty when there is no confident default.
                self.mode = Mode::Create(CreateState {
                    base: self.default_base.clone().unwrap_or_default(),
                    options,
                    ..Default::default()
                });
            }
            KeyAction::Remove => {
                // A real worktree is removed; a worktree-less branch row has no
                // worktree to remove, so Remove deletes its local branch instead
                // (issue #53).
                if let Some(&index) = self.visible.get(self.selected) {
                    self.mode = if self.worktrees[index].has_worktree {
                        Mode::ConfirmRemove(index)
                    } else {
                        Mode::ConfirmDeleteBranch {
                            index,
                            force: false,
                        }
                    };
                }
            }
            KeyAction::PrCheckout => {
                self.mode = Mode::PrPicker(crate::tui::app::PrPickerState {
                    loading: true,
                    ..Default::default()
                });
                return Effect::FetchPrs;
            }
            KeyAction::Checkout => {
                // Seed a branch picker for the selected worktree (its index into
                // `worktrees`, matching the `Remove` pattern). Checking out a
                // branch needs a worktree to check it out *in*, so it is a no-op
                // on a branch row (issue #47).
                if let Some(&index) = self.visible.get(self.selected)
                    && self.worktrees[index].has_worktree
                {
                    // Open the dropdown immediately so the local + remote branch
                    // list is browsable with ↑/↓ from the start — checkout is a
                    // pick-an-existing-branch action, not free text entry (#32).
                    let mut options = crate::tui::OptionList::new(self.branches.clone());
                    options.open();
                    self.mode = Mode::Checkout(crate::tui::app::CheckoutState {
                        worktree_index: index,
                        options,
                        ..Default::default()
                    });
                }
            }
            KeyAction::Sync => {
                // Sync works on a real worktree (fetch + ff/push in place) and on a
                // worktree-less branch row (fetch + move the ref / push from the
                // repo root); the runtime picks the path by `has_worktree`
                // (issue #47/#63).
                if let Some(&index) = self.visible.get(self.selected) {
                    return Effect::Sync {
                        worktree_index: index,
                    };
                }
            }
            KeyAction::OpenEditor => {
                // A branch row has only a virtual path; nothing to open (issue #47).
                if let Some(wt) = self.selected_worktree()
                    && wt.has_worktree
                {
                    return Effect::OpenEditor(wt.path.clone());
                }
            }
            KeyAction::Refresh => return Effect::Refresh,
            KeyAction::SortCycle => self.cycle_sort(),
            KeyAction::SortReverse => self.reverse_sort(),
            KeyAction::Help => self.mode = Mode::Help,
            KeyAction::Quit => {
                // Quitting while background jobs run would abandon them (killing
                // in-flight git subprocesses), so confirm first (issue #46
                // overhaul); with nothing running, quit immediately.
                if self.any_jobs() {
                    self.mode = Mode::ConfirmQuit {
                        jobs: self.jobs.len(),
                    };
                } else {
                    self.quit = true;
                    return Effect::Quit;
                }
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

    /// Create-mode key handling. The active field offers an inline dropdown of
    /// existing branches (issue #25): typing filters it, `↑/↓` move into it, and
    /// `Enter` accepts the highlight when engaged — otherwise `Enter` advances /
    /// submits the typed text. `Esc` closes an open dropdown before the modal.
    fn key_create(&mut self, key: KeyEvent) -> Effect {
        let Mode::Create(state) = &mut self.mode else {
            return Effect::None;
        };
        match key.code {
            KeyCode::Char(c) => {
                state.field_mut().push(c);
                state.error = None;
                state.refresh_options();
            }
            KeyCode::Backspace => {
                state.field_mut().pop();
                state.refresh_options();
            }
            KeyCode::Up => state.options.up(),
            KeyCode::Down => state.options.down(),
            KeyCode::Tab => {
                if state.step == CreateStep::Base {
                    complete_base_ref(state, &self.branches);
                    state.refresh_options();
                }
            }
            KeyCode::Esc => {
                if state.options.is_open() {
                    state.options.close();
                } else {
                    self.mode = Mode::List;
                }
            }
            KeyCode::Enter => {
                // Accept the highlighted suggestion only once the user has moved
                // into the list; otherwise fall through to advance/submit.
                if let Some(selected) = state.options.selected().map(str::to_owned) {
                    *state.field_mut() = selected;
                    state.options.close();
                } else {
                    match state.step {
                        CreateStep::Branch => {
                            let branch = state.branch.trim();
                            if branch.is_empty() {
                                state.error = Some("branch name is required".into());
                            } else if let Err(msg) = crate::git::validate_branch_name(branch) {
                                state.error = Some(msg);
                            } else {
                                state.step = CreateStep::Base;
                                // Reveal fork candidates as soon as the base field opens.
                                state.refresh_options();
                            }
                        }
                        CreateStep::Base => {
                            let branch = state.branch.clone();
                            let base = (!state.base.trim().is_empty()).then(|| state.base.clone());
                            // No decision yet — the runtime pre-flights the base
                            // for staleness before creating (issue #56).
                            return Effect::Create {
                                branch,
                                base,
                                decision: None,
                            };
                        }
                    }
                }
            }
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

    /// PR-compose key handling. Fixed keys (overlay convention): typing edits the
    /// active text field, Tab cycles the fields (title → body → model → effort),
    /// `↑/↓` pick from the model/effort options dropdown (issue #25), Enter
    /// advances (or inserts a newline in the body), Ctrl-S submits, Ctrl-D toggles
    /// draft, Ctrl-A auto-fills with the agent, Ctrl-M/Ctrl-E quick-cycle the
    /// model/effort, Esc cancels.
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
            // Ctrl-A: auto-fill title/body with the agent (current model/effort).
            KeyCode::Char('a') if ctrl => {
                state.submitting = true;
                state.error = None;
                return Effect::DraftPrAi;
            }
            // Ctrl-M / Ctrl-E: quick-cycle the model / effort used for the next fill.
            KeyCode::Char('m') if ctrl => state.model = state.model.next(),
            KeyCode::Char('e') if ctrl => state.effort = state.effort.next(),
            KeyCode::Char('d') if ctrl => state.draft = !state.draft,
            // Plain character input edits the text fields; option fields ignore it.
            KeyCode::Char(c) if !ctrl => {
                match state.field {
                    ComposeField::Title => state.title.push(c),
                    ComposeField::Body => state.body.push(c),
                    ComposeField::Model | ComposeField::Effort => {}
                }
                state.error = None;
            }
            KeyCode::Backspace => {
                match state.field {
                    ComposeField::Title => state.title.pop(),
                    ComposeField::Body => state.body.pop(),
                    ComposeField::Model | ComposeField::Effort => None,
                };
                state.error = None;
            }
            // On an option field the arrows move the selection like the picker.
            KeyCode::Up => match state.field {
                ComposeField::Model => state.model = state.model.prev(),
                ComposeField::Effort => state.effort = state.effort.prev(),
                _ => {}
            },
            KeyCode::Down => match state.field {
                ComposeField::Model => state.model = state.model.next(),
                ComposeField::Effort => state.effort = state.effort.next(),
                _ => {}
            },
            KeyCode::Tab => state.field = compose_next_field(state.field),
            KeyCode::BackTab => state.field = compose_prev_field(state.field),
            KeyCode::Enter => match state.field {
                ComposeField::Title => state.field = ComposeField::Body,
                ComposeField::Body => state.body.push('\n'),
                // On the option fields, Enter confirms and moves on.
                ComposeField::Model => state.field = ComposeField::Effort,
                ComposeField::Effort => state.field = ComposeField::Title,
            },
            KeyCode::Esc => self.mode = Mode::List,
            _ => {}
        }
        Effect::None
    }

    /// Checkout branch-picker key handling. A single type-ahead field over the
    /// known branches (issue #32): typing filters the dropdown, `↑/↓` move into
    /// it, and `Enter` checks out the highlighted suggestion (when engaged) or the
    /// typed text. A first `Esc` closes an open dropdown; a second cancels.
    fn key_checkout_picker(&mut self, key: KeyEvent) -> Effect {
        let Mode::Checkout(state) = &mut self.mode else {
            return Effect::None;
        };
        // Ignore input while a checkout is in flight.
        if state.submitting {
            return Effect::None;
        }
        match key.code {
            KeyCode::Char(c) => {
                state.query.push(c);
                state.error = None;
                state.options.refilter(&state.query);
                state.options.open();
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.error = None;
                state.options.refilter(&state.query);
                state.options.open();
            }
            KeyCode::Up => state.options.up(),
            KeyCode::Down => state.options.down(),
            KeyCode::Esc => {
                if state.options.is_open() {
                    state.options.close();
                } else {
                    self.mode = Mode::List;
                }
            }
            KeyCode::Enter => {
                // Accept the highlighted suggestion once engaged; else the typed
                // text (so a branch the list does not contain still works).
                let branch = state
                    .options
                    .selected()
                    .map(str::to_owned)
                    .unwrap_or_else(|| state.query.trim().to_string());
                if branch.is_empty() {
                    state.error = Some("branch name is required".into());
                } else {
                    let worktree_index = state.worktree_index;
                    return Effect::CheckoutBranch {
                        worktree_index,
                        branch,
                    };
                }
            }
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

    /// Confirm-create key handling: `y`/`Y` materializes a worktree for the
    /// branch row and switches into it; any other key cancels (issue #47).
    fn key_confirm_create(&mut self, key: KeyEvent) -> Effect {
        let Mode::ConfirmCreate(index) = self.mode else {
            return Effect::None;
        };
        self.mode = Mode::List;
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
            && let Some(branch) = self.worktrees.get(index).and_then(|w| w.branch.clone())
        {
            return Effect::MaterializeBranch { branch };
        }
        Effect::None
    }

    /// Confirm-delete-branch key handling (issue #53): `y`/`Y` deletes the branch
    /// row's local branch; any other key cancels. The `force` flag (set on the
    /// unmerged re-prompt) is carried into the effect so the runtime uses
    /// `git branch -D`.
    fn key_confirm_delete_branch(&mut self, key: KeyEvent) -> Effect {
        let Mode::ConfirmDeleteBranch { index, force } = self.mode else {
            return Effect::None;
        };
        self.mode = Mode::List;
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'))
            && let Some(branch) = self.worktrees.get(index).and_then(|w| w.branch.clone())
        {
            return Effect::DeleteBranch { branch, force };
        }
        Effect::None
    }

    /// Confirm-stale-base key handling (issue #56): `u`/`U` updates the base
    /// (fast-forward) then creates; `p`/`P` proceeds off the stale base; any other
    /// key cancels. Re-issues the create with the chosen decision.
    fn key_confirm_stale_base(&mut self, key: KeyEvent) -> Effect {
        let Mode::ConfirmStaleBase(state) = &self.mode else {
            return Effect::None;
        };
        let branch = state.branch.clone();
        let base = state.base.clone();
        self.mode = Mode::List;
        let decision = match key.code {
            KeyCode::Char('u') | KeyCode::Char('U') => CreateDecision::Update,
            KeyCode::Char('p') | KeyCode::Char('P') => CreateDecision::Proceed,
            _ => return Effect::None,
        };
        Effect::Create {
            branch,
            base,
            decision: Some(decision),
        }
    }

    /// Confirm-init-submodules key handling (issue #50): `Enter`/`y`/`Y` — the
    /// default — initializes the new worktree's submodules recursively; `n`/`N`/`Esc`
    /// dismisses and leaves them uninitialized; any other key is ignored.
    fn key_confirm_init_submodules(&mut self, key: KeyEvent) -> Effect {
        let Mode::ConfirmInitSubmodules(state) = &self.mode else {
            return Effect::None;
        };
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                let dir = state.dir.clone();
                let count = state.count;
                self.mode = Mode::List;
                Effect::InitSubmodules { dir, count }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.mode = Mode::List;
                Effect::None
            }
            _ => Effect::None,
        }
    }

    /// Confirm-quit key handling (issue #46 overhaul): `y`/`Y` quits, abandoning
    /// the running background jobs; any other key cancels back to the list.
    fn key_confirm_quit(&mut self, key: KeyEvent) -> Effect {
        if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
            self.quit = true;
            Effect::Quit
        } else {
            self.mode = Mode::List;
            Effect::None
        }
    }

    /// Mouse handling: row click selects, wheel scrolls, detail click focuses.
    fn on_mouse(&mut self, mouse: MouseEvent) -> Effect {
        // A modal overlay owns all input (issue #70): the background list/detail
        // must not react to clicks, and the wheel scrolls the modal's own list
        // rather than the hidden list behind it. List/Filter render inline (no
        // overlay), so they keep the normal mouse behaviour below.
        if !matches!(self.mode, Mode::List | Mode::Filter) {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.modal_scroll(-1),
                MouseEventKind::ScrollDown => self.modal_scroll(1),
                _ => {}
            }
            return Effect::None;
        }
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

    /// Routes one wheel step to the open modal's own list (issue #70): the
    /// create/checkout pickers move their options dropdown (a no-op while it is
    /// closed), the PR picker moves its selection. Other overlays have nothing
    /// to scroll. `delta` is negative for up, positive for down.
    fn modal_scroll(&mut self, delta: isize) {
        let up = delta < 0;
        match &mut self.mode {
            Mode::Create(state) => {
                if up {
                    state.options.up();
                } else {
                    state.options.down();
                }
            }
            Mode::Checkout(state) => {
                if up {
                    state.options.up();
                } else {
                    state.options.down();
                }
            }
            Mode::PrPicker(state) => {
                if up {
                    state.selected = state.selected.saturating_sub(1);
                } else {
                    state.selected = (state.selected + 1).min(state.prs.len().saturating_sub(1));
                }
            }
            _ => {}
        }
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
    fn enter_on_branch_row_opens_confirm_create() {
        use crate::tui::app::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        a.worktrees.push(branch_row("topic"));
        a.apply_filter(String::new()); // recompute `visible` to include the row
        a.selected = a.visible.len() - 1; // select the branch row
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(effect, Effect::None);
        assert!(matches!(a.mode, Mode::ConfirmCreate(_)));
        assert!(a.chosen.is_none()); // no switch yet — only after confirming
    }

    #[test]
    fn confirm_create_y_materializes_other_cancels() {
        use crate::tui::app::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        a.worktrees.push(branch_row("topic"));
        let idx = a
            .worktrees
            .iter()
            .position(|w| w.branch.as_deref() == Some("topic"))
            .unwrap();
        a.mode = Mode::ConfirmCreate(idx);
        let effect = a.handle_event(press(KeyCode::Char('y')));
        assert_eq!(
            effect,
            Effect::MaterializeBranch {
                branch: "topic".into()
            }
        );
        assert_eq!(a.mode, Mode::List);
        // Any non-`y` key cancels.
        a.mode = Mode::ConfirmCreate(idx);
        let effect = a.handle_event(press(KeyCode::Char('n')));
        assert_eq!(effect, Effect::None);
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn checkout_and_open_editor_are_noops_on_branch_rows() {
        // Checkout / open-editor both need a worktree; on a branch row they do
        // nothing (issue #47). Remove instead deletes the branch — see below.
        use crate::tui::app::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        a.worktrees.push(branch_row("topic"));
        a.apply_filter(String::new());
        a.selected = a.visible.len() - 1; // the branch row
        assert_eq!(a.handle_event(press(KeyCode::Char('c'))), Effect::None);
        assert_eq!(a.mode, Mode::List);
        assert_eq!(a.handle_event(press(KeyCode::Char('o'))), Effect::None);
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn sync_acts_on_worktree_rows_and_branch_rows() {
        use crate::tui::app::testutil::branch_row;
        let mut a = app(&[("main", true), ("feat", false)]);
        // On a real worktree row, `y` yields a Sync effect for that row's index.
        a.selected = 1;
        assert_eq!(
            a.handle_event(press(KeyCode::Char('y'))),
            Effect::Sync { worktree_index: 1 }
        );
        // On a worktree-less branch row, `y` now also yields a Sync effect for that
        // row's index; the runtime syncs it by branch name (issue #47/#63).
        a.worktrees.push(branch_row("topic"));
        a.apply_filter(String::new());
        let idx = a
            .worktrees
            .iter()
            .position(|w| w.branch.as_deref() == Some("topic"))
            .unwrap();
        a.selected = a.visible.iter().position(|&i| i == idx).unwrap();
        assert_eq!(
            a.handle_event(press(KeyCode::Char('y'))),
            Effect::Sync {
                worktree_index: idx
            }
        );
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn remove_on_branch_row_confirms_then_deletes_branch() {
        // Remove on a worktree-less branch row deletes its local branch (issue
        // #53): first a confirm dialog, then `y` yields a DeleteBranch effect.
        use crate::tui::app::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        a.worktrees.push(branch_row("topic"));
        a.apply_filter(String::new());
        a.selected = a.visible.len() - 1; // the branch row
        assert_eq!(a.handle_event(press(KeyCode::Char('d'))), Effect::None);
        assert!(matches!(
            a.mode,
            Mode::ConfirmDeleteBranch { force: false, .. }
        ));
        let effect = a.handle_event(press(KeyCode::Char('y')));
        assert_eq!(
            effect,
            Effect::DeleteBranch {
                branch: "topic".into(),
                force: false,
            }
        );
        assert_eq!(a.mode, Mode::List);
        // A non-`y` key cancels back to the list.
        a.mode = Mode::ConfirmDeleteBranch {
            index: a.visible[a.selected],
            force: true,
        };
        assert_eq!(a.handle_event(press(KeyCode::Char('n'))), Effect::None);
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn confirm_stale_base_keys_reissue_create_or_cancel() {
        use crate::tui::app::StaleBaseState;
        let state = StaleBaseState {
            branch: "feature".into(),
            base: Some("main".into()),
            behind: 2,
            upstream_display: "origin/main".into(),
            can_fast_forward: true,
        };
        let mut a = app(&[("main", true)]);
        // `u` updates the base then creates.
        a.mode = Mode::ConfirmStaleBase(state.clone());
        assert_eq!(
            a.handle_event(press(KeyCode::Char('u'))),
            Effect::Create {
                branch: "feature".into(),
                base: Some("main".into()),
                decision: Some(CreateDecision::Update),
            }
        );
        assert_eq!(a.mode, Mode::List);
        // `p` proceeds off the stale base.
        a.mode = Mode::ConfirmStaleBase(state.clone());
        assert_eq!(
            a.handle_event(press(KeyCode::Char('p'))),
            Effect::Create {
                branch: "feature".into(),
                base: Some("main".into()),
                decision: Some(CreateDecision::Proceed),
            }
        );
        // Any other key (Esc) cancels.
        a.mode = Mode::ConfirmStaleBase(state);
        assert_eq!(a.handle_event(press(KeyCode::Esc)), Effect::None);
        assert_eq!(a.mode, Mode::List);
    }

    #[test]
    fn confirm_init_submodules_keys_init_or_skip() {
        use crate::tui::app::InitSubmodulesState;
        let state = InitSubmodulesState {
            dir: PathBuf::from("/wt/feature"),
            branch: "feature".into(),
            count: 2,
        };
        let mut a = app(&[("main", true)]);
        // Enter (the default) initializes.
        a.mode = Mode::ConfirmInitSubmodules(state.clone());
        assert_eq!(
            a.handle_event(press(KeyCode::Enter)),
            Effect::InitSubmodules {
                dir: PathBuf::from("/wt/feature"),
                count: 2,
            }
        );
        assert_eq!(a.mode, Mode::List);
        // `y` also initializes.
        a.mode = Mode::ConfirmInitSubmodules(state.clone());
        assert_eq!(
            a.handle_event(press(KeyCode::Char('y'))),
            Effect::InitSubmodules {
                dir: PathBuf::from("/wt/feature"),
                count: 2,
            }
        );
        // `n` skips back to the list.
        a.mode = Mode::ConfirmInitSubmodules(state.clone());
        assert_eq!(a.handle_event(press(KeyCode::Char('n'))), Effect::None);
        assert_eq!(a.mode, Mode::List);
        // Esc skips too.
        a.mode = Mode::ConfirmInitSubmodules(state.clone());
        assert_eq!(a.handle_event(press(KeyCode::Esc)), Effect::None);
        assert_eq!(a.mode, Mode::List);
        // An unrelated key is ignored and leaves the modal open.
        a.mode = Mode::ConfirmInitSubmodules(state);
        assert_eq!(a.handle_event(press(KeyCode::Char('x'))), Effect::None);
        assert!(matches!(a.mode, Mode::ConfirmInitSubmodules(_)));
    }

    #[test]
    fn quit_returns_quit() {
        let mut a = app(&[("a", true)]);
        assert_eq!(a.handle_event(press(KeyCode::Char('q'))), Effect::Quit);
        assert!(a.quit);
    }

    #[test]
    fn quit_with_jobs_confirms_first() {
        // Quitting while a background job runs opens the confirm dialog rather than
        // abandoning it outright (issue #46 overhaul).
        use crate::tui::app::JobKey;
        let mut a = app(&[("a", true)]);
        a.begin_job(JobKey::New("feat".into()), "Creating feat");
        assert_eq!(a.handle_event(press(KeyCode::Char('q'))), Effect::None);
        assert!(matches!(a.mode, Mode::ConfirmQuit { jobs: 1 }));
        assert!(!a.quit);
        // `y` quits and abandons the jobs; any other key cancels back to the list.
        assert_eq!(a.handle_event(press(KeyCode::Char('y'))), Effect::Quit);
        assert!(a.quit);
        // Cancel path.
        let mut b = app(&[("a", true)]);
        b.begin_job(JobKey::New("feat".into()), "Creating feat");
        b.handle_event(press(KeyCode::Char('q')));
        assert_eq!(b.handle_event(press(KeyCode::Char('n'))), Effect::None);
        assert_eq!(b.mode, Mode::List);
        assert!(!b.quit);
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
                base: None,
                decision: None,
            }
        );
    }

    #[test]
    fn create_mode_prefills_default_base() {
        // Opening the create prompt seeds the base with the upstream default
        // branch so the new worktree forks off the remote tip (issue #70).
        let mut a = app(&[("main", true)]);
        a.branches = vec!["main".into(), "origin/main".into()];
        a.default_base = Some("origin/main".into());
        a.handle_event(press(KeyCode::Char('n')));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.base, "origin/main");
            assert_eq!(s.step, CreateStep::Branch); // still starts on the branch
        } else {
            panic!("expected create mode");
        }
    }

    #[test]
    fn create_mode_base_empty_without_default() {
        // No detected default: the base starts empty and submitting leaves it
        // `None` so the CLI's own resolution applies (unchanged behaviour).
        let mut a = app(&[("main", true)]);
        assert!(a.default_base.is_none());
        a.handle_event(press(KeyCode::Char('n')));
        for c in "feature/x".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        a.handle_event(press(KeyCode::Enter)); // advance to base
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.base, "");
        } else {
            panic!("expected create mode");
        }
        assert_eq!(
            a.handle_event(press(KeyCode::Enter)),
            Effect::Create {
                branch: "feature/x".into(),
                base: None,
                decision: None,
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
    fn create_mode_dropdown_filters_navigates_and_accepts() {
        let mut a = app(&[("a", true)]);
        a.branches = vec!["main".into(), "origin/main".into(), "origin/dev".into()];
        a.handle_event(press(KeyCode::Char('n')));
        // Type a new branch name (no existing branch contains "feature/login"),
        // so the dropdown has no matches and Enter advances to the base field.
        for c in "feature/login".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        a.handle_event(press(KeyCode::Enter));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.step, CreateStep::Base);
            // The base field opens its dropdown to all fork candidates.
            assert!(s.options.is_open());
        } else {
            panic!("expected create mode");
        }
        // Filter the base candidates to the two "origin/" branches.
        for c in "origin".chars() {
            a.handle_event(press(KeyCode::Char(c)));
        }
        // Engage the list and accept the highlighted suggestion. Matches follow
        // the seeded order [origin/main, origin/dev], so one Down lands on dev.
        a.handle_event(press(KeyCode::Down));
        a.handle_event(press(KeyCode::Enter));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.base, "origin/dev");
            assert!(!s.options.is_open()); // closed after accepting
        }
        // A final Enter (dropdown closed) submits the create.
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(
            effect,
            Effect::Create {
                branch: "feature/login".into(),
                base: Some("origin/dev".into()),
                decision: None,
            }
        );
    }

    #[test]
    fn create_mode_escape_closes_dropdown_before_modal() {
        let mut a = app(&[("a", true)]);
        a.branches = vec!["main".into()];
        a.handle_event(press(KeyCode::Char('n')));
        a.handle_event(press(KeyCode::Char('m'))); // opens the dropdown (matches "main")
        if let Mode::Create(s) = &a.mode {
            assert!(s.options.is_open());
        }
        a.handle_event(press(KeyCode::Esc)); // first Esc closes the dropdown
        if let Mode::Create(s) = &a.mode {
            assert!(!s.options.is_open());
        } else {
            panic!("expected create mode (still open)");
        }
        a.handle_event(press(KeyCode::Esc)); // second Esc cancels the modal
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
                    created_at: String::new(),
                },
                crate::tui::app::PrItem {
                    number: 9,
                    title: "y".into(),
                    author: "b".into(),
                    state: "open".into(),
                    created_at: String::new(),
                },
            ];
        }
        a.handle_event(press(KeyCode::Down));
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(effect, Effect::CheckoutPr(9));
    }

    #[test]
    fn checkout_key_opens_picker_for_selected_worktree() {
        let mut a = app(&[("main", true), ("feature/x", false)]);
        a.branches = vec!["main".into(), "feature/x".into()];
        a.selected = 1; // the feature/x row
        a.handle_event(press(KeyCode::Char('c')));
        if let Mode::Checkout(s) = &a.mode {
            // The target is the selected row's index into `worktrees`.
            assert_eq!(s.worktree_index, a.visible[1]);
            assert_eq!(s.options.match_count(), 2);
            // The branch list is open immediately so ↑/↓ browse it without typing.
            assert!(s.options.is_open());
        } else {
            panic!("expected checkout mode");
        }
    }

    #[test]
    fn checkout_picker_arrows_select_a_branch_without_typing() {
        // The dropdown opens on entry, so ↓ then Enter checks out the highlighted
        // branch with no type-ahead — picking a local or remote branch directly.
        let mut a = app(&[("main", true)]);
        a.branches = vec!["main".into(), "origin/feature/x".into()];
        a.handle_event(press(KeyCode::Char('c')));
        a.handle_event(press(KeyCode::Down)); // engage the list, move off `main`
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(
            effect,
            Effect::CheckoutBranch {
                worktree_index: 0,
                branch: "origin/feature/x".into(),
            }
        );
    }

    #[test]
    fn checkout_picker_submits_typed_branch() {
        let mut a = app(&[("main", true)]);
        a.branches = vec!["main".into(), "feature/x".into()];
        a.handle_event(press(KeyCode::Char('c')));
        for ch in "feature/x".chars() {
            a.handle_event(press(KeyCode::Char(ch)));
        }
        // Enter without engaging the list submits the typed text.
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(
            effect,
            Effect::CheckoutBranch {
                worktree_index: 0,
                branch: "feature/x".into(),
            }
        );
    }

    #[test]
    fn checkout_picker_submits_highlighted_suggestion() {
        let mut a = app(&[("main", true)]);
        a.branches = vec!["main".into(), "feature/x".into(), "feature/y".into()];
        a.handle_event(press(KeyCode::Char('c')));
        for ch in "feature".chars() {
            a.handle_event(press(KeyCode::Char(ch)));
        }
        // Matches follow seeded order [feature/x, feature/y]; one Down lands on y.
        a.handle_event(press(KeyCode::Down));
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(
            effect,
            Effect::CheckoutBranch {
                worktree_index: 0,
                branch: "feature/y".into(),
            }
        );
    }

    #[test]
    fn checkout_picker_empty_query_errors() {
        let mut a = app(&[("main", true)]);
        a.handle_event(press(KeyCode::Char('c')));
        let effect = a.handle_event(press(KeyCode::Enter));
        assert_eq!(effect, Effect::None);
        if let Mode::Checkout(s) = &a.mode {
            assert!(s.error.is_some());
        } else {
            panic!("expected checkout mode (still open)");
        }
    }

    #[test]
    fn checkout_picker_escape_closes_dropdown_then_cancels() {
        let mut a = app(&[("main", true)]);
        a.branches = vec!["main".into()];
        a.handle_event(press(KeyCode::Char('c')));
        a.handle_event(press(KeyCode::Char('m'))); // dropdown open on entry; filters to "main"
        if let Mode::Checkout(s) = &a.mode {
            assert!(s.options.is_open());
        }
        a.handle_event(press(KeyCode::Esc)); // first Esc closes the dropdown
        if let Mode::Checkout(s) = &a.mode {
            assert!(!s.options.is_open());
        } else {
            panic!("expected checkout mode (still open)");
        }
        a.handle_event(press(KeyCode::Esc)); // second Esc cancels the modal
        assert_eq!(a.mode, Mode::List);
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
        // Shift-Tab steps back from the body to the title; Backspace pops it.
        a.handle_event(press(KeyCode::BackTab));
        a.handle_event(press(KeyCode::Backspace));
        if let Mode::PrCompose(s) = &a.mode {
            assert_eq!(s.field, ComposeField::Title);
            assert_eq!(s.title, "h");
        }
    }

    #[test]
    fn compose_tab_cycles_all_four_fields() {
        use crate::tui::app::PrComposeState;
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrCompose(PrComposeState::default());
        let field = |a: &App| {
            if let Mode::PrCompose(s) = &a.mode {
                s.field
            } else {
                panic!("expected compose mode")
            }
        };
        assert_eq!(field(&a), ComposeField::Title);
        a.handle_event(press(KeyCode::Tab));
        assert_eq!(field(&a), ComposeField::Body);
        a.handle_event(press(KeyCode::Tab));
        assert_eq!(field(&a), ComposeField::Model);
        a.handle_event(press(KeyCode::Tab));
        assert_eq!(field(&a), ComposeField::Effort);
        a.handle_event(press(KeyCode::Tab));
        assert_eq!(field(&a), ComposeField::Title); // wraps
    }

    #[test]
    fn compose_model_effort_fields_pick_with_arrows() {
        use crate::agent::{AgentModel, Effort};
        use crate::tui::app::PrComposeState;
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrCompose(PrComposeState::default());
        // Tab to the model field (title → body → model).
        a.handle_event(press(KeyCode::Tab));
        a.handle_event(press(KeyCode::Tab));
        // Down advances like next(); Up reverses like prev() (defaults: Sonnet).
        a.handle_event(press(KeyCode::Down));
        a.handle_event(press(KeyCode::Up));
        // Typing on an option field is ignored (no stray character lands).
        a.handle_event(press(KeyCode::Char('z')));
        if let Mode::PrCompose(s) = &a.mode {
            assert_eq!(s.field, ComposeField::Model);
            assert_eq!(s.model, AgentModel::Sonnet);
            assert_eq!(s.title, "");
        } else {
            panic!("expected compose mode");
        }
        // Tab to effort; Down advances it like next() (default: Medium).
        a.handle_event(press(KeyCode::Tab));
        a.handle_event(press(KeyCode::Down));
        if let Mode::PrCompose(s) = &a.mode {
            assert_eq!(s.field, ComposeField::Effort);
            assert_eq!(s.effort, Effort::Medium.next());
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
    fn compose_ctrl_a_triggers_ai_fill() {
        use crate::tui::app::PrComposeState;
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrCompose(PrComposeState::default());
        let effect = a.handle_event(ctrl('a'));
        assert_eq!(effect, Effect::DraftPrAi);
        if let Mode::PrCompose(s) = &a.mode {
            // The form enters the "working" state; Ctrl-A is not typed as 'a'.
            assert!(s.submitting);
            assert_eq!(s.title, "");
        } else {
            panic!("expected compose mode");
        }
    }

    #[test]
    fn compose_ctrl_m_and_e_cycle_model_and_effort() {
        use crate::agent::{AgentModel, Effort};
        use crate::tui::app::PrComposeState;
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrCompose(PrComposeState::default());
        // Defaults are Sonnet / Medium; cycling advances and never types a char.
        a.handle_event(ctrl('m'));
        a.handle_event(ctrl('e'));
        if let Mode::PrCompose(s) = &a.mode {
            assert_eq!(s.model, AgentModel::Sonnet.next());
            assert_eq!(s.effort, Effort::Medium.next());
            assert_eq!(s.title, "");
        } else {
            panic!("expected compose mode");
        }
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
    fn mouse_in_modal_does_not_touch_background() {
        // While a modal overlay is open the background list must not react to the
        // mouse (issue #70): neither a click nor a wheel scroll changes it.
        let mut a = app(&[("a", true), ("b", false), ("c", false)]);
        a.selected = 1;
        a.mode = Mode::Create(CreateState::default());
        let click = Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });
        assert_eq!(a.handle_event(click), Effect::None);
        assert_eq!(a.selected, 1);
        assert!(matches!(a.mode, Mode::Create(_)));
        a.handle_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 5,
            row: 3,
            modifiers: KeyModifiers::empty(),
        }));
        assert_eq!(a.selected, 1); // background selection untouched
    }

    #[test]
    fn mouse_scroll_moves_create_dropdown() {
        // The wheel drives the modal's own options dropdown instead of the
        // hidden background list (issue #70).
        let mut a = app(&[("a", true)]);
        let mut options = crate::tui::OptionList::new(vec![
            "main".into(),
            "origin/main".into(),
            "origin/dev".into(),
        ]);
        options.open();
        a.mode = Mode::Create(CreateState {
            options,
            ..Default::default()
        });
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            })
        };
        a.handle_event(wheel(MouseEventKind::ScrollDown));
        if let Mode::Create(s) = &a.mode {
            // Engaged the list and moved one row down.
            assert_eq!(s.options.selected(), Some("origin/main"));
        } else {
            panic!("expected create mode");
        }
        a.handle_event(wheel(MouseEventKind::ScrollUp));
        if let Mode::Create(s) = &a.mode {
            assert_eq!(s.options.selected(), Some("main"));
        }
    }

    #[test]
    fn mouse_scroll_moves_pr_picker_selection() {
        use crate::tui::app::{PrItem, PrPickerState};
        let pr = |number| PrItem {
            number,
            title: "t".into(),
            author: "a".into(),
            state: "open".into(),
            created_at: String::new(),
        };
        let mut a = app(&[("a", true)]);
        a.mode = Mode::PrPicker(PrPickerState {
            loading: false,
            prs: vec![pr(1), pr(2)],
            ..Default::default()
        });
        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::empty(),
            })
        };
        a.handle_event(wheel(MouseEventKind::ScrollDown));
        if let Mode::PrPicker(s) = &a.mode {
            assert_eq!(s.selected, 1);
        } else {
            panic!("expected pr picker");
        }
        // Clamps at the last row, then scrolls back up.
        a.handle_event(wheel(MouseEventKind::ScrollDown));
        if let Mode::PrPicker(s) = &a.mode {
            assert_eq!(s.selected, 1);
        }
        a.handle_event(wheel(MouseEventKind::ScrollUp));
        if let Mode::PrPicker(s) = &a.mode {
            assert_eq!(s.selected, 0);
        }
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
