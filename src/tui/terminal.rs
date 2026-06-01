//! Terminal lifecycle for the TUI (spec §10): raw mode + alternate screen on
//! stderr (stdout stays reserved for the chosen path, §5), a panic hook that
//! restores the terminal, and suspend/resume for the foreground editor.
//!
//! This module is the deliberately-thin, terminal-touching shell of the TUI;
//! all decisions live in the tested [`crate::tui::app`]/[`crate::tui::event`].

use std::io::{Stderr, stderr};

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::error::Result;
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
    pub fn enter(mouse: bool) -> Result<Tui> {
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
        execute!(stderr(), EnterAlternateScreen)?;
        if self.mouse {
            execute!(stderr(), EnableMouseCapture)?;
        }
        self.terminal.clear()?;
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
