//! The terminal UI (spec §10): a live dashboard and action center.
//!
//! The state and pure event handling live in [`app`] and [`event`]; the views
//! in [`view`]/[`glyphs`]/[`theme`]; and the terminal runtime (raw mode, the
//! event loop, async loading) in [`terminal`]/[`runtime`].

pub mod app;
pub mod event;
pub mod glyphs;
pub mod runtime;
pub mod terminal;
pub mod theme;
pub mod view;

pub use app::{App, Mode, Pane};
pub use event::Effect;
pub use runtime::{ComposeSeed, run_pr_compose, run_pr_picker, run_tui};
