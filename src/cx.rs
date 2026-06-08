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

use crate::agent::AgentClient;
use crate::error::Result;
use crate::gh::GhClient;
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
#[derive(Clone)]
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
    /// The `gh` subprocess handle (real, or a fake in tests).
    pub gh: Arc<dyn GhClient + Send + Sync>,
    /// The code-agent subprocess handle (real, or a fake in tests). Drives a
    /// code agent (e.g. `claude`) to draft PR content; see [`AgentClient`].
    pub agent: Arc<dyn AgentClient + Send + Sync>,
    /// Interactive input source for confirmation prompts.
    pub input: Box<dyn Input + Send>,
    /// The `--color` flag value, if given (set during dispatch).
    pub color_flag: Option<crate::output::color::ColorChoice>,
    /// The `--no-pager` flag (set during dispatch).
    pub no_pager: bool,
    /// The `-v`/`--verbose` count: extra diagnostics to stderr (set during
    /// dispatch). `0` is the default (quiet).
    pub verbose: u8,
}

impl Cx {
    /// Builds a context from injected streams, environment, working dir, the
    /// `git`/`gh`/`agent` handles, and the input source. The global flag fields
    /// (`color_flag`, `no_pager`) default off and are set during dispatch.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        out: Stream,
        err: Stream,
        env: Env,
        cwd: PathBuf,
        git: Arc<dyn GitCli + Send + Sync>,
        gh: Arc<dyn GhClient + Send + Sync>,
        agent: Arc<dyn AgentClient + Send + Sync>,
        input: Box<dyn Input + Send>,
    ) -> Self {
        Self {
            out,
            err,
            env,
            cwd,
            git,
            gh,
            agent,
            input,
            color_flag: None,
            no_pager: false,
            verbose: 0,
        }
    }

    /// Resolves whether to emit color for stdout, given the resolved config's
    /// `ui.color` (spec §11 precedence).
    pub fn color_enabled(&self, ui_color: crate::output::color::ColorChoice) -> bool {
        crate::output::color::resolve_color(
            self.color_flag,
            self.env.is_set_nonempty("NO_COLOR"),
            Some(ui_color),
            self.out.is_tty(),
        )
    }

    /// Resolves whether to emit color for the TUI, which draws to the alternate
    /// screen on stderr. The precedence (`--color`, `NO_COLOR`, `ui.color`) is
    /// the same as [`Cx::color_enabled`], but `auto` follows stderr's TTY status
    /// rather than stdout's (stdout is reserved for the chosen path and is
    /// usually piped, e.g. `cd "$(wt)"`).
    pub fn color_enabled_err(&self, ui_color: crate::output::color::ColorChoice) -> bool {
        crate::output::color::resolve_color(
            self.color_flag,
            self.env.is_set_nonempty("NO_COLOR"),
            Some(ui_color),
            self.err.is_tty(),
        )
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
    fn color_enabled_err_follows_stderr_tty() {
        use crate::output::color::ColorChoice;
        // stdout not a TTY (piped), stderr a TTY (where the TUI draws).
        let mut t = test_cx(&[], "/work");
        t.cx.err = Stream::new(Box::new(SharedBuf::new()), true);
        // `auto` resolves against stderr for the TUI, but stdout for the CLI.
        assert!(t.cx.color_enabled_err(ColorChoice::Auto));
        assert!(!t.cx.color_enabled(ColorChoice::Auto));
        // `never`/`always` and NO_COLOR keep the usual precedence.
        assert!(!t.cx.color_enabled_err(ColorChoice::Never));
        t.cx.color_flag = Some(ColorChoice::Always);
        assert!(t.cx.color_enabled_err(ColorChoice::Never));
    }

    #[test]
    fn color_enabled_err_honors_no_color() {
        use crate::output::color::ColorChoice;
        let mut t = test_cx(&[("NO_COLOR", "1")], "/work");
        t.cx.err = Stream::new(Box::new(SharedBuf::new()), true);
        assert!(!t.cx.color_enabled_err(ColorChoice::Always));
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
