//! A reusable inline option list (the "dropdown" of selectable values shown on a
//! pop-up field, issue #25).
//!
//! [`OptionList`] is pure interaction state: the owning modal seeds it with the
//! candidate labels, drives it from key events (filter while typing, navigate
//! with `↑/↓`, accept with `Enter`), and the view renders the matches with the
//! cursor row highlighted. It powers both the create-worktree branch/base
//! type-ahead and the PR-compose model/effort pickers.
//!
//! Two usage shapes share one widget:
//! - *Type-ahead* (text-backed fields): [`refilter`](OptionList::refilter) on
//!   every keystroke narrows the matches and clears the active flag, so
//!   [`selected`](OptionList::selected) only yields a value once the user has
//!   moved into the list with `↑/↓`. This keeps `Enter` free to submit freshly
//!   typed text the list does not contain.
//! - *Fixed choice* (enum fields): seed the labels, [`open`](OptionList::open),
//!   and [`set_cursor`](OptionList::set_cursor) to the current value; the owning
//!   modal drives the selection directly and renders the list for affordance.

/// A filterable, navigable list of option labels rendered as an inline dropdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OptionList {
    /// Every candidate label, in stable display order.
    items: Vec<String>,
    /// Indices into `items` matching the current query, in display order.
    matches: Vec<usize>,
    /// The highlighted position within `matches`.
    cursor: usize,
    /// Whether the dropdown is shown (still gated on having matches).
    open: bool,
    /// Whether the user has moved into the list (`↑/↓`); only then does `Enter`
    /// accept the highlighted item rather than the field's typed text.
    active: bool,
}

impl OptionList {
    /// Builds a list over `items`, initially closed with every item matching.
    pub fn new(items: Vec<String>) -> OptionList {
        let matches = (0..items.len()).collect();
        OptionList {
            items,
            matches,
            cursor: 0,
            open: false,
            active: false,
        }
    }

    /// Requests the dropdown be shown. Visibility is still gated on there being
    /// at least one match (see [`is_open`](OptionList::is_open)).
    pub fn open(&mut self) {
        self.open = true;
    }

    /// Hides the dropdown and clears the active selection.
    pub fn close(&mut self) {
        self.open = false;
        self.active = false;
    }

    /// Whether the dropdown is currently visible (open with at least one match).
    pub fn is_open(&self) -> bool {
        self.open && !self.matches.is_empty()
    }

    /// Recomputes the matches for `query` (case-insensitive substring), resets
    /// the cursor to the top, and clears the active flag so typing re-suggests
    /// rather than committing to a highlighted row.
    pub fn refilter(&mut self, query: &str) {
        let q = query.to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.cursor = 0;
        self.active = false;
    }

    /// Moves the highlight up one row (clamped) and marks the list active.
    pub fn up(&mut self) {
        if self.is_open() {
            self.active = true;
            self.cursor = self.cursor.saturating_sub(1);
        }
    }

    /// Moves the highlight down one row (clamped) and marks the list active.
    pub fn down(&mut self) {
        if self.is_open() {
            self.active = true;
            let last = self.matches.len().saturating_sub(1);
            self.cursor = (self.cursor + 1).min(last);
        }
    }

    /// The highlighted label, but only once the user has engaged the list with
    /// `↑/↓` — so a type-ahead field can tell "accept this suggestion" apart from
    /// "submit my typed text".
    pub fn selected(&self) -> Option<&str> {
        if self.is_open() && self.active {
            self.matches
                .get(self.cursor)
                .map(|&i| self.items[i].as_str())
        } else {
            None
        }
    }

    /// Points the cursor at the match at `index` (clamped) and marks the list
    /// active, used to seed a fixed-choice picker to its current value.
    pub fn set_cursor(&mut self, index: usize) {
        if !self.matches.is_empty() {
            self.cursor = index.min(self.matches.len() - 1);
        }
        self.active = true;
    }

    /// The highlighted position within the matches (for rendering).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The number of matches (for rendering windowing / "N more" hints).
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// The match labels in display order (for rendering).
    pub fn match_labels(&self) -> impl Iterator<Item = &str> {
        self.matches.iter().map(move |&i| self.items[i].as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list() -> OptionList {
        OptionList::new(vec![
            "main".into(),
            "origin/main".into(),
            "origin/dev".into(),
            "feature/login".into(),
        ])
    }

    #[test]
    fn new_matches_everything_and_starts_closed() {
        let ol = list();
        assert_eq!(ol.match_count(), 4);
        assert!(!ol.is_open());
        assert_eq!(ol.selected(), None);
    }

    #[test]
    fn refilter_substring_case_insensitive() {
        let mut ol = list();
        ol.open();
        ol.refilter("MAIN");
        let m: Vec<&str> = ol.match_labels().collect();
        assert_eq!(m, vec!["main", "origin/main"]);
        // A query matching nothing hides the dropdown even while "open".
        ol.refilter("zzz");
        assert_eq!(ol.match_count(), 0);
        assert!(!ol.is_open());
    }

    #[test]
    fn navigation_clamps_within_matches() {
        let mut ol = list();
        ol.open();
        assert_eq!(ol.cursor(), 0);
        ol.up(); // already at top
        assert_eq!(ol.cursor(), 0);
        ol.down();
        ol.down();
        assert_eq!(ol.cursor(), 2);
        for _ in 0..10 {
            ol.down();
        }
        assert_eq!(ol.cursor(), 3); // clamped to last
    }

    #[test]
    fn selected_only_after_engaging_the_list() {
        let mut ol = list();
        ol.open();
        // Open but not engaged: Enter should submit typed text, not a suggestion.
        assert_eq!(ol.selected(), None);
        ol.down(); // engage
        assert_eq!(ol.selected(), Some("origin/main"));
        // Typing re-suggests and de-activates.
        ol.refilter("feat");
        assert_eq!(ol.selected(), None);
        assert_eq!(ol.match_labels().collect::<Vec<_>>(), vec!["feature/login"]);
    }

    #[test]
    fn close_clears_open_and_active() {
        let mut ol = list();
        ol.open();
        ol.down();
        assert!(ol.selected().is_some());
        ol.close();
        assert!(!ol.is_open());
        assert_eq!(ol.selected(), None);
    }

    #[test]
    fn set_cursor_seeds_a_fixed_choice() {
        let mut ol = OptionList::new(vec!["low".into(), "medium".into(), "high".into()]);
        ol.open();
        ol.set_cursor(2);
        assert_eq!(ol.cursor(), 2);
        assert_eq!(ol.selected(), Some("high"));
        // Out-of-range clamps to the last match.
        ol.set_cursor(99);
        assert_eq!(ol.cursor(), 2);
    }

    #[test]
    fn navigation_is_a_noop_while_closed() {
        let mut ol = list();
        ol.down();
        assert_eq!(ol.cursor(), 0);
        assert_eq!(ol.selected(), None);
    }
}
