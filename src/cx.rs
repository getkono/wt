//! Runtime context: injected I/O, environment, and working directory.
//!
//! [`Cx`] threads every side-effecting handle through command dispatch so the
//! library is unit-testable: tests build a `Cx` over in-memory buffers and a
//! fixed environment, run a command, then inspect what was written. The binary
//! builds a `Cx` over real stdio and the process environment.
//!
//! The stdout/stderr split is the spec §5 contract: navigation paths, JSON, and
//! data-command results go to [`Cx::out`]; all human-facing text, prompts,
//! logs, and errors go to [`Cx::err`].

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::Result;
use crate::git::cli::GitCli;

/// A writable output stream (stdout or stderr) tagged with whether it is a TTY.
pub struct Stream {
    writer: Box<dyn Write + Send>,
    is_tty: bool,
}

impl Stream {
    /// Wraps a writer, recording whether it is connected to a terminal.
    pub fn new(writer: Box<dyn Write + Send>, is_tty: bool) -> Self {
        Self { writer, is_tty }
    }

    /// Returns `true` if this stream is connected to a terminal.
    pub fn is_tty(&self) -> bool {
        self.is_tty
    }

    /// Writes `s` followed by a newline.
    pub fn line(&mut self, s: &str) -> Result<()> {
        writeln!(self.writer, "{s}")?;
        Ok(())
    }

    /// Writes `s` with no trailing newline.
    pub fn text(&mut self, s: &str) -> Result<()> {
        write!(self.writer, "{s}")?;
        Ok(())
    }

    /// Flushes any buffered output.
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush()?;
        Ok(())
    }
}

/// A source of interactive input lines (e.g. `y/N` confirmations), injectable
/// for testing.
pub trait Input {
    /// Reads one line of input, including any trailing newline. An empty string
    /// signals end-of-input.
    fn read_line(&mut self) -> Result<String>;
}

/// The production [`Input`] that reads from standard input.
pub struct StdinInput;

impl Input for StdinInput {
    fn read_line(&mut self) -> Result<String> {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Ok(line)
    }
}

/// A snapshot of environment variables, injectable for testing.
pub struct Env {
    vars: HashMap<String, String>,
}

impl Env {
    /// Builds an `Env` from an explicit map (used in tests).
    pub fn from_map(vars: HashMap<String, String>) -> Self {
        Self { vars }
    }

    /// Captures the current process environment.
    pub fn from_real() -> Self {
        Self {
            vars: std::env::vars().collect(),
        }
    }

    /// Returns the value of `key`, if set.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    /// Returns `true` if `key` is set to a non-empty value.
    pub fn is_set_nonempty(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| !v.is_empty())
    }
}

/// The runtime context threaded through command dispatch.
pub struct Cx {
    /// Standard output: navigation paths, JSON, and data-command results only.
    pub out: Stream,
    /// Standard error: all human-facing text, prompts, logs, and errors.
    pub err: Stream,
    /// A snapshot of the process environment.
    pub env: Env,
    /// The effective working directory (after any `-C`).
    pub cwd: PathBuf,
    /// The `git` subprocess handle (real, or a fake in tests). Shared via `Arc`
    /// so the TUI can clone it into async tasks.
    pub git: Arc<dyn GitCli + Send + Sync>,
    /// Interactive input source for confirmation prompts.
    pub input: Box<dyn Input + Send>,
}

impl Cx {
    /// Builds a context from injected streams, environment, working dir, the
    /// `git` handle, and the input source.
    pub fn new(
        out: Stream,
        err: Stream,
        env: Env,
        cwd: PathBuf,
        git: Arc<dyn GitCli + Send + Sync>,
        input: Box<dyn Input + Send>,
    ) -> Self {
        Self {
            out,
            err,
            env,
            cwd,
            git,
            input,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{SharedBuf, test_cx};

    #[test]
    fn stream_writes_line_and_text() {
        let buf = SharedBuf::new();
        let mut s = Stream::new(Box::new(buf.clone()), false);
        s.text("a").unwrap();
        s.line("b").unwrap();
        s.flush().unwrap();
        assert_eq!(buf.contents(), "ab\n");
        assert!(!s.is_tty());
    }

    #[test]
    fn stream_reports_tty_flag() {
        let s = Stream::new(Box::new(SharedBuf::new()), true);
        assert!(s.is_tty());
    }

    #[test]
    fn env_get_and_nonempty() {
        let env = Env::from_map(
            [
                ("A".to_string(), "1".to_string()),
                ("E".to_string(), String::new()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(env.get("A"), Some("1"));
        assert_eq!(env.get("MISSING"), None);
        assert!(env.is_set_nonempty("A"));
        assert!(!env.is_set_nonempty("E"));
        assert!(!env.is_set_nonempty("MISSING"));
    }

    #[test]
    fn cx_exposes_streams_env_cwd() {
        let mut t = test_cx(&[("X", "y")], "/work");
        t.cx.out.line("path").unwrap();
        t.cx.err.line("note").unwrap();
        assert_eq!(t.out.contents(), "path\n");
        assert_eq!(t.err.contents(), "note\n");
        assert_eq!(t.cx.env.get("X"), Some("y"));
        assert_eq!(t.cx.cwd, PathBuf::from("/work"));
    }
}
