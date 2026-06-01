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

pub mod cx;
pub mod error;
pub mod output;

#[cfg(test)]
mod testutil;

pub use cx::{Cx, Env, Stream};
pub use error::{Error, Result};

/// Runs `wt` with the given command-line arguments (excluding `argv[0]`),
/// writing through the provided [`Cx`], and returns the process exit code.
pub fn run(args: Vec<String>, cx: &mut Cx) -> u8 {
    let result = dispatch(args, cx);
    finish(result, &mut cx.err)
}

/// Dispatches the parsed command. Stage 2 wires up `clap` and the subcommands;
/// for now an empty invocation succeeds with no output.
fn dispatch(_args: Vec<String>, _cx: &mut Cx) -> Result<u8> {
    Ok(0)
}

/// Maps a command result to an exit code, reporting any error to `err`.
fn finish(result: Result<u8>, err: &mut Stream) -> u8 {
    match result {
        Ok(code) => code,
        Err(e) => {
            let _ = err.line(&format!("error: {e}"));
            e.exit_code()
        }
    }
}

#[cfg(test)]
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
    fn run_empty_args_succeeds_silently() {
        let mut t = test_cx(&[], "/tmp");
        assert_eq!(run(vec![], &mut t.cx), 0);
        assert!(t.out.contents().is_empty());
        assert!(t.err.contents().is_empty());
    }
}
