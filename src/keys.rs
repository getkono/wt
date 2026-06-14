//! TUI key bindings (spec §10/§11): the action set, the default keymap, and
//! parsing/rendering of key strings such as `ctrl+u` or `f5`.
//!
//! A [`KeyChord`] is a normalized key + modifier combination used as the map
//! key. Normalization makes matching terminal-independent: `Shift+Tab` (reported
//! by terminals as `BackTab`) becomes `Tab`+`SHIFT`, and the `SHIFT` modifier is
//! dropped from character keys (the shift is already encoded in the character).

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A TUI action that can be bound to a key (spec §10/§11). The 24 variants match
/// the action names accepted by `ui.keybindings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyAction {
    /// Move the selection up.
    NavigateUp,
    /// Move the selection down.
    NavigateDown,
    /// Scroll up one page.
    PageUp,
    /// Scroll down one page.
    PageDown,
    /// Jump to the first row.
    GoToTop,
    /// Jump to the last row.
    GoToBottom,
    /// Focus the next pane.
    FocusNextPane,
    /// Focus the previous pane.
    FocusPrevPane,
    /// Switch to the selected worktree (print path, exit).
    Switch,
    /// Enter filter mode.
    Filter,
    /// Clear the filter / dismiss an overlay.
    ClearFilter,
    /// Open the create-worktree prompt.
    New,
    /// Open the confirm-remove dialog.
    Remove,
    /// Open the PR picker.
    PrCheckout,
    /// Check out a branch in the selected worktree (syncs with origin).
    Checkout,
    /// Sync (pull then push) the selected worktree's branch.
    Sync,
    /// Open the selected worktree in the editor.
    OpenEditor,
    /// Force a full async refresh.
    Refresh,
    /// Cycle the sort field.
    SortCycle,
    /// Toggle the sort direction.
    SortReverse,
    /// Show the help overlay.
    Help,
    /// Quit without switching.
    Quit,
    /// Toggle the list pane (full-screen detail).
    ToggleSidebar,
    /// Grow the list pane width.
    ResizeSidebarGrow,
    /// Shrink the list pane width.
    ResizeSidebarShrink,
}

impl KeyAction {
    /// All actions, in the order documented in §11.
    pub const ALL: [KeyAction; 25] = [
        KeyAction::NavigateUp,
        KeyAction::NavigateDown,
        KeyAction::PageUp,
        KeyAction::PageDown,
        KeyAction::GoToTop,
        KeyAction::GoToBottom,
        KeyAction::FocusNextPane,
        KeyAction::FocusPrevPane,
        KeyAction::Switch,
        KeyAction::Filter,
        KeyAction::ClearFilter,
        KeyAction::New,
        KeyAction::Remove,
        KeyAction::PrCheckout,
        KeyAction::Checkout,
        KeyAction::Sync,
        KeyAction::OpenEditor,
        KeyAction::Refresh,
        KeyAction::SortCycle,
        KeyAction::SortReverse,
        KeyAction::Help,
        KeyAction::Quit,
        KeyAction::ToggleSidebar,
        KeyAction::ResizeSidebarGrow,
        KeyAction::ResizeSidebarShrink,
    ];

    /// The action name used in `ui.keybindings` configuration.
    pub fn name(self) -> &'static str {
        match self {
            KeyAction::NavigateUp => "navigate-up",
            KeyAction::NavigateDown => "navigate-down",
            KeyAction::PageUp => "page-up",
            KeyAction::PageDown => "page-down",
            KeyAction::GoToTop => "go-to-top",
            KeyAction::GoToBottom => "go-to-bottom",
            KeyAction::FocusNextPane => "focus-next-pane",
            KeyAction::FocusPrevPane => "focus-prev-pane",
            KeyAction::Switch => "switch",
            KeyAction::Filter => "filter",
            KeyAction::ClearFilter => "clear-filter",
            KeyAction::New => "new",
            KeyAction::Remove => "remove",
            KeyAction::PrCheckout => "pr-checkout",
            KeyAction::Checkout => "checkout",
            KeyAction::Sync => "sync",
            KeyAction::OpenEditor => "open-editor",
            KeyAction::Refresh => "refresh",
            KeyAction::SortCycle => "sort-cycle",
            KeyAction::SortReverse => "sort-reverse",
            KeyAction::Help => "help",
            KeyAction::Quit => "quit",
            KeyAction::ToggleSidebar => "toggle-sidebar",
            KeyAction::ResizeSidebarGrow => "resize-sidebar-grow",
            KeyAction::ResizeSidebarShrink => "resize-sidebar-shrink",
        }
    }

    /// Parses an action name, or `None` if unknown.
    pub fn parse(name: &str) -> Option<KeyAction> {
        KeyAction::ALL.into_iter().find(|a| a.name() == name)
    }

    /// A short human label for the status bar and help overlay (e.g. `switch`,
    /// `new`, `checkout`). The match is exhaustive, so a new [`KeyAction`]
    /// variant cannot be added without giving it a label — this is what keeps
    /// the on-screen hints and help from drifting away from the key bindings
    /// (issue #39).
    pub fn label(self) -> &'static str {
        match self {
            KeyAction::NavigateUp => "navigate up",
            KeyAction::NavigateDown => "navigate down",
            KeyAction::PageUp => "page up",
            KeyAction::PageDown => "page down",
            KeyAction::GoToTop => "go to top",
            KeyAction::GoToBottom => "go to bottom",
            KeyAction::FocusNextPane => "next pane",
            KeyAction::FocusPrevPane => "prev pane",
            KeyAction::Switch => "switch",
            KeyAction::Filter => "filter",
            KeyAction::ClearFilter => "clear / back",
            KeyAction::New => "new",
            KeyAction::Remove => "remove",
            KeyAction::PrCheckout => "pr picker",
            KeyAction::Checkout => "checkout",
            KeyAction::Sync => "sync",
            KeyAction::OpenEditor => "open in editor",
            KeyAction::Refresh => "refresh",
            KeyAction::SortCycle => "sort cycle",
            KeyAction::SortReverse => "sort reverse",
            KeyAction::Help => "help",
            KeyAction::Quit => "quit",
            KeyAction::ToggleSidebar => "toggle sidebar",
            KeyAction::ResizeSidebarGrow => "grow sidebar",
            KeyAction::ResizeSidebarShrink => "shrink sidebar",
        }
    }
}

/// A normalized key + modifier combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyChord {
    /// The key code.
    pub code: KeyCode,
    /// The active modifiers (after normalization).
    pub mods: KeyModifiers,
}

impl KeyChord {
    /// A chord with no modifiers.
    pub fn key(code: KeyCode) -> KeyChord {
        KeyChord {
            code,
            mods: KeyModifiers::empty(),
        }
    }

    /// A `Ctrl`+character chord.
    pub fn ctrl(c: char) -> KeyChord {
        KeyChord {
            code: KeyCode::Char(c),
            mods: KeyModifiers::CONTROL,
        }
    }

    /// Builds a normalized chord from a key code and modifiers.
    pub fn normalized(code: KeyCode, mods: KeyModifiers) -> KeyChord {
        let mut code = code;
        let mut mods = mods;
        // `Shift+Tab` arrives as `BackTab`; normalize to `Tab`+`SHIFT`.
        if code == KeyCode::BackTab {
            code = KeyCode::Tab;
            mods |= KeyModifiers::SHIFT;
        }
        // For character keys, shift is already encoded in the character.
        if matches!(code, KeyCode::Char(_)) {
            mods.remove(KeyModifiers::SHIFT);
        }
        // Keep only the modifiers we bind on.
        mods &= KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT;
        KeyChord { code, mods }
    }

    /// Normalizes a terminal key event into a chord for lookup.
    pub fn from_event(ev: KeyEvent) -> KeyChord {
        KeyChord::normalized(ev.code, ev.modifiers)
    }

    /// Parses a key string such as `ctrl+u`, `alt+enter`, `shift+tab`, `f5`, or
    /// `q`. Returns `None` for malformed strings.
    pub fn parse(s: &str) -> Option<KeyChord> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // The literal `+`/`-` keys double as separators; special-case them.
        if s == "+" {
            return Some(KeyChord::key(KeyCode::Char('+')));
        }
        if s == "-" {
            return Some(KeyChord::key(KeyCode::Char('-')));
        }
        let parts: Vec<&str> = s.split('+').collect();
        let (key_tok, mod_toks) = parts.split_last()?;
        if key_tok.is_empty() {
            return None;
        }
        let mut mods = KeyModifiers::empty();
        for m in mod_toks {
            mods |= parse_modifier(m)?;
        }
        let code = parse_keycode(key_tok)?;
        Some(KeyChord::normalized(code, mods))
    }

    /// Renders this chord back to a key string (inverse of [`KeyChord::parse`]).
    pub fn render(&self) -> String {
        let mut s = String::new();
        if self.mods.contains(KeyModifiers::CONTROL) {
            s.push_str("ctrl+");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push_str("alt+");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push_str("shift+");
        }
        s.push_str(&keycode_name(self.code));
        s
    }

    /// Renders this chord for on-screen hints and the help overlay:
    /// terminal-pretty (`↑`, `Enter`, `Shift+Tab`, `Ctrl-S`, plain letters).
    /// Distinct from [`KeyChord::render`], which produces the lowercase
    /// `ctrl+u` *config* format that round-trips through [`KeyChord::parse`].
    pub fn display(&self) -> String {
        let ctrl = self.mods.contains(KeyModifiers::CONTROL);
        let mut s = String::new();
        if ctrl {
            s.push_str("Ctrl-");
        }
        if self.mods.contains(KeyModifiers::ALT) {
            s.push_str("Alt-");
        }
        if self.mods.contains(KeyModifiers::SHIFT) {
            s.push_str("Shift+");
        }
        // Control chords read better uppercased (`Ctrl-U`), matching convention.
        match self.code {
            KeyCode::Char(c) if ctrl => s.push(c.to_ascii_uppercase()),
            code => s.push_str(&keycode_display(code)),
        }
        s
    }
}

/// Parses a modifier token (case-insensitive).
fn parse_modifier(token: &str) -> Option<KeyModifiers> {
    Some(match token.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => KeyModifiers::CONTROL,
        "alt" | "option" => KeyModifiers::ALT,
        "shift" => KeyModifiers::SHIFT,
        _ => return None,
    })
}

/// Parses a key token (case-insensitive for names, case-preserving for chars).
fn parse_keycode(token: &str) -> Option<KeyCode> {
    let lower = token.to_ascii_lowercase();
    Some(match lower.as_str() {
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "space" => KeyCode::Char(' '),
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        _ => {
            if let Some(n) = lower.strip_prefix('f').and_then(|d| d.parse::<u8>().ok())
                && (1..=12).contains(&n)
            {
                return Some(KeyCode::F(n));
            }
            let mut chars = token.chars();
            let c = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(c)
        }
    })
}

/// Renders a key code back to its token (inverse of [`parse_keycode`]).
fn keycode_name(code: KeyCode) -> String {
    match code {
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

/// Renders a key code for on-screen display (pretty glyphs/short names), the
/// counterpart to [`keycode_name`]'s config tokens (see [`KeyChord::display`]).
fn keycode_display(code: KeyCode) -> String {
    match code {
        KeyCode::Up => "↑".into(),
        KeyCode::Down => "↓".into(),
        KeyCode::Left => "←".into(),
        KeyCode::Right => "→".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PgUp".into(),
        KeyCode::PageDown => "PgDn".into(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "Shift+Tab".into(),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Delete => "Del".into(),
        KeyCode::Insert => "Ins".into(),
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    }
}

/// A mapping from key chords to actions (spec §10 defaults, overridable per
/// action by `ui.keybindings`).
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: HashMap<KeyChord, KeyAction>,
}

impl Keymap {
    /// The default key bindings (spec §10 table).
    pub fn defaults() -> Keymap {
        let pairs: Vec<(KeyAction, KeyChord)> = vec![
            (KeyAction::NavigateUp, KeyChord::key(KeyCode::Up)),
            (KeyAction::NavigateUp, KeyChord::key(KeyCode::Char('k'))),
            (KeyAction::NavigateDown, KeyChord::key(KeyCode::Down)),
            (KeyAction::NavigateDown, KeyChord::key(KeyCode::Char('j'))),
            (KeyAction::PageUp, KeyChord::key(KeyCode::PageUp)),
            (KeyAction::PageUp, KeyChord::ctrl('u')),
            (KeyAction::PageDown, KeyChord::key(KeyCode::PageDown)),
            (KeyAction::PageDown, KeyChord::ctrl('d')),
            (KeyAction::GoToTop, KeyChord::key(KeyCode::Char('g'))),
            (KeyAction::GoToTop, KeyChord::key(KeyCode::Home)),
            (KeyAction::GoToBottom, KeyChord::key(KeyCode::Char('G'))),
            (KeyAction::GoToBottom, KeyChord::key(KeyCode::End)),
            (KeyAction::FocusNextPane, KeyChord::key(KeyCode::Tab)),
            (
                KeyAction::FocusPrevPane,
                KeyChord::normalized(KeyCode::Tab, KeyModifiers::SHIFT),
            ),
            (KeyAction::Switch, KeyChord::key(KeyCode::Enter)),
            (KeyAction::Filter, KeyChord::key(KeyCode::Char('/'))),
            (KeyAction::ClearFilter, KeyChord::key(KeyCode::Esc)),
            (KeyAction::New, KeyChord::key(KeyCode::Char('n'))),
            (KeyAction::Remove, KeyChord::key(KeyCode::Char('d'))),
            (KeyAction::PrCheckout, KeyChord::key(KeyCode::Char('p'))),
            (KeyAction::Checkout, KeyChord::key(KeyCode::Char('c'))),
            (KeyAction::Sync, KeyChord::key(KeyCode::Char('y'))),
            (KeyAction::OpenEditor, KeyChord::key(KeyCode::Char('o'))),
            (KeyAction::Refresh, KeyChord::key(KeyCode::Char('r'))),
            (KeyAction::SortCycle, KeyChord::key(KeyCode::Char('s'))),
            (KeyAction::SortReverse, KeyChord::key(KeyCode::Char('S'))),
            (KeyAction::Help, KeyChord::key(KeyCode::Char('?'))),
            (KeyAction::Quit, KeyChord::key(KeyCode::Char('q'))),
            (KeyAction::ToggleSidebar, KeyChord::key(KeyCode::Char('\\'))),
            (
                KeyAction::ResizeSidebarGrow,
                KeyChord::key(KeyCode::Char('+')),
            ),
            (
                KeyAction::ResizeSidebarShrink,
                KeyChord::key(KeyCode::Char('-')),
            ),
        ];
        let mut bindings = HashMap::with_capacity(pairs.len());
        for (action, chord) in pairs {
            bindings.insert(chord, action);
        }
        Keymap { bindings }
    }

    /// Returns the action bound to `chord`, if any.
    pub fn action_for(&self, chord: KeyChord) -> Option<KeyAction> {
        self.bindings.get(&chord).copied()
    }

    /// Rebinds `action` to a single `chord`, replacing all of that action's
    /// existing bindings (the `ui.keybindings` override semantics, §11).
    pub fn rebind(&mut self, action: KeyAction, chord: KeyChord) {
        self.bindings.retain(|_, a| *a != action);
        self.bindings.insert(chord, action);
    }

    /// Returns the chords currently bound to `action` (for help/hints display).
    pub fn chords_for(&self, action: KeyAction) -> Vec<KeyChord> {
        self.bindings
            .iter()
            .filter(|(_, a)| **a == action)
            .map(|(c, _)| *c)
            .collect()
    }

    /// A stable, human-readable display of every chord bound to `action`, joined
    /// with `/` (e.g. `↑/k`), or `None` if the action is currently unbound. The
    /// chords are sorted because [`Keymap::chords_for`] draws from a `HashMap`
    /// whose iteration order is unspecified; without this the displayed hint
    /// would flicker between runs.
    pub fn display_for(&self, action: KeyAction) -> Option<String> {
        let mut chords = self.chords_for(action);
        if chords.is_empty() {
            return None;
        }
        chords.sort_by_key(chord_sort_key);
        Some(
            chords
                .iter()
                .map(KeyChord::display)
                .collect::<Vec<_>>()
                .join("/"),
        )
    }
}

/// A stable ordering key for displaying an action's chords: named/special keys
/// (arrows, `Enter`, …) sort before character keys, with the config `render()`
/// string as a deterministic tiebreaker.
fn chord_sort_key(chord: &KeyChord) -> (u8, String) {
    let bucket = u8::from(matches!(chord.code, KeyCode::Char(_)));
    (bucket, chord.render())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_names_round_trip_and_are_unique() {
        assert_eq!(KeyAction::ALL.len(), 25);
        let mut names = std::collections::HashSet::new();
        for action in KeyAction::ALL {
            assert_eq!(KeyAction::parse(action.name()), Some(action));
            assert!(
                names.insert(action.name()),
                "duplicate name {}",
                action.name()
            );
        }
        assert_eq!(KeyAction::parse("not-an-action"), None);
    }

    #[test]
    fn parse_modifiers_and_keys() {
        assert_eq!(KeyChord::parse("ctrl+u"), Some(KeyChord::ctrl('u')));
        assert_eq!(
            KeyChord::parse("alt+enter"),
            Some(KeyChord::normalized(KeyCode::Enter, KeyModifiers::ALT))
        );
        assert_eq!(KeyChord::parse("f5"), Some(KeyChord::key(KeyCode::F(5))));
        assert_eq!(
            KeyChord::parse("g"),
            Some(KeyChord::key(KeyCode::Char('g')))
        );
        assert_eq!(
            KeyChord::parse("G"),
            Some(KeyChord::key(KeyCode::Char('G')))
        );
        assert_eq!(
            KeyChord::parse("?"),
            Some(KeyChord::key(KeyCode::Char('?')))
        );
        assert_eq!(
            KeyChord::parse("+"),
            Some(KeyChord::key(KeyCode::Char('+')))
        );
        assert_eq!(
            KeyChord::parse("-"),
            Some(KeyChord::key(KeyCode::Char('-')))
        );
        assert_eq!(
            KeyChord::parse("space"),
            Some(KeyChord::key(KeyCode::Char(' ')))
        );
        assert_eq!(
            KeyChord::parse("PgUp"),
            Some(KeyChord::key(KeyCode::PageUp))
        );
    }

    #[test]
    fn parse_normalizes_shift_tab() {
        let want = KeyChord::normalized(KeyCode::Tab, KeyModifiers::SHIFT);
        assert_eq!(KeyChord::parse("shift+tab"), Some(want));
        assert_eq!(KeyChord::parse("backtab"), Some(want));
        assert_eq!(want.code, KeyCode::Tab);
        assert!(want.mods.contains(KeyModifiers::SHIFT));
    }

    #[test]
    fn parse_rejects_malformed() {
        assert_eq!(KeyChord::parse(""), None);
        assert_eq!(KeyChord::parse("ctrl+"), None);
        assert_eq!(KeyChord::parse("nope+x"), None);
        assert_eq!(KeyChord::parse("f99"), None);
        assert_eq!(KeyChord::parse("abc"), None);
    }

    #[test]
    fn from_event_normalizes() {
        let backtab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::empty());
        assert_eq!(
            KeyChord::from_event(backtab),
            KeyChord::normalized(KeyCode::Tab, KeyModifiers::SHIFT)
        );
        let shifted_g = KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT);
        assert_eq!(
            KeyChord::from_event(shifted_g),
            KeyChord::key(KeyCode::Char('G'))
        );
        let ctrl_u = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(KeyChord::from_event(ctrl_u), KeyChord::ctrl('u'));
    }

    #[test]
    fn render_round_trips() {
        for s in [
            "ctrl+u",
            "alt+enter",
            "shift+tab",
            "f5",
            "g",
            "G",
            "esc",
            "space",
        ] {
            let chord = KeyChord::parse(s).unwrap();
            assert_eq!(
                KeyChord::parse(&chord.render()),
                Some(chord),
                "round-trip {s}"
            );
        }
    }

    #[test]
    fn all_named_keycodes_round_trip() {
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Left,
            KeyCode::Right,
            KeyCode::Home,
            KeyCode::End,
            KeyCode::PageUp,
            KeyCode::PageDown,
            KeyCode::Enter,
            KeyCode::Esc,
            KeyCode::Tab,
            KeyCode::Backspace,
            KeyCode::Delete,
            KeyCode::Insert,
            KeyCode::Char(' '),
            KeyCode::Char('x'),
            KeyCode::F(12),
        ] {
            let chord = KeyChord::key(code);
            let rendered = chord.render();
            assert_eq!(
                KeyChord::parse(&rendered),
                Some(chord),
                "round-trip {rendered}"
            );
        }
    }

    #[test]
    fn defaults_cover_the_spec_table() {
        let m = Keymap::defaults();
        assert_eq!(
            m.action_for(KeyChord::key(KeyCode::Up)),
            Some(KeyAction::NavigateUp)
        );
        assert_eq!(
            m.action_for(KeyChord::key(KeyCode::Char('k'))),
            Some(KeyAction::NavigateUp)
        );
        assert_eq!(m.action_for(KeyChord::ctrl('u')), Some(KeyAction::PageUp));
        assert_eq!(
            m.action_for(KeyChord::key(KeyCode::Enter)),
            Some(KeyAction::Switch)
        );
        assert_eq!(
            m.action_for(KeyChord::normalized(KeyCode::Tab, KeyModifiers::SHIFT)),
            Some(KeyAction::FocusPrevPane)
        );
        assert_eq!(
            m.action_for(KeyChord::key(KeyCode::Char('?'))),
            Some(KeyAction::Help)
        );
        assert_eq!(
            m.action_for(KeyChord::key(KeyCode::Char('S'))),
            Some(KeyAction::SortReverse)
        );
        assert_eq!(
            m.action_for(KeyChord::key(KeyCode::Char('c'))),
            Some(KeyAction::Checkout)
        );
        assert_eq!(
            m.action_for(KeyChord::key(KeyCode::Char('y'))),
            Some(KeyAction::Sync)
        );
        assert_eq!(m.action_for(KeyChord::key(KeyCode::Char('z'))), None);
    }

    #[test]
    fn every_action_has_a_label() {
        for action in KeyAction::ALL {
            assert!(!action.label().is_empty(), "missing label for {action:?}");
        }
    }

    #[test]
    fn chord_display_is_terminal_pretty() {
        assert_eq!(KeyChord::key(KeyCode::Up).display(), "↑");
        assert_eq!(KeyChord::key(KeyCode::Down).display(), "↓");
        assert_eq!(KeyChord::key(KeyCode::Enter).display(), "Enter");
        assert_eq!(KeyChord::key(KeyCode::Esc).display(), "Esc");
        assert_eq!(KeyChord::key(KeyCode::Char('k')).display(), "k");
        assert_eq!(KeyChord::key(KeyCode::Char('?')).display(), "?");
        assert_eq!(KeyChord::ctrl('s').display(), "Ctrl-S");
        assert_eq!(
            KeyChord::normalized(KeyCode::Tab, KeyModifiers::SHIFT).display(),
            "Shift+Tab"
        );
    }

    #[test]
    fn display_for_is_sorted_and_deterministic() {
        let m = Keymap::defaults();
        // Multi-chord action: named key (↑) sorts before the char key (k).
        assert_eq!(m.display_for(KeyAction::NavigateUp).as_deref(), Some("↑/k"));
        assert_eq!(m.display_for(KeyAction::Switch).as_deref(), Some("Enter"));
        assert_eq!(m.display_for(KeyAction::SortCycle).as_deref(), Some("s"));
        assert_eq!(m.display_for(KeyAction::Checkout).as_deref(), Some("c"));
        // Stable across repeated calls despite the HashMap backing.
        assert_eq!(
            m.display_for(KeyAction::NavigateUp),
            m.display_for(KeyAction::NavigateUp)
        );
    }

    #[test]
    fn display_for_follows_rebind_and_is_none_when_unbound() {
        let mut m = Keymap::defaults();
        m.rebind(KeyAction::Checkout, KeyChord::key(KeyCode::Char('x')));
        assert_eq!(m.display_for(KeyAction::Checkout).as_deref(), Some("x"));
        // Rebind another action onto Checkout's last chord, leaving it unbound.
        m.rebind(KeyAction::Quit, KeyChord::key(KeyCode::Char('x')));
        assert_eq!(m.display_for(KeyAction::Checkout), None);
    }

    #[test]
    fn rebind_replaces_all_chords_for_action() {
        let mut m = Keymap::defaults();
        m.rebind(KeyAction::NavigateUp, KeyChord::key(KeyCode::Char('w')));
        assert_eq!(
            m.action_for(KeyChord::key(KeyCode::Char('w'))),
            Some(KeyAction::NavigateUp)
        );
        // The old bindings for the action are gone.
        assert_eq!(m.action_for(KeyChord::key(KeyCode::Char('k'))), None);
        assert_eq!(m.action_for(KeyChord::key(KeyCode::Up)), None);
        assert_eq!(m.chords_for(KeyAction::NavigateUp).len(), 1);
    }
}
