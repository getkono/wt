//! TUI view-model: the [`App`] state and the modal substates (spec §10).
//!
//! All state lives here; [`crate::tui::event`] drives transitions purely (no
//! terminal I/O), which is what makes the TUI testable.

use std::path::PathBuf;

use crate::agent::{AgentModel, Effort};
use crate::keys::Keymap;
use crate::model::{Column, SortKey, SortSpec, Worktree};
use crate::tui::event::Effect;
use crate::tui::options::OptionList;
use crate::tui::theme::Palette;
use crate::util::fuzzy;

/// The narrowest terminal width at which the detail pane is shown (spec §10).
pub const MIN_DETAIL_WIDTH: u16 = 60;
/// The terminal height below which the TUI exits cleanly (spec §10).
pub const MIN_HEIGHT: u16 = 5;

/// The interaction mode (spec §10 "View modes").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// The default worktree list.
    List,
    /// Fuzzy-filter overlay.
    Filter,
    /// Create-worktree prompt.
    Create(CreateState),
    /// PR picker overlay.
    PrPicker(PrPickerState),
    /// PR compose form (`wt pr open`): edit a title + body, then submit.
    PrCompose(PrComposeState),
    /// Branch picker for checking out a branch in the selected worktree.
    Checkout(CheckoutState),
    /// Confirm-remove dialog (the worktree index).
    ConfirmRemove(usize),
    /// Confirm creating a worktree for the worktree-less branch row at the given
    /// index, then switching into it (issue #47).
    ConfirmCreate(usize),
    /// Confirm-delete dialog for the worktree-less branch row at `index` (issue
    /// #53). A branch row has no worktree to remove, so Remove deletes its local
    /// branch instead. `force` is set on the second prompt, after a safe
    /// `git branch -d` refused an unmerged branch, to offer a `git branch -D`.
    ConfirmDeleteBranch {
        /// Index into [`App::worktrees`] of the branch row to delete.
        index: usize,
        /// Whether this is the force-delete (`-D`) re-prompt for an unmerged branch.
        force: bool,
    },
    /// Confirm dialog shown when the base a new worktree would fork from is behind
    /// its origin counterpart (issue #56): update the base, proceed as-is, or cancel.
    ConfirmStaleBase(StaleBaseState),
    /// Confirm dialog shown after a worktree is created with uninitialized
    /// submodules and the `[submodules] init` policy is left at its `prompt`
    /// default (issue #50): initialize them recursively, or leave them. Defaults
    /// to yes.
    ConfirmInitSubmodules(InitSubmodulesState),
    /// Confirm dialog shown when the user quits while background jobs are still
    /// running: quit anyway (abandoning them) or cancel. Carries how many jobs
    /// were in flight, for the prompt text.
    ConfirmQuit {
        /// The number of background jobs running when the quit was requested.
        jobs: usize,
    },
    /// Help overlay.
    Help,
}

/// Which pane has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The worktree list (left).
    List,
    /// The detail pane (right).
    Detail,
}

/// The severity of a transient status-bar message, used to color it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StatusKind {
    /// A neutral, uncolored message.
    #[default]
    Info,
    /// A successful action (e.g. "created feature/x").
    Success,
    /// A failed action (e.g. a git error).
    Error,
}

/// Identifies the target of a background job so its per-row spinner can be found
/// and so a second action on the same target can be refused (issue #46 overhaul).
/// Keyed by the row's stable identity (path or branch name) so it survives a
/// re-sort/refresh, mirroring [`App::loaded_paths`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobKey {
    /// A job targeting the worktree at this path (remove, sync, checkout, submodule
    /// init).
    Path(PathBuf),
    /// A job targeting the worktree-less branch row with this name (delete branch,
    /// materialize, branch-row sync).
    Branch(String),
    /// A job with no existing row yet (creating a brand-new worktree, or checking
    /// out a PR into a new branch): it has nothing to attach a per-row spinner to,
    /// so it shows only in the status-bar summary.
    New(String),
}

/// An in-flight background action (issue #46 overhaul). Multiple jobs run
/// concurrently, each attached to its target row via [`JobKey`]; the shared
/// [`App::spinner_frame`] animates every row spinner in sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveJob {
    /// The target this job acts on.
    pub key: JobKey,
    /// The human-facing label, e.g. `Removing feat/foo` or `Initializing submodules`.
    pub label: String,
}

/// The interaction context a finished job is allowed to drive a mode change from,
/// so a background job never clobbers an unrelated modal the user opened while it
/// ran (issue #46 overhaul). A job may transition the mode only when the user is
/// idle (List/Filter) or still in the job's own single-instance modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobHome {
    /// The job's confirm dialog already closed to the list before it began, so it
    /// may act only when the user is idle.
    List,
    /// The create modal stays open (submitting) during a create job.
    Create,
    /// The checkout picker stays open during an in-place checkout.
    Checkout,
    /// The PR picker stays open during a PR checkout.
    PrPicker,
}

/// The create-worktree prompt state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateState {
    /// Which field is being edited.
    pub step: CreateStep,
    /// The entered branch name.
    pub branch: String,
    /// The entered base ref.
    pub base: String,
    /// An inline error from a failed submission.
    pub error: Option<String>,
    /// The inline branch-options dropdown for the active field (issue #25):
    /// existing local + remote branches to fork from or check out.
    pub options: OptionList,
}

/// The stale-base confirm state (issue #56): the base a new worktree would fork
/// from is behind its upstream. Carries the pending create's inputs so the
/// user's choice (update / proceed) can re-issue it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleBaseState {
    /// The new branch name being created.
    pub branch: String,
    /// The base ref the user entered (or `None` for the default).
    pub base: Option<String>,
    /// How many commits the base is behind its upstream.
    pub behind: u32,
    /// The upstream display name, e.g. `origin/main`.
    pub upstream_display: String,
    /// Whether the base can be fast-forwarded (no local-only commits); when
    /// false, updating will fail and only proceed/cancel make sense.
    pub can_fast_forward: bool,
}

/// The submodule-init confirm state (issue #50): a freshly created worktree has
/// uninitialized submodules and the policy is left at its `prompt` default.
/// Carries the new worktree directory (where the init runs) and what to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSubmodulesState {
    /// The new worktree directory whose submodules would be initialized.
    pub dir: PathBuf,
    /// The branch the worktree was created for (for the status text).
    pub branch: String,
    /// How many uninitialized submodules were detected.
    pub count: usize,
}

/// Which create-prompt field is active.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CreateStep {
    /// Editing the branch name.
    #[default]
    Branch,
    /// Editing the base ref.
    Base,
}

/// The checkout-branch picker state: a type-ahead branch list plus the target
/// worktree to switch in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckoutState {
    /// Index into [`App::worktrees`] of the target worktree (the selected row).
    pub worktree_index: usize,
    /// The type-ahead query (the branch the user is filtering/typing).
    pub query: String,
    /// The inline branch-options dropdown (local + remote branches to check out).
    pub options: OptionList,
    /// An inline error from a failed checkout (e.g. a dirty worktree).
    pub error: Option<String>,
    /// Whether a checkout is in flight (input is ignored while set).
    pub submitting: bool,
}

/// One PR shown in the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrItem {
    /// PR number.
    pub number: u64,
    /// PR title.
    pub title: String,
    /// PR author login.
    pub author: String,
    /// PR state label.
    pub state: String,
    /// ISO-8601 creation time, used to render a relative age.
    pub created_at: String,
}

/// Which PR-compose field is active.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ComposeField {
    /// Editing the single-line title.
    #[default]
    Title,
    /// Editing the multi-line body.
    Body,
    /// Selecting the AI auto-fill model from its options dropdown (issue #25).
    Model,
    /// Selecting the AI auto-fill effort from its options dropdown (issue #25).
    Effort,
}

/// The `wt pr open` compose-form state: a title and (multi-line) body the user
/// edits before submitting, plus the precomputed header context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrComposeState {
    /// Which field is being edited.
    pub field: ComposeField,
    /// The PR title (single line).
    pub title: String,
    /// The PR body (may contain newlines).
    pub body: String,
    /// Whether to open the PR as a draft (create only).
    pub draft: bool,
    /// The current branch (for the header).
    pub branch: String,
    /// The base/trunk branch (for the header).
    pub trunk: String,
    /// Precomputed action label, e.g. `create` or `update #12`.
    pub action_label: String,
    /// The model used for AI auto-fill (`Ctrl-A`), cycled with `Ctrl-M`.
    pub model: AgentModel,
    /// The effort used for AI auto-fill, cycled with `Ctrl-E`.
    pub effort: Effort,
    /// Whether a submit/draft operation is in flight (shown as a hint).
    pub submitting: bool,
    /// An inline error from a failed draft or submission.
    pub error: Option<String>,
}

/// The PR-picker state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrPickerState {
    /// Whether PRs are still loading.
    pub loading: bool,
    /// The loaded PRs.
    pub prs: Vec<PrItem>,
    /// The selected PR index.
    pub selected: usize,
    /// An error (e.g. gh unavailable).
    pub error: Option<String>,
}

/// The TUI application state.
pub struct App {
    /// All worktrees (sorted).
    pub worktrees: Vec<Worktree>,
    /// Indices into `worktrees` currently visible (after filtering).
    pub visible: Vec<usize>,
    /// Selected index into `visible`.
    pub selected: usize,
    /// The current mode.
    pub mode: Mode,
    /// The active filter string.
    pub filter: String,
    /// Which pane has focus.
    pub focus: Pane,
    /// Whether the list (sidebar) pane is shown.
    pub show_sidebar: bool,
    /// The list pane width.
    pub sidebar_width: u16,
    /// The current sort.
    pub sort: SortSpec,
    /// Scroll offset of the detail pane.
    pub detail_scroll: u16,
    /// Terminal size (cols, rows).
    pub size: (u16, u16),
    /// The key bindings.
    pub keymap: Keymap,
    /// Columns to render in the list.
    pub columns: Vec<Column>,
    /// Whether untracked files show `?`.
    pub show_untracked: bool,
    /// Whether untracked-only files count as "dirty" for the remove guard
    /// (the confirm dialog mirrors `remove.untracked_blocks`, not `show_untracked`).
    pub remove_untracked_blocks: bool,
    /// Whether Nerd Font glyphs are enabled.
    pub nerd_fonts: bool,
    /// Whether mouse support is enabled.
    pub mouse: bool,
    /// Whether color output is enabled (spec §11 precedence, resolved once).
    pub color: bool,
    /// The resolved color palette (preset + `[ui.theme]` overrides).
    pub palette: Palette,
    /// Set when the user quits without switching.
    pub quit: bool,
    /// Set to the chosen path when the user switches (Enter).
    pub chosen: Option<PathBuf>,
    /// Worktree paths whose async fields have loaded; rows not in this set show
    /// the per-row spinner (spec §10). Keyed by path so it survives re-sorting.
    loaded_paths: std::collections::HashSet<PathBuf>,
    /// A transient status/error line shown in the status bar.
    pub status_message: Option<String>,
    /// The severity of `status_message`, used to color it.
    pub status_kind: StatusKind,
    /// Set when the terminal became too small to continue (spec §10).
    pub too_small: bool,
    /// The in-flight background actions (issue #46 overhaul): each runs on its own
    /// task and shows a per-row spinner; input is never gated. Empty when idle.
    pub jobs: Vec<ActiveJob>,
    /// The shared spinner animation frame, advanced on each tick while any job is
    /// in flight; every per-row job spinner reads it so they animate in sync.
    pub spinner_frame: usize,
    /// Follow-up background actions queued by a just-applied job outcome (e.g. a
    /// created worktree with uninitialized submodules under the `always` policy),
    /// drained by the event loop after `apply_outcome` and spawned as their own
    /// jobs. Kept off the render path.
    pub pending_jobs: Vec<Effect>,
    /// Local + remote-tracking branch names offered in the create-prompt
    /// options dropdown and used to tab-complete the base ref (best-effort;
    /// empty when enumeration fails).
    pub branches: Vec<String>,
    /// The remote-tracking default branch (e.g. `origin/main`) a new worktree
    /// forks from by default, pre-filled into the create-prompt base field
    /// (issue #70). `None` when there is no confident remote default (no
    /// `origin/HEAD`), in which case the base starts empty.
    pub default_base: Option<String>,
}

/// Display/config inputs for the TUI (the parts of [`crate::config::Config`]
/// the view needs), bundled to keep [`App::new`] tidy.
pub struct AppConfig {
    /// The effective key bindings.
    pub keymap: Keymap,
    /// The initial sort.
    pub sort: SortSpec,
    /// Columns to render in the list.
    pub columns: Vec<Column>,
    /// Whether untracked files show `?`.
    pub show_untracked: bool,
    /// Whether untracked-only files count as "dirty" for the remove guard.
    pub remove_untracked_blocks: bool,
    /// Whether Nerd Font glyphs are enabled.
    pub nerd_fonts: bool,
    /// Whether mouse support is enabled.
    pub mouse: bool,
    /// Whether color output is enabled (spec §11 precedence, resolved once).
    pub color: bool,
    /// The resolved color palette (preset + `[ui.theme]` overrides).
    pub palette: Palette,
}

impl App {
    /// Builds an app over the given worktrees, selecting the current one. All
    /// rows start marked loaded; the runtime marks them loading before async
    /// enrichment.
    pub fn new(worktrees: Vec<Worktree>, config: AppConfig, size: (u16, u16)) -> App {
        let visible = (0..worktrees.len()).collect();
        let selected = worktrees.iter().position(|w| w.is_current).unwrap_or(0);
        let loaded_paths = worktrees.iter().map(|w| w.path.clone()).collect();
        App {
            loaded_paths,
            status_message: None,
            status_kind: StatusKind::Info,
            too_small: false,
            jobs: Vec::new(),
            spinner_frame: 0,
            pending_jobs: Vec::new(),
            branches: Vec::new(),
            default_base: None,
            worktrees,
            visible,
            selected,
            mode: Mode::List,
            filter: String::new(),
            focus: Pane::List,
            show_sidebar: true,
            sidebar_width: 40,
            sort: config.sort,
            detail_scroll: 0,
            size,
            keymap: config.keymap,
            columns: config.columns,
            show_untracked: config.show_untracked,
            remove_untracked_blocks: config.remove_untracked_blocks,
            nerd_fonts: config.nerd_fonts,
            mouse: config.mouse,
            color: config.color,
            palette: config.palette,
            quit: false,
            chosen: None,
        }
    }

    /// Sets the transient status-bar message and its severity (for coloring).
    pub fn set_status(&mut self, message: impl Into<String>, kind: StatusKind) {
        self.status_message = Some(message.into());
        self.status_kind = kind;
    }

    /// Registers a background job on `key` with a display label (issue #46
    /// overhaul). If a job already targets `key` it is replaced (the caller
    /// guards against conflicts first via [`App::has_job`]).
    pub fn begin_job(&mut self, key: JobKey, label: impl Into<String>) {
        self.jobs.retain(|j| j.key != key);
        self.jobs.push(ActiveJob {
            key,
            label: label.into(),
        });
    }

    /// Removes the job targeting `key` once it completes; a no-op if absent.
    pub fn finish_job(&mut self, key: &JobKey) {
        self.jobs.retain(|j| &j.key != key);
    }

    /// Whether a background job already targets `key` (used to refuse a second,
    /// conflicting action on the same row).
    pub fn has_job(&self, key: &JobKey) -> bool {
        self.jobs.iter().any(|j| &j.key == key)
    }

    /// The active job attached to `worktree`'s row, if any, so the list can render
    /// its per-row spinner and label. Matches a `Path` job by path and a `Branch`
    /// job by the branch-row's name; `New` jobs attach to no row.
    pub fn job_for(&self, worktree: &Worktree) -> Option<&ActiveJob> {
        self.jobs.iter().find(|j| match &j.key {
            JobKey::Path(p) => worktree.has_worktree && &worktree.path == p,
            JobKey::Branch(b) => {
                !worktree.has_worktree && worktree.branch.as_deref() == Some(b.as_str())
            }
            JobKey::New(_) => false,
        })
    }

    /// Advances the shared spinner one frame (called on each animation tick); a
    /// no-op when no job is in flight.
    pub fn tick_spinner(&mut self) {
        if !self.jobs.is_empty() {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
        }
    }

    /// Whether any background job is in flight (keeps the animation ticker awake).
    pub fn any_jobs(&self) -> bool {
        !self.jobs.is_empty()
    }

    /// Whether the event loop should exit after a background job just finished.
    /// The single break predicate the loop consults so every job-driven exit
    /// funnels through one place (issue #46 overhaul): today that is only a job
    /// that recorded a path to switch into.
    pub fn exit_now(&mut self) -> bool {
        self.chosen.is_some()
    }

    /// A compact status-bar summary of the in-flight jobs — the count and the
    /// first job's label — so background work stays visible even when its row is
    /// scrolled off. `None` when idle.
    pub fn job_summary(&self) -> Option<String> {
        let first = self.jobs.first()?;
        Some(if self.jobs.len() == 1 {
            format!("{}…", first.label)
        } else {
            format!("{} (+{} more)…", first.label, self.jobs.len() - 1)
        })
    }

    /// Whether a finished job in `home` context may drive a mode change without
    /// clobbering an unrelated modal the user opened while it ran (issue #46
    /// overhaul): true when the user is idle or still in the job's own modal.
    pub fn may_apply_mode(&self, home: JobHome) -> bool {
        matches!(self.mode, Mode::List | Mode::Filter)
            || match home {
                JobHome::List => false,
                JobHome::Create => matches!(self.mode, Mode::Create(_)),
                JobHome::Checkout => matches!(self.mode, Mode::Checkout(_)),
                JobHome::PrPicker => matches!(self.mode, Mode::PrPicker(_)),
            }
    }

    /// Queues a follow-up background action for the loop to spawn after the
    /// current outcome is applied (e.g. submodule init after a create).
    pub fn queue_job(&mut self, effect: Effect) {
        self.pending_jobs.push(effect);
    }

    /// Drains the queued follow-up actions (issue #46 overhaul).
    pub fn take_pending_jobs(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.pending_jobs)
    }

    /// The currently selected worktree, if any.
    pub fn selected_worktree(&self) -> Option<&Worktree> {
        self.visible
            .get(self.selected)
            .and_then(|&i| self.worktrees.get(i))
    }

    /// Whether a worktree's async fields have loaded (else it shows a spinner).
    pub fn is_loaded(&self, worktree: &Worktree) -> bool {
        self.loaded_paths.contains(&worktree.path)
    }

    /// Marks all rows as loading (clears the loaded set), for the initial render.
    pub fn mark_loading(&mut self) {
        self.loaded_paths.clear();
    }

    /// Marks a worktree's path as loaded.
    pub fn mark_loaded(&mut self, path: PathBuf) {
        self.loaded_paths.insert(path);
    }

    /// Whether the detail pane is visible at the current size.
    pub fn detail_visible(&self) -> bool {
        !self.show_sidebar || self.size.0 >= MIN_DETAIL_WIDTH
    }

    /// Replaces the worktrees (e.g. after a refresh), preserving the selection by
    /// path and re-applying the sort and filter.
    pub fn set_worktrees(&mut self, worktrees: Vec<Worktree>) {
        let selected_path = self.selected_worktree().map(|w| w.path.clone());
        self.worktrees = worktrees;
        self.apply_sort();
        self.recompute_visible();
        if let Some(path) = selected_path {
            self.select_path(&path);
        }
    }

    /// Moves the selection by `delta`, clamped to the visible range. Changing
    /// the selection resets the detail-pane scroll.
    pub fn move_selection(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let max = self.visible.len() as isize - 1;
        let next = (self.selected as isize + delta).clamp(0, max);
        self.selected = next as usize;
        self.detail_scroll = 0;
    }

    /// Selects the first / last visible row.
    pub fn select_edge(&mut self, last: bool) {
        if self.visible.is_empty() {
            return;
        }
        self.selected = if last { self.visible.len() - 1 } else { 0 };
        self.detail_scroll = 0;
    }

    /// Selects the visible row at display position `row`, if any.
    pub fn select_row(&mut self, row: usize) {
        if row < self.visible.len() {
            self.selected = row;
            self.detail_scroll = 0;
        }
    }

    /// Scrolls the detail pane by `delta` lines (spec §10), clamped to roughly
    /// the selected worktree's detail content so it cannot scroll into the void.
    pub fn scroll_detail(&mut self, delta: isize) {
        let max = self.selected_worktree().map_or(0, |w| {
            // path/branch/base/status + the commit block + the PR block.
            (w.recent_commits.len() + 10) as isize
        });
        let next = (self.detail_scroll as isize + delta).clamp(0, max.max(0));
        self.detail_scroll = next as u16;
    }

    /// Cycles the sort field (spec §10 sort-cycle).
    pub fn cycle_sort(&mut self) {
        const ORDER: [SortKey; 6] = [
            SortKey::Branch,
            SortKey::Dirty,
            SortKey::Ahead,
            SortKey::Behind,
            SortKey::Activity,
            SortKey::Path,
        ];
        let current = ORDER.iter().position(|k| *k == self.sort.key).unwrap_or(0);
        self.sort.key = ORDER[(current + 1) % ORDER.len()];
        self.resort_preserving_selection();
    }

    /// Toggles the sort direction (spec §10 sort-reverse).
    pub fn reverse_sort(&mut self) {
        self.sort.descending = !self.sort.descending;
        self.resort_preserving_selection();
    }

    /// Appends a character to the filter and recomputes the visible set.
    pub fn filter_push(&mut self, c: char) {
        self.filter.push(c);
        self.recompute_visible();
    }

    /// Removes the last filter character.
    pub fn filter_pop(&mut self) {
        self.filter.pop();
        self.recompute_visible();
    }

    /// Clears the filter.
    pub fn clear_filter(&mut self) {
        self.filter.clear();
        self.recompute_visible();
    }

    /// Replaces the filter wholesale and recomputes the visible set, resetting
    /// the selection to the first match. Used to seed the picker with a query
    /// (e.g. the ambiguous-query fallback opens pre-filtered to that query).
    pub(crate) fn apply_filter(&mut self, filter: String) {
        self.filter = filter;
        self.selected = 0;
        self.recompute_visible();
    }

    /// Re-sorts worktrees and rebuilds the visible set, keeping the selection.
    fn resort_preserving_selection(&mut self) {
        let selected_path = self.selected_worktree().map(|w| w.path.clone());
        self.apply_sort();
        self.recompute_visible();
        if let Some(path) = selected_path {
            self.select_path(&path);
        }
    }

    /// Sorts `worktrees` by the current spec, keeping the base (primary)
    /// worktree pinned first (issue #4).
    fn apply_sort(&mut self) {
        crate::worktree_service::sort_worktrees_base_first(&mut self.worktrees, self.sort);
    }

    /// Recomputes `visible` from the filter, clamping the selection.
    fn recompute_visible(&mut self) {
        if self.filter.is_empty() {
            self.visible = (0..self.worktrees.len()).collect();
        } else {
            let haystacks: Vec<String> = self.worktrees.iter().map(haystack).collect();
            let matched = fuzzy::filter_indices(&haystacks, &self.filter);
            // Keep worktree order rather than fuzzy-score order for stability.
            let keep: std::collections::HashSet<usize> = matched.into_iter().collect();
            self.visible = (0..self.worktrees.len())
                .filter(|i| keep.contains(i))
                .collect();
        }
        if self.selected >= self.visible.len() {
            self.selected = self.visible.len().saturating_sub(1);
        }
    }

    /// Selects the visible row whose worktree path matches `path`.
    pub fn select_path(&mut self, path: &std::path::Path) {
        if let Some(pos) = self
            .visible
            .iter()
            .position(|&i| self.worktrees[i].path == path)
        {
            self.selected = pos;
        }
    }

    /// Selects the visible row for the real worktree on `branch`, if present.
    /// Returns whether a matching visible row was found — `false` when the row is
    /// filtered out or absent, leaving the selection unchanged. Used to focus a
    /// freshly created worktree (issue #52).
    pub fn select_branch(&mut self, branch: &str) -> bool {
        let Some(pos) = self.visible.iter().position(|&i| {
            let w = &self.worktrees[i];
            w.has_worktree && w.branch.as_deref() == Some(branch)
        }) else {
            return false;
        };
        self.selected = pos;
        self.detail_scroll = 0;
        true
    }
}

/// The fuzzy-filter haystack for a worktree: branch + slug + path. A
/// worktree-less branch row has only a virtual path (issue #47), so it matches on
/// branch + slug alone.
fn haystack(worktree: &Worktree) -> String {
    let path = if worktree.has_worktree {
        worktree.path.display().to_string()
    } else {
        String::new()
    };
    format!(
        "{} {} {}",
        worktree.branch.as_deref().unwrap_or(""),
        worktree.slug.as_deref().unwrap_or(""),
        path
    )
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::path::PathBuf;

    /// Builds a worktree with a branch for tests.
    pub(crate) fn wt(branch: &str, current: bool) -> Worktree {
        let mut w = Worktree::new(PathBuf::from(format!("/r/{branch}")));
        w.branch = Some(branch.to_string());
        w.slug = Some(branch.replace('/', "-"));
        w.is_current = current;
        w
    }

    /// Builds a worktree-less branch row for tests (issue #47).
    pub(crate) fn branch_row(branch: &str) -> Worktree {
        let mut w = Worktree::new(PathBuf::from(format!("branch://{branch}")));
        w.branch = Some(branch.to_string());
        w.slug = Some(branch.replace('/', "-"));
        w.has_worktree = false;
        w
    }

    /// Builds an app over the given branches.
    pub(crate) fn app(branches: &[(&str, bool)]) -> App {
        let worktrees: Vec<Worktree> = branches.iter().map(|(b, c)| wt(b, *c)).collect();
        App::new(
            worktrees,
            AppConfig {
                keymap: Keymap::defaults(),
                sort: SortSpec::default(),
                columns: Column::ALL.to_vec(),
                show_untracked: true,
                remove_untracked_blocks: false,
                nerd_fonts: false,
                mouse: true,
                color: true,
                palette: Palette::one_dark(),
            },
            (100, 30),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::app;
    use super::*;

    #[test]
    fn selects_current_worktree_initially() {
        let a = app(&[("main", false), ("feature", true)]);
        assert_eq!(
            a.selected_worktree().unwrap().branch.as_deref(),
            Some("feature")
        );
    }

    #[test]
    fn navigation_clamps() {
        let mut a = app(&[("a", true), ("b", false), ("c", false)]);
        a.selected = 0;
        a.move_selection(-1);
        assert_eq!(a.selected, 0);
        a.move_selection(5);
        assert_eq!(a.selected, 2);
        a.select_edge(false);
        assert_eq!(a.selected, 0);
        a.select_edge(true);
        assert_eq!(a.selected, 2);
    }

    #[test]
    fn filter_narrows_and_clamps_selection() {
        let mut a = app(&[("alpha", true), ("beta", false), ("alphabet", false)]);
        a.selected = 2;
        a.filter_push('a');
        a.filter_push('l');
        a.filter_push('p');
        // Only alpha + alphabet match.
        assert_eq!(a.visible.len(), 2);
        assert!(a.selected < a.visible.len());
        a.clear_filter();
        assert_eq!(a.visible.len(), 3);
    }

    #[test]
    fn apply_filter_seeds_filter_and_resets_selection() {
        let mut a = app(&[("alpha", true), ("beta", false), ("alphabet", false)]);
        a.selected = 2;
        a.apply_filter("alph".to_string());
        assert_eq!(a.filter, "alph");
        // Only alpha + alphabet match; selection resets to the first match.
        assert_eq!(a.visible.len(), 2);
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn sort_preserves_selection_by_path() {
        let mut a = app(&[("zebra", false), ("alpha", true), ("mango", false)]);
        // Sort by branch ascending.
        a.sort = SortSpec {
            key: SortKey::Branch,
            descending: false,
        };
        a.resort_preserving_selection();
        // The current worktree (alpha) is still selected.
        assert_eq!(
            a.selected_worktree().unwrap().branch.as_deref(),
            Some("alpha")
        );
    }

    #[test]
    fn base_worktree_stays_first_after_sort() {
        let mut a = app(&[("zebra", false), ("main", true), ("alpha", false)]);
        // Mark "main" as the primary (base) worktree.
        let base = a
            .worktrees
            .iter()
            .position(|w| w.branch.as_deref() == Some("main"))
            .unwrap();
        a.worktrees[base].is_main = true;
        a.sort = SortSpec {
            key: SortKey::Branch,
            descending: false,
        };
        a.resort_preserving_selection();
        // The base is pinned first; the rest follow in sorted order.
        let order: Vec<&str> = a
            .visible
            .iter()
            .map(|&i| a.worktrees[i].branch.as_deref().unwrap())
            .collect();
        assert_eq!(order, vec!["main", "alpha", "zebra"]);
        // The current worktree (main) remains selected after the resort.
        assert_eq!(
            a.selected_worktree().unwrap().branch.as_deref(),
            Some("main")
        );
    }

    #[test]
    fn cycle_sort_advances_field() {
        let mut a = app(&[("a", true)]);
        assert_eq!(a.sort.key, SortKey::Branch);
        a.cycle_sort();
        assert_eq!(a.sort.key, SortKey::Dirty);
        a.reverse_sort();
        assert!(a.sort.descending);
    }

    #[test]
    fn detail_visible_respects_width() {
        let mut a = app(&[("a", true)]);
        a.size = (100, 30);
        assert!(a.detail_visible());
        a.size = (50, 30); // < 60 cols
        assert!(!a.detail_visible());
        a.show_sidebar = false; // full-screen detail
        assert!(a.detail_visible());
    }

    #[test]
    fn branch_rows_sort_below_worktrees_and_filter_by_name() {
        use super::testutil::branch_row;
        let mut a = app(&[("main", true), ("zebra", false)]);
        a.worktrees.push(branch_row("feature/lonely"));
        // A resort groups branch rows below the worktrees (issue #47).
        a.resort_preserving_selection();
        let order: Vec<&str> = a
            .visible
            .iter()
            .map(|&i| a.worktrees[i].branch.as_deref().unwrap())
            .collect();
        assert_eq!(order, vec!["main", "zebra", "feature/lonely"]);
        // The branch row matches on its name even though its path is virtual.
        a.apply_filter("lonely".into());
        assert_eq!(a.visible.len(), 1);
        assert_eq!(
            a.selected_worktree().unwrap().branch.as_deref(),
            Some("feature/lonely")
        );
    }

    #[test]
    fn select_row_within_bounds() {
        let mut a = app(&[("a", true), ("b", false)]);
        a.select_row(1);
        assert_eq!(a.selected, 1);
        a.select_row(99); // out of bounds -> no change
        assert_eq!(a.selected, 1);
    }

    #[test]
    fn select_branch_focuses_match() {
        let mut a = app(&[("main", true), ("feature/x", false), ("other", false)]);
        a.selected = 0;
        assert!(a.select_branch("feature/x"));
        assert_eq!(
            a.selected_worktree().unwrap().branch.as_deref(),
            Some("feature/x")
        );
    }

    #[test]
    fn select_branch_misses_leave_selection_unchanged() {
        let mut a = app(&[("alpha", true), ("beta", false)]);
        a.selected = 1;
        // A branch that exists but is filtered out of the visible set.
        a.apply_filter("alph".into());
        a.selected = 0;
        assert!(!a.select_branch("beta"));
        assert_eq!(a.selected, 0);
        // A branch that is not present at all.
        assert!(!a.select_branch("ghost"));
        assert_eq!(a.selected, 0);
    }

    #[test]
    fn select_branch_ignores_worktree_less_branch_rows() {
        use super::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        a.worktrees.push(branch_row("topic"));
        a.apply_filter(String::new()); // include the branch row in `visible`
        a.selected = 0;
        // A worktree-less branch row is not a created worktree to focus.
        assert!(!a.select_branch("topic"));
    }

    #[test]
    fn job_registry_begin_finish_and_query() {
        let mut a = app(&[("main", true), ("feat", false)]);
        assert!(!a.any_jobs());
        let key = JobKey::Path(PathBuf::from("/r/feat"));
        a.begin_job(key.clone(), "Removing feat");
        assert!(a.any_jobs());
        assert!(a.has_job(&key));
        assert_eq!(a.job_summary().as_deref(), Some("Removing feat…"));
        // The job attaches to the matching worktree row (by path).
        let feat = a
            .worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some("feat"));
        assert_eq!(a.job_for(feat.unwrap()).unwrap().label, "Removing feat");
        // Re-registering the same key replaces rather than duplicates.
        a.begin_job(key.clone(), "Removing feat again");
        assert_eq!(a.jobs.len(), 1);
        a.finish_job(&key);
        assert!(!a.any_jobs());
        assert!(a.job_summary().is_none());
    }

    #[test]
    fn job_summary_counts_multiple() {
        let mut a = app(&[("main", true)]);
        a.begin_job(JobKey::New("feat/a".into()), "Creating feat/a");
        a.begin_job(JobKey::Branch("feat/b".into()), "Deleting branch feat/b");
        let summary = a.job_summary().unwrap();
        assert!(summary.contains("+1 more"));
    }

    #[test]
    fn branch_job_attaches_to_branch_row_only() {
        use super::testutil::branch_row;
        let mut a = app(&[("main", true)]);
        a.worktrees.push(branch_row("topic"));
        a.begin_job(JobKey::Branch("topic".into()), "Deleting branch topic");
        let row = a
            .worktrees
            .iter()
            .find(|w| !w.has_worktree && w.branch.as_deref() == Some("topic"))
            .unwrap();
        assert!(a.job_for(row).is_some());
        // A `New` job attaches to no existing row (status-bar only).
        a.begin_job(JobKey::New("brand-new".into()), "Creating brand-new");
        assert!(a.job_for(&a.worktrees[0]).is_none());
    }

    #[test]
    fn tick_spinner_advances_only_with_jobs() {
        let mut a = app(&[("a", true)]);
        a.tick_spinner();
        assert_eq!(a.spinner_frame, 0); // idle: no advance
        a.begin_job(JobKey::New("x".into()), "Creating x");
        a.tick_spinner();
        a.tick_spinner();
        assert_eq!(a.spinner_frame, 2);
    }

    #[test]
    fn may_apply_mode_guards_against_unrelated_modals() {
        let mut a = app(&[("a", true)]);
        // Idle: any job may transition.
        assert!(a.may_apply_mode(JobHome::List));
        assert!(a.may_apply_mode(JobHome::Create));
        // In an unrelated confirm modal, a List-home job must not touch the mode.
        a.mode = Mode::ConfirmRemove(0);
        assert!(!a.may_apply_mode(JobHome::List));
        // A checkout job may still finish into its own open picker.
        a.mode = Mode::Checkout(Default::default());
        assert!(a.may_apply_mode(JobHome::Checkout));
        assert!(!a.may_apply_mode(JobHome::Create));
    }

    #[test]
    fn pending_jobs_queue_and_drain() {
        let mut a = app(&[("a", true)]);
        assert!(a.take_pending_jobs().is_empty());
        a.queue_job(Effect::InitSubmodules {
            dir: PathBuf::from("/wt/x"),
            count: 2,
        });
        let drained = a.take_pending_jobs();
        assert_eq!(drained.len(), 1);
        assert!(a.take_pending_jobs().is_empty());
    }
}
