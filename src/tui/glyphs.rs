//! Status/marker glyphs for the TUI (spec §10 Nerd Font support). The ASCII
//! fallbacks (the default) match the `wt list` markers; Nerd Font glyphs are an
//! optional, purely cosmetic alternative.

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
}
