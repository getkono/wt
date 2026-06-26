//! Terminal lifecycle for the TUI (spec §10): raw mode + alternate screen on
//! stderr (stdout stays reserved for the chosen path, §5), a panic hook that
//! restores the terminal, and suspend/resume for the foreground editor.
//!
//! This module is the deliberately-thin, terminal-touching shell of the TUI;
//! all decisions live in the tested [`crate::tui::app`]/[`crate::tui::event`].

use std::io::{IsTerminal, Stderr, stderr};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::error::{Error, Result};
use crate::tui::App;
use crate::tui::view;

/// The ratatui backend over stderr.
type Backend = CrosstermBackend<Stderr>;

/// An owned terminal in raw mode + alternate screen, restored on drop.
pub struct Tui {
    terminal: Terminal<Backend>,
    mouse: bool,
}

impl Tui {
    /// Enters raw mode and the alternate screen (with mouse capture if enabled).
    ///
    /// Errors (without touching the terminal) when stderr is not a real
    /// terminal. The TUI draws to stderr, and `enable_raw_mode` performs a
    /// `tcsetattr` on the controlling terminal — so taking it over is only safe
    /// when stderr is genuinely a terminal. Enforcing that precondition *here*,
    /// at the irreversible boundary, rather than relying solely on the
    /// higher-level [`crate::cx::Cx`] TTY gate, keeps any non-TTY run (tests,
    /// pipes, or a `cargo mutants` child in a background process group) from
    /// being stopped by `SIGTTOU` and wedging indefinitely.
    pub fn enter(mouse: bool) -> Result<Tui> {
        if !stderr().is_terminal() {
            return Err(Error::operation(
                "refusing to start the TUI: stderr is not a terminal",
            ));
        }
        enable_raw_mode()?;
        execute!(stderr(), EnterAlternateScreen)?;
        if mouse {
            execute!(stderr(), EnableMouseCapture)?;
        }
        let terminal = Terminal::new(CrosstermBackend::new(stderr()))?;
        Ok(Tui { terminal, mouse })
    }

    /// Draws the current app state.
    pub fn draw(&mut self, app: &App) -> Result<()> {
        self.terminal.draw(|frame| view::render(app, frame))?;
        Ok(())
    }

    /// Leaves raw mode / alt screen to run a foreground program (e.g. editor).
    pub fn suspend(&mut self) -> Result<()> {
        restore(self.mouse)
    }

    /// Re-enters raw mode / alt screen after [`Tui::suspend`].
    pub fn resume(&mut self) -> Result<()> {
        enable_raw_mode()?;
        // Clear the alternate screen with a plain escape (no cursor read).
        execute!(stderr(), EnterAlternateScreen, Clear(ClearType::All))?;
        if self.mouse {
            execute!(stderr(), EnableMouseCapture)?;
        }
        // Recreate the terminal to force a full repaint on the next draw without
        // a cursor-position query: ratatui ≥0.30.1's `Terminal::clear` reads the
        // cursor (ESC[6n) on stdout, but `wt`'s stdout is captured by the shell
        // wrapper, so the reply never arrives and crossterm times out (#36). A
        // fresh fullscreen `Terminal` resets the diff buffers via `backend.size()`
        // (ioctl) only — no cursor read.
        self.terminal = Terminal::new(CrosstermBackend::new(stderr()))?;
        Ok(())
    }

    /// The current terminal size (cols, rows).
    pub fn size(&self) -> (u16, u16) {
        self.terminal
            .size()
            .map(|s| (s.width, s.height))
            .unwrap_or((100, 30))
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = restore(self.mouse);
    }
}

/// Restores the terminal to its normal state (idempotent, best-effort).
fn restore(mouse: bool) -> Result<()> {
    if mouse {
        let _ = execute!(stderr(), DisableMouseCapture);
    }
    let _ = execute!(stderr(), LeaveAlternateScreen);
    disable_raw_mode()?;
    Ok(())
}

/// Installs a panic hook that restores the terminal before the default hook
/// runs, so a panic never leaves the terminal in raw mode (spec §10).
pub fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore(true);
        original(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_refuses_when_stderr_is_not_a_terminal() {
        // `cargo test` captures stderr, so it is not a terminal: entering the
        // TUI must fail fast instead of driving raw mode on a non-terminal —
        // which under a background process group (e.g. a `cargo mutants` child)
        // would raise SIGTTOU and hang the run. Guard against the rare case of
        // running attached to a real terminal (e.g. `--nocapture` from a tty),
        // where grabbing it would be both unwanted and disruptive.
        if stderr().is_terminal() {
            return;
        }
        assert!(Tui::enter(false).is_err());
    }
}
