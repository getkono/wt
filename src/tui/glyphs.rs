//! Status/marker glyphs for the TUI (spec §10 Nerd Font support). The ASCII
//! fallbacks (the default) match the `wt list` markers; Nerd Font glyphs are an
//! optional, purely cosmetic alternative.

/// Braille busy-spinner frames (Nerd Font); cycled by a frame counter.
const SPINNER_NERD: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// ASCII busy-spinner frames (the default).
const SPINNER_ASCII: [&str; 4] = ["-", "\\", "|", "/"];

/// A glyph set, ASCII by default or Nerd Font when enabled.
pub struct Glyphs {
    nerd: bool,
}

impl Glyphs {
    /// Creates a glyph set.
    pub fn new(nerd: bool) -> Glyphs {
        Glyphs { nerd }
    }

    /// The per-field loading spinner.
    pub fn spinner(&self) -> &'static str {
        "…"
    }

    /// The animated busy-spinner frame for `tick`, wrapping around the active
    /// frame set (Nerd Font braille or the ASCII fallback). Drives the in-TUI
    /// overlay shown while a shell-based action runs (issue #46).
    pub fn spinner_frame(&self, tick: usize) -> &'static str {
        let set: &[&str] = if self.nerd {
            &SPINNER_NERD
        } else {
            &SPINNER_ASCII
        };
        set[tick % set.len()]
    }

    /// The number of frames in the active busy-spinner set.
    pub fn spinner_frame_count(&self) -> usize {
        if self.nerd {
            SPINNER_NERD.len()
        } else {
            SPINNER_ASCII.len()
        }
    }

    /// The "field unavailable" placeholder.
    pub fn absent(&self) -> &'static str {
        "–"
    }

    /// The current-worktree marker (`*`).
    pub fn current(&self) -> &'static str {
        if self.nerd { "▸" } else { "*" }
    }

    /// The missing-worktree marker (`!`).
    pub fn missing(&self) -> &'static str {
        if self.nerd { "✘" } else { "!" }
    }

    /// The detached-HEAD marker (`~`).
    pub fn detached(&self) -> &'static str {
        if self.nerd { "⚓" } else { "~" }
    }

    /// The worktree-less branch marker (a hollow `○`): a local branch listed
    /// beneath the worktrees with no checkout of its own (issue #47).
    pub fn branchless(&self) -> &'static str {
        if self.nerd { "" } else { "○" }
    }

    /// The modified marker (`M`).
    pub fn dirty(&self) -> &'static str {
        if self.nerd { "●" } else { "M" }
    }

    /// The untracked marker (`?`).
    pub fn untracked(&self) -> &'static str {
        "?"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_fallbacks_match_list_markers() {
        let g = Glyphs::new(false);
        assert_eq!(g.current(), "*");
        assert_eq!(g.missing(), "!");
        assert_eq!(g.detached(), "~");
        assert_eq!(g.branchless(), "○");
        assert_eq!(g.dirty(), "M");
        assert_eq!(g.untracked(), "?");
        assert_eq!(g.spinner(), "…");
        assert_eq!(g.absent(), "–");
    }

    #[test]
    fn nerd_fonts_differ_from_ascii() {
        let ascii = Glyphs::new(false);
        let nerd = Glyphs::new(true);
        assert_ne!(ascii.current(), nerd.current());
        assert_ne!(ascii.missing(), nerd.missing());
        assert_ne!(ascii.branchless(), nerd.branchless());
        // The spinner/absent are shared across both.
        assert_eq!(ascii.spinner(), nerd.spinner());
    }

    #[test]
    fn spinner_frame_wraps_and_advances() {
        for nerd in [false, true] {
            let g = Glyphs::new(nerd);
            let count = g.spinner_frame_count();
            // Wraps around after a full cycle.
            assert_eq!(g.spinner_frame(0), g.spinner_frame(count));
            // Consecutive frames differ within a cycle.
            assert_ne!(g.spinner_frame(0), g.spinner_frame(1));
        }
        assert_eq!(Glyphs::new(true).spinner_frame_count(), 10);
        assert_eq!(Glyphs::new(false).spinner_frame_count(), 4);
    }

    #[test]
    fn spinner_frames_differ_nerd_vs_ascii() {
        assert_ne!(
            Glyphs::new(true).spinner_frame(2),
            Glyphs::new(false).spinner_frame(2)
        );
    }
}
