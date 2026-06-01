//! Color output decision (spec §11 color precedence) and ANSI painting.

/// ANSI SGR codes used by `wt`'s human output.
pub mod ansi {
    /// Reset all attributes.
    pub const RESET: &str = "\x1b[0m";
    /// Red.
    pub const RED: &str = "\x1b[31m";
    /// Green.
    pub const GREEN: &str = "\x1b[32m";
    /// Yellow.
    pub const YELLOW: &str = "\x1b[33m";
    /// Cyan.
    pub const CYAN: &str = "\x1b[36m";
    /// Magenta.
    pub const MAGENTA: &str = "\x1b[35m";
    /// Dim.
    pub const DIM: &str = "\x1b[2m";
}

/// Wraps `text` in the SGR `code` when `enabled` (and `text` is not blank),
/// otherwise returns it unchanged. ANSI codes are zero display-width, so this is
/// safe to apply to already-laid-out cells.
pub fn paint(text: &str, code: &str, enabled: bool) -> String {
    if enabled && !text.trim().is_empty() {
        format!("{code}{text}{}", ansi::RESET)
    } else {
        text.to_string()
    }
}

/// How to colorize output, as selected by `--color` or the `ui.color` config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Color when the relevant stream is a TTY.
    Auto,
    /// Always colorize.
    Always,
    /// Never colorize.
    Never,
}

impl ColorChoice {
    /// Parses a `--color`/`ui.color` value (`auto`, `always`, `never`).
    pub fn parse(value: &str) -> Option<ColorChoice> {
        match value {
            "auto" => Some(ColorChoice::Auto),
            "always" => Some(ColorChoice::Always),
            "never" => Some(ColorChoice::Never),
            _ => None,
        }
    }
}

/// Resolves whether to emit ANSI color, following the spec §11 precedence
/// (first match wins):
/// 1. an explicit `--color always|never`;
/// 2. `NO_COLOR` set and non-empty → never;
/// 3. `ui.color` set to `always`/`never`;
/// 4. otherwise auto — color when stdout is a TTY.
///
/// An explicit `--color auto` (or no flag) and a `ui.color = "auto"` both fall
/// through to the next rule rather than forcing a decision.
pub fn resolve_color(
    flag: Option<ColorChoice>,
    no_color: bool,
    config: Option<ColorChoice>,
    stdout_is_tty: bool,
) -> bool {
    match flag {
        Some(ColorChoice::Always) => return true,
        Some(ColorChoice::Never) => return false,
        Some(ColorChoice::Auto) | None => {}
    }
    if no_color {
        return false;
    }
    match config {
        Some(ColorChoice::Always) => return true,
        Some(ColorChoice::Never) => return false,
        Some(ColorChoice::Auto) | None => {}
    }
    stdout_is_tty
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_and_unknown() {
        assert_eq!(ColorChoice::parse("auto"), Some(ColorChoice::Auto));
        assert_eq!(ColorChoice::parse("always"), Some(ColorChoice::Always));
        assert_eq!(ColorChoice::parse("never"), Some(ColorChoice::Never));
        assert_eq!(ColorChoice::parse("bogus"), None);
    }

    #[test]
    fn flag_always_wins_over_no_color() {
        assert!(resolve_color(
            Some(ColorChoice::Always),
            true,
            Some(ColorChoice::Never),
            false
        ));
    }

    #[test]
    fn flag_never_wins() {
        assert!(!resolve_color(
            Some(ColorChoice::Never),
            false,
            Some(ColorChoice::Always),
            true
        ));
    }

    #[test]
    fn no_color_env_beats_config_and_auto() {
        assert!(!resolve_color(None, true, Some(ColorChoice::Always), true));
        assert!(!resolve_color(Some(ColorChoice::Auto), true, None, true));
    }

    #[test]
    fn config_used_when_no_flag_or_no_color() {
        assert!(resolve_color(None, false, Some(ColorChoice::Always), false));
        assert!(!resolve_color(None, false, Some(ColorChoice::Never), true));
    }

    #[test]
    fn auto_falls_back_to_tty() {
        assert!(resolve_color(None, false, None, true));
        assert!(!resolve_color(None, false, None, false));
        assert!(resolve_color(
            Some(ColorChoice::Auto),
            false,
            Some(ColorChoice::Auto),
            true
        ));
    }
}
