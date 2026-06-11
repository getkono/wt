//! Truecolor theme for the TUI (spec §10/§11): maps worktree and UI state to
//! ratatui [`Style`]s.
//!
//! All color is gated behind a single `enabled` flag — resolved once from the
//! `--color` flag, `NO_COLOR`, and `ui.color` (spec §11 precedence) — so that
//! disabling color collapses cleanly to the monochrome look (only the
//! structural `DIM`/`BOLD`/`REVERSED` modifiers remain). The concrete colors
//! come from a [`Palette`]: a built-in [`ThemePreset`] (One-Dark by default) with
//! per-color overrides applied on top (`[ui.theme]`, spec §11). Each palette is
//! fixed 24-bit RGB, so it renders consistently regardless of the terminal's own
//! theme.

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

/// The set of semantic colors a [`Theme`] draws from. Every field is a concrete
/// 24-bit (or named) [`Color`]; [`Theme`] decides where each is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Accent (focus, links, selection bar).
    pub accent: Color,
    /// Good/current/ahead/open.
    pub green: Color,
    /// Bad/behind/missing/closed.
    pub red: Color,
    /// Dirty/detached/warning.
    pub yellow: Color,
    /// Commit hash.
    pub orange: Color,
    /// Untracked.
    pub cyan: Color,
    /// Merged PR.
    pub magenta: Color,
    /// Muted text (absent marker, spinner, relative time, labels).
    pub gray: Color,
    /// Selected-row background.
    pub selection_bg: Color,
    /// Foreground for text drawn on a colored chip (the mode label).
    pub chip_fg: Color,
}

impl Palette {
    /// The default One-Dark-style palette.
    pub fn one_dark() -> Palette {
        Palette {
            accent: ACCENT,
            green: GREEN,
            red: RED,
            yellow: YELLOW,
            orange: ORANGE,
            cyan: CYAN,
            magenta: MAGENTA,
            gray: GRAY,
            selection_bg: SELECTION_BG,
            chip_fg: CHIP_FG,
        }
    }

    /// The Solarized-dark palette.
    pub fn solarized() -> Palette {
        Palette {
            accent: Color::Rgb(38, 139, 210),    // blue
            green: Color::Rgb(133, 153, 0),      // green
            red: Color::Rgb(220, 50, 47),        // red
            yellow: Color::Rgb(181, 137, 0),     // yellow
            orange: Color::Rgb(203, 75, 22),     // orange
            cyan: Color::Rgb(42, 161, 152),      // cyan
            magenta: Color::Rgb(211, 54, 130),   // magenta
            gray: Color::Rgb(88, 110, 117),      // base01
            selection_bg: Color::Rgb(7, 54, 66), // base02
            chip_fg: Color::Rgb(0, 43, 54),      // base03
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Palette::one_dark()
    }
}

/// A built-in base palette, selected by `ui.theme.preset` (spec §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemePreset {
    /// One-Dark-style (the default).
    #[default]
    OneDark,
    /// Solarized-dark.
    Solarized,
}

impl ThemePreset {
    /// Parses a preset identifier (`one-dark`/`solarized`), or `None` if unknown.
    pub fn parse(s: &str) -> Option<ThemePreset> {
        match s {
            "one-dark" => Some(ThemePreset::OneDark),
            "solarized" => Some(ThemePreset::Solarized),
            _ => None,
        }
    }

    /// The stable identifier for this preset (the `config get` form).
    pub fn id(self) -> &'static str {
        match self {
            ThemePreset::OneDark => "one-dark",
            ThemePreset::Solarized => "solarized",
        }
    }

    /// The base [`Palette`] for this preset.
    pub fn palette(self) -> Palette {
        match self {
            ThemePreset::OneDark => Palette::one_dark(),
            ThemePreset::Solarized => Palette::solarized(),
        }
    }
}

/// A resolved theme. Construct with [`Theme::new`] (One-Dark) or
/// [`Theme::with_palette`]; every accessor returns a ratatui [`Style`] that is
/// plain ([`Style::default`]) when color is disabled.
pub struct Theme {
    enabled: bool,
    palette: Palette,
}

impl Theme {
    /// Builds a theme over the default (One-Dark) palette. When `enabled` is
    /// false every color accessor returns a plain style, preserving the
    /// monochrome (`NO_COLOR`) appearance.
    pub fn new(enabled: bool) -> Theme {
        Theme {
            enabled,
            palette: Palette::one_dark(),
        }
    }

    /// Builds a theme over a specific [`Palette`] (the configured one). Color is
    /// still gated by `enabled`.
    pub fn with_palette(enabled: bool, palette: Palette) -> Theme {
        Theme { enabled, palette }
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
        self.styled(self.palette.green, Modifier::BOLD)
    }

    /// Style for the missing-worktree marker (`!`/✘).
    pub fn missing(&self) -> Style {
        self.fg(self.palette.red)
    }

    /// Style for the detached-HEAD marker (`~`/⚓).
    pub fn detached(&self) -> Style {
        self.fg(self.palette.yellow)
    }

    /// Style for the worktree-less branch marker (`○`): muted, since a branch row
    /// is secondary to the real worktrees above it (issue #47).
    pub fn branchless(&self) -> Style {
        self.fg(self.palette.gray)
    }

    /// Style for the dirty marker (`M`/●).
    pub fn dirty(&self) -> Style {
        self.fg(self.palette.yellow)
    }

    /// Style for the untracked marker (`?`).
    pub fn untracked(&self) -> Style {
        self.fg(self.palette.cyan)
    }

    /// Style for the "field unavailable" placeholder (`–`).
    pub fn absent(&self) -> Style {
        self.fg(self.palette.gray)
    }

    /// Style for the per-field loading spinner (`…`).
    pub fn spinner(&self) -> Style {
        self.fg(self.palette.gray)
    }

    /// Style for the ahead count (`↑N`): green when ahead, muted at zero.
    pub fn ahead(&self, count: u32) -> Style {
        if count > 0 {
            self.fg(self.palette.green)
        } else {
            self.fg(self.palette.gray)
        }
    }

    /// Style for the behind count (`↓N`): red when behind, muted at zero.
    pub fn behind(&self, count: u32) -> Style {
        if count > 0 {
            self.fg(self.palette.red)
        } else {
            self.fg(self.palette.gray)
        }
    }

    /// Style for a commit short hash.
    pub fn commit_hash(&self) -> Style {
        self.fg(self.palette.orange)
    }

    /// Style for a relative timestamp.
    pub fn time(&self) -> Style {
        self.fg(self.palette.gray)
    }

    /// Style for a branch name, by role.
    pub fn branch(&self, is_current: bool, is_detached: bool) -> Style {
        if is_detached {
            self.fg(self.palette.yellow)
        } else if is_current {
            self.styled(self.palette.accent, Modifier::BOLD)
        } else {
            Style::default()
        }
    }

    /// Style for a PR's number/state cell, by PR state.
    pub fn pr_state(&self, state: PrState) -> Style {
        let color = match state {
            PrState::Open => self.palette.green,
            PrState::Draft => self.palette.gray,
            PrState::Merged => self.palette.magenta,
            PrState::Closed => self.palette.red,
        };
        self.fg(color)
    }

    /// The selected-row highlight: a background bar when color is enabled (so the
    /// per-field foreground colors stay readable), reversed video otherwise.
    pub fn selection(&self) -> Style {
        if self.enabled {
            Style::default()
                .bg(self.palette.selection_bg)
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
            Mode::List => self.palette.accent,
            Mode::Filter => self.palette.yellow,
            Mode::Create(_) => self.palette.green,
            Mode::PrPicker(_) => self.palette.magenta,
            Mode::PrCompose(_) => self.palette.green,
            Mode::Checkout(_) => self.palette.accent,
            Mode::ConfirmRemove(_) => self.palette.red,
            Mode::ConfirmCreate(_) => self.palette.green,
            Mode::Help => self.palette.cyan,
        };
        Style::default()
            .bg(color)
            .fg(self.palette.chip_fg)
            .add_modifier(Modifier::BOLD)
    }

    /// A pane border style; the focused pane is accented, others muted.
    pub fn border(&self, focused: bool) -> Style {
        match (self.enabled, focused) {
            (true, true) => Style::default().fg(self.palette.accent),
            (true, false) => Style::default().fg(self.palette.gray),
            (false, true) => Style::default(),
            (false, false) => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// A pane title style; the focused pane is accented/bold, others muted.
    pub fn title(&self, focused: bool) -> Style {
        match (self.enabled, focused) {
            (true, true) => Style::default()
                .fg(self.palette.accent)
                .add_modifier(Modifier::BOLD),
            (true, false) => Style::default().fg(self.palette.gray),
            (false, true) => Style::default().add_modifier(Modifier::BOLD),
            (false, false) => Style::default().add_modifier(Modifier::DIM),
        }
    }

    /// Style for a key token in the status-bar / help key hints.
    pub fn hint_key(&self) -> Style {
        self.styled(self.palette.accent, Modifier::BOLD)
    }

    /// Style for the description text next to a key hint.
    pub fn hint_label(&self) -> Style {
        self.fg(self.palette.gray)
    }

    /// Style for a detail-pane field label.
    pub fn label(&self) -> Style {
        self.fg(self.palette.gray)
    }

    /// The accent style (active fields, prompts).
    pub fn accent(&self) -> Style {
        self.fg(self.palette.accent)
    }

    /// Style for a clickable/URL value.
    pub fn url(&self) -> Style {
        if self.enabled {
            Style::default()
                .fg(self.palette.accent)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
        }
    }

    /// Style for a transient status message, by severity.
    pub fn status(&self, kind: StatusKind) -> Style {
        match kind {
            StatusKind::Success => self.fg(self.palette.green),
            StatusKind::Error => self.fg(self.palette.red),
            StatusKind::Info => Style::default(),
        }
    }

    /// Style for error text in modals.
    pub fn error(&self) -> Style {
        self.fg(self.palette.red)
    }

    /// Style for warning text in modals.
    pub fn warning(&self) -> Style {
        self.fg(self.palette.yellow)
    }

    /// Style for a reassuring/positive note (e.g. "merged — safe to delete").
    pub fn success(&self) -> Style {
        self.fg(self.palette.green)
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
        // The worktree-less branch marker is muted (issue #47).
        assert_eq!(t.branchless().fg, Some(GRAY));
        assert_eq!(Theme::new(false).branchless(), Style::default());
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
        assert_eq!(t.mode_chip(&Mode::ConfirmCreate(0)).bg, Some(GREEN));
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

    #[test]
    fn preset_parse_and_id_round_trip() {
        assert_eq!(ThemePreset::parse("one-dark"), Some(ThemePreset::OneDark));
        assert_eq!(
            ThemePreset::parse("solarized"),
            Some(ThemePreset::Solarized)
        );
        assert_eq!(ThemePreset::parse("nope"), None);
        assert_eq!(ThemePreset::OneDark.id(), "one-dark");
        assert_eq!(ThemePreset::Solarized.id(), "solarized");
        // The default preset is One-Dark and matches the legacy constants.
        assert_eq!(ThemePreset::default(), ThemePreset::OneDark);
        assert_eq!(ThemePreset::OneDark.palette(), Palette::one_dark());
    }

    #[test]
    fn one_dark_palette_matches_legacy_constants() {
        let p = Palette::one_dark();
        assert_eq!(p.accent, ACCENT);
        assert_eq!(p.green, GREEN);
        assert_eq!(p.red, RED);
        assert_eq!(p.yellow, YELLOW);
        assert_eq!(p.orange, ORANGE);
        assert_eq!(p.cyan, CYAN);
        assert_eq!(p.magenta, MAGENTA);
        assert_eq!(p.gray, GRAY);
        assert_eq!(p.selection_bg, SELECTION_BG);
        assert_eq!(p.chip_fg, CHIP_FG);
        // `Default` is One-Dark.
        assert_eq!(Palette::default(), p);
    }

    #[test]
    fn with_palette_applies_custom_colors() {
        // A custom palette flows through the semantic accessors.
        let mut p = Palette::one_dark();
        p.green = Color::Rgb(1, 2, 3);
        p.accent = Color::Rgb(4, 5, 6);
        let t = Theme::with_palette(true, p);
        assert_eq!(t.current().fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(t.border(true).fg, Some(Color::Rgb(4, 5, 6)));
        // Solarized differs from One-Dark on every primary slot.
        let sol = Palette::solarized();
        assert_ne!(sol.accent, ACCENT);
        assert_ne!(sol.green, GREEN);
        // Color still gates: disabled is plain regardless of palette.
        assert_eq!(Theme::with_palette(false, p).current(), Style::default());
    }
}
