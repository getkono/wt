//! `wt` — a Git worktree and GitHub PR manager (library crate).
//!
//! All real logic lives here so it is unit-testable and counted by coverage;
//! `src/main.rs` is a thin entry point. See `spec.md` for the full behavior
//! specification.
//!
//! The single entry point is [`run`], which takes the command-line arguments
//! and a [`Cx`] (injected I/O, environment, and working directory) and returns
//! the process exit code. Keeping the side-effecting handles in `Cx` makes the
//! whole dispatch path testable without touching the real terminal.

pub mod agent;
#[cfg(feature = "cli")]
pub(crate) mod cli;
#[cfg(feature = "cli")]
pub(crate) mod commands;
pub mod config;
pub mod copy;
pub mod cx;
pub mod error;
pub mod gh;
pub mod git;
pub mod hooks;
#[cfg(feature = "tui")]
pub mod keys;
pub mod model;
pub mod naming;
pub mod output;
/// Human-facing progress for slow foreground operations (CLI only).
#[cfg(feature = "cli")]
pub(crate) mod progress;
pub mod query;
pub mod slug;
pub mod template;
pub mod time;
#[cfg(feature = "tui")]
pub mod tui;
pub mod util;
pub mod version;
pub mod worktree;

#[cfg(test)]
mod testutil;

pub use cx::{Cx, Env, Stream};
pub use error::{Error, Result};

/// Arms the signal handlers that release the advisory repository lock.
///
/// The lock taken by [`worktree::Workspace`] (and every other `gix` tempfile)
/// is released by `Drop`, which a terminating signal skips — stranding
/// `wt-mutation.lock` so the next mutating command waits out its full timeout
/// and then fails. This installs handlers for `SIGINT`, `SIGTERM` and
/// `SIGQUIT` that remove those files and then re-raise the signal with its
/// default disposition, so the process still terminates as the caller expects.
/// `SIGHUP` is not covered.
///
/// The `wt` binary calls this at startup. Embedders that drive the worktree
/// API themselves should call it once, early: it must run before the first
/// tempfile is created, and only the first call takes effect.
pub fn install_signal_handlers() {
    // Route through `gix-lock`'s own re-export rather than a separate
    // `gix_tempfile::` path. The tempfile registry and the handler mode are
    // per-crate-instance statics, so arming any copy other than the one
    // `gix-lock` registers the lock with would silently do nothing.
    gix_lock::tempfile::signal::setup(Default::default());
}

/// Runs `wt` with the given command-line arguments (excluding `argv[0]`),
/// writing through the provided [`Cx`], and returns the process exit code.
#[cfg(feature = "cli")]
pub fn run(args: Vec<String>, cx: &mut Cx) -> u8 {
    let result = cli::dispatch(args, cx);
    finish(result, &mut cx.err)
}

/// Maps a command result to an exit code, reporting any error to `err`.
#[cfg(feature = "cli")]
fn finish(result: Result<u8>, err: &mut Stream) -> u8 {
    match result {
        Ok(code) => code,
        Err(e) => {
            let _ = err.line(&format!("error: {e}"));
            e.exit_code()
        }
    }
}

// The dispatch tests exercise the full application surface (`run` with no
// subcommand reaches the TUI), so they need the default feature set.
#[cfg(all(test, feature = "tui"))]
mod tests {
    use super::*;
    use crate::testutil::test_cx;

    #[test]
    fn finish_passes_through_success_code() {
        let mut t = test_cx(&[], "/tmp");
        assert_eq!(finish(Ok(0), &mut t.cx.err), 0);
        assert_eq!(finish(Ok(1), &mut t.cx.err), 1);
        assert!(t.err.contents().is_empty());
    }

    #[test]
    fn finish_reports_error_to_stderr_and_maps_code() {
        let mut t = test_cx(&[], "/tmp");
        let code = finish(Err(Error::usage("bad flag")), &mut t.cx.err);
        assert_eq!(code, 2);
        assert_eq!(t.err.contents(), "error: bad flag\n");
        assert!(t.out.contents().is_empty());
    }

    #[test]
    fn run_help_exits_zero_via_clap() {
        let mut t = test_cx(&[], "/tmp");
        assert_eq!(run(vec!["--help".to_string()], &mut t.cx), 0);
        assert!(t.out.contents().contains("Usage"));
    }

    #[test]
    fn run_maps_command_error_to_exit_code() {
        // No subcommand launches the TUI, which fails at discovery from a
        // non-repo dir: exit 1 with the NotInRepo message.
        let mut t = test_cx(&[], "/tmp");
        assert_eq!(run(vec![], &mut t.cx), 1);
        assert!(t.err.contents().contains("not in a git repository"));
    }
}

#[cfg(test)]
mod signal_handler_tests {
    /// Arming must be safe to repeat: only the first call takes effect, and a
    /// second must not panic or re-register.
    #[test]
    fn installing_signal_handlers_is_idempotent() {
        super::install_signal_handlers();
        super::install_signal_handlers();
    }
}
