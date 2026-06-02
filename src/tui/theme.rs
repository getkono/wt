//! Truecolor theme for the TUI (spec §10/§11): maps worktree and UI state to
//! ratatui [`Style`]s.
//!
//! All color is gated behind a single `enabled` flag — resolved once from the
//! `--color` flag, `NO_COLOR`, and `ui.color` (spec §11 precedence) — so that
//! disabling color collapses cleanly to the monochrome look (only the
//! structural `DIM`/`BOLD`/`REVERSED` modifiers remain). The palette is fixed
//! 24-bit RGB (One-Dark-style), so it renders consistently regardless of the
//! terminal's theme.

use ratatui::style::{Color, Modifier, Style};

use crate::model::PrState;
use crate::tui::app::{Mode, StatusKind};

/// Accent (focus, links, selection bar) — blue.
const ACCENT: Color = Color::Rgb(97, 175, 239);
/// Good/current/ahead/open — green.
const GREEN: Color = Color::Rgb(152, 195, 121);
/// Bad/behind/missing/closed — red.
const RED: Color = Color::Rgb(224, 108, 117);
/// Dirty/detached/warning — yellow.
const YELLOW: Color = Color::Rgb(229, 192, 123);
/// Commit hash — orange.
const ORANGE: Color = Color::Rgb(209, 154, 102);
/// Untracked — cyan.
const CYAN: Color = Color::Rgb(86, 182, 194);
/// Merged PR — magenta.
const MAGENTA: Color = Color::Rgb(198, 120, 221);
/// Muted text (absent marker, spinner, relative time, labels) — gray.
const GRAY: Color = Color::Rgb(92, 99, 112);
/// Selected-row background.
const SELECTION_BG: Color = Color::Rgb(62, 68, 81);
/// Foreground for text drawn on a colored chip (the mode label).
const CHIP_FG: Color = Color::Rgb(30, 33, 39);

/// A resolved theme. Construct with [`Theme::new`]; every accessor returns a
/// ratatui [`Style`] that is plain ([`Style::default`]) when color is disabled.
pub struct Theme {
    enabled: bool,
}

impl Theme {
    /// Builds a theme. When `enabled` is false every color accessor returns a
    /// plain style, preserving the monochrome (`NO_COLOR`) appearance.
    pub fn new(enabled: bool) -> Theme {
        Theme { enabled }
    }

    /// Whether color is enabled (some widgets adjust their fallback styling).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// A foreground color, or a plain style when color is disabled.
    fn fg(&self, color: Color) -> Style {
        if self.enabled {
            Style::default().fg(color)
        } else {
            Style::default()
        }
    }

    /// A foreground color plus a modifier, or a plain style when disabled.
    fn styled(&self, color: Color, modifier: Modifier) -> Style {
        if self.enabled {
            Style::default().fg(color).add_modifier(modifier)
        } else {
            Style::default()
        }
    }

    /// Style for the current-worktree marker (`*`/▸).
    pub fn current(&self) -> Style {
        self.styled(GREEN, Modifier::BOLD)
    }

    /// Style for the missing-worktree marker (`!`/✘).
    pub fn missing(&self) -> Style {
        self.fg(RED)
    }

    /// Style for the detached-HEAD marker (`~`/⚓).
    pub fn detached(&self) -> Style {
        self.fg(YELLOW)
    }

    /// Style for the dirty marker (`M`/●).
    pub fn dirty(&self) -> Style {
        self.fg(YELLOW)
    }

    /// Style for the untracked marker (`?`).
    pub fn untracked(&self) -> Style {
        self.fg(CYAN)
    }

    /// Style for the "field unavailable" placeholder (`–`).
    pub fn absent(&self) -> Style {
        self.fg(GRAY)
    }

    /// Style for the per-field loading spinner (`…`).
    pub fn spinner(&self) -> Style {
        self.fg(GRAY)
    }

    /// Style for the ahead count (`↑N`): green when ahead, muted at zero.
    pub fn ahead(&self, count: u32) -> Style {
        if count > 0 {
            self.fg(GREEN)
        } else {
            self.fg(GRAY)
        }
    }

    /// Style for the behind count (`↓N`): red when behind, muted at zero.
    pub fn behind(&self, count: u32) -> Style {
        if count > 0 {
            self.fg(RED)
        } else {
            self.fg(GRAY)
        }
    }

    /// Style for a commit short hash.
    pub fn commit_hash(&self) -> Style {
        self.fg(ORANGE)
    }

    /// Style for a relative timestamp.
    pub fn time(&self) -> Style {
        self.fg(GRAY)
    }

    /// Style for a branch name, by role.
    pub fn branch(&self, is_current: bool, is_detached: bool) -> Style {
        if is_detached {
            self.fg(YELLOW)
        } else if is_current {
            self.styled(ACCENT, Modifier::BOLD)
        } else {
            Style::default()
        }
    }

    /// Style for a PR's number/state cell, by PR state.
    pub fn pr_state(&self, state: PrState) -> Style {
        let color = match state {
            PrState::Open => GREEN,
            PrState::Draft => GRAY,
            PrState::Merged => MAGENTA,
            PrState::Closed => RED,
        };
        self.fg(color)
    }

    /// The selected-row highlight: a background bar when color is enabled (so the
    /// per-field foreground colors stay readable), reversed video otherwise.
    pub fn selection(&self) -> Style {
        if self.enabled {
            Style::default()
                .bg(SELECTION_BG)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// The left-bar highlight symbol for the selected row.
    pub fn selection_symbol(&self) -> &'static str {
        if self.enabled { "▌ " } else { "> " }
    }

    /// The status-bar mode chip, colored per mode (reversed when disabled).
    pub fn mode_chip(&self, mode: &Mode) -> Style {
        if !self.enabled {
            return Style::default().add_modifier(Modifier::REVERSED);
        }
        let color = match mode {
            Mode::List => ACCENT,
            Mode::Filter => YELLOW,
            Mode::Create(_) => GREEN,
            Mode::PrPicker(_) => MAGENTA,
            Mode::ConfirmRemove(_) => RED,
            Mode::Help => CYAN,
        };
        Style::default()
            .bg(color)
            .fg(CHIP_FG)
            .add_modifier(Modifier::BOLD)
    }

    /// A pane border style; the focused pane is accented, others muted.
    pub fn border(&self, focused: bool) -> Style {
        match (self.enabled, focused) {
            (true, true) => Style::default().fg(ACCENT),
            (true, false) => Style::default().fg(GRAY),
            (false, true) => Style::default(),
            (false, false) => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// A pane title style; the focused pane is accented/bold, others muted.
    pub fn title(&self, focused: bool) -> Style {
        match (self.enabled, focused) {
            (true, true) => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            (true, false) => Style::default().fg(GRAY),
            (false, true) => Style::default().add_modifier(Modifier::BOLD),
            (false, false) => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// Style for a key token in the status-bar / help key hints.
    pub fn hint_key(&self) -> Style {
        self.styled(ACCENT, Modifier::BOLD)
    }

    /// Style for the description text next to a key hint.
    pub fn hint_label(&self) -> Style {
        self.fg(GRAY)
    }

    /// Style for a detail-pane field label.
    pub fn label(&self) -> Style {
        self.fg(GRAY)
    }

    /// The accent style (active fields, prompts).
    pub fn accent(&self) -> Style {
        self.fg(ACCENT)
    }

    /// Style for a clickable/URL value.
    pub fn url(&self) -> Style {
        if self.enabled {
            Style::default()
                .fg(ACCENT)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
        }
    }

    /// Style for a transient status message, by severity.
    pub fn status(&self, kind: StatusKind) -> Style {
        match kind {
            StatusKind::Success => self.fg(GREEN),
            StatusKind::Error => self.fg(RED),
            StatusKind::Info => Style::default(),
        }
    }

    /// Style for error text in modals.
    pub fn error(&self) -> Style {
        self.fg(RED)
    }

    /// Style for warning text in modals.
    pub fn warning(&self) -> Style {
        self.fg(YELLOW)
    }

    /// Style for a reassuring/positive note (e.g. "merged — safe to delete").
    pub fn success(&self) -> Style {
        self.fg(GREEN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_applies_color_disabled_is_plain() {
        let on = Theme::new(true);
        let off = Theme::new(false);
        assert!(on.enabled());
        assert_eq!(on.current().fg, Some(GREEN));
        assert!(on.current().add_modifier.contains(Modifier::BOLD));
        // Disabled returns a plain style: no fg, no bold.
        assert_eq!(off.current(), Style::default());
        assert_eq!(off.commit_hash(), Style::default());
    }

    #[test]
    fn ahead_behind_mute_at_zero() {
        let t = Theme::new(true);
        assert_eq!(t.ahead(2).fg, Some(GREEN));
        assert_eq!(t.ahead(0).fg, Some(GRAY));
        assert_eq!(t.behind(3).fg, Some(RED));
        assert_eq!(t.behind(0).fg, Some(GRAY));
    }

    #[test]
    fn pr_state_colors() {
        let t = Theme::new(true);
        assert_eq!(t.pr_state(PrState::Open).fg, Some(GREEN));
        assert_eq!(t.pr_state(PrState::Draft).fg, Some(GRAY));
        assert_eq!(t.pr_state(PrState::Merged).fg, Some(MAGENTA));
        assert_eq!(t.pr_state(PrState::Closed).fg, Some(RED));
    }

    #[test]
    fn branch_role_styling() {
        let t = Theme::new(true);
        assert_eq!(t.branch(false, true).fg, Some(YELLOW)); // detached
        assert_eq!(t.branch(true, false).fg, Some(ACCENT)); // current
        assert_eq!(t.branch(false, false), Style::default()); // plain
    }

    #[test]
    fn selection_uses_bg_or_reversed() {
        assert_eq!(Theme::new(true).selection().bg, Some(SELECTION_BG));
        assert!(
            Theme::new(false)
                .selection()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(Theme::new(true).selection_symbol(), "▌ ");
        assert_eq!(Theme::new(false).selection_symbol(), "> ");
    }

    #[test]
    fn mode_chip_colors_per_mode() {
        let t = Theme::new(true);
        assert_eq!(t.mode_chip(&Mode::List).bg, Some(ACCENT));
        assert_eq!(t.mode_chip(&Mode::Filter).bg, Some(YELLOW));
        assert_eq!(t.mode_chip(&Mode::Help).bg, Some(CYAN));
        assert_eq!(t.mode_chip(&Mode::ConfirmRemove(0)).bg, Some(RED));
        // Disabled falls back to reversed video.
        assert!(
            Theme::new(false)
                .mode_chip(&Mode::List)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn focus_changes_border_and_title() {
        let t = Theme::new(true);
        assert_eq!(t.border(true).fg, Some(ACCENT));
        assert_eq!(t.border(false).fg, Some(GRAY));
        assert_eq!(t.title(true).fg, Some(ACCENT));
        assert!(t.title(true).add_modifier.contains(Modifier::BOLD));
        // Monochrome still conveys focus via bold vs dim.
        let off = Theme::new(false);
        assert!(off.title(true).add_modifier.contains(Modifier::BOLD));
        assert!(off.border(false).add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn status_severity_colors() {
        let t = Theme::new(true);
        assert_eq!(t.status(StatusKind::Success).fg, Some(GREEN));
        assert_eq!(t.status(StatusKind::Error).fg, Some(RED));
        assert_eq!(t.status(StatusKind::Info), Style::default());
    }
}
