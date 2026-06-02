//! TUI view-model: the [`App`] state and the modal substates (spec §10).
//!
//! All state lives here; [`crate::tui::event`] drives transitions purely (no
//! terminal I/O), which is what makes the TUI testable.

use std::path::PathBuf;

use crate::keys::Keymap;
use crate::model::{Column, SortKey, SortSpec, Worktree};
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
    /// Confirm-remove dialog (the worktree index).
    ConfirmRemove(usize),
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
            quit: false,
            chosen: None,
        }
    }

    /// Sets the transient status-bar message and its severity (for coloring).
    pub fn set_status(&mut self, message: impl Into<String>, kind: StatusKind) {
        self.status_message = Some(message.into());
        self.status_kind = kind;
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
    fn select_path(&mut self, path: &std::path::Path) {
        if let Some(pos) = self
            .visible
            .iter()
            .position(|&i| self.worktrees[i].path == path)
        {
            self.selected = pos;
        }
    }
}

/// The fuzzy-filter haystack for a worktree: branch + slug + path.
fn haystack(worktree: &Worktree) -> String {
    format!(
        "{} {} {}",
        worktree.branch.as_deref().unwrap_or(""),
        worktree.slug.as_deref().unwrap_or(""),
        worktree.path.display()
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
    fn select_row_within_bounds() {
        let mut a = app(&[("a", true), ("b", false)]);
        a.select_row(1);
        assert_eq!(a.selected, 1);
        a.select_row(99); // out of bounds -> no change
        assert_eq!(a.selected, 1);
    }
}
