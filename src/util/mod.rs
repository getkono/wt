//! Shared utilities: fuzzy matching, copy-on-write cloning, and editor
//! resolution.

pub mod editor;
#[cfg(feature = "tui")]
pub mod fuzzy;
pub mod reflink;
