//! The `git` subprocess boundary (spec §4): state-mutating and network
//! operations shell out to `git`. The [`GitCli`] trait isolates this so tests
//! can inject a fake; [`RealGit`] is the production implementation.

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// The captured result of running a `git` subprocess.
#[derive(Debug, Clone)]
pub struct GitOutput {
    /// Whether the process exited successfully (status `0`).
    pub success: bool,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

/// Runs `git` subcommands in a repository. Mutations and network operations go
/// through this boundary (reads use `gix` where possible).
pub trait GitCli {
    /// Runs `git -C <repo> <args>`, capturing output. Returns the captured
    /// [`GitOutput`] even on a non-zero exit; errors only if the process cannot
    /// be spawned.
    fn run_raw(&self, repo: &Path, args: &[&str]) -> Result<GitOutput>;

    /// Runs `git`, returning stdout on success or an [`Error::Subprocess`] with
    /// captured stderr on failure. Lock contention is annotated with a hint
    /// (spec §12).
    fn run(&self, repo: &Path, args: &[&str]) -> Result<String> {
        let out = self.run_raw(repo, args)?;
        if out.success {
            return Ok(out.stdout);
        }
        let mut stderr = out.stderr.trim_end().to_string();
        if stderr.contains(".lock") {
            stderr.push_str("\n(hint: another git process holds the worktree lock)");
        }
        Err(Error::Subprocess {
            program: "git".into(),
            stderr,
        })
    }
}

/// The production [`GitCli`] that spawns the real `git` binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealGit;

impl GitCli for RealGit {
    fn run_raw(&self, repo: &Path, args: &[&str]) -> Result<GitOutput> {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .map_err(|e| Error::Subprocess {
                program: "git".into(),
                stderr: format!("failed to run git: {e}"),
            })?;
        Ok(GitOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `GitCli` returning canned output, for testing the `run` wrapper.
    struct Canned(GitOutput);
    impl GitCli for Canned {
        fn run_raw(&self, _repo: &Path, _args: &[&str]) -> Result<GitOutput> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn run_returns_stdout_on_success() {
        let git = Canned(GitOutput {
            success: true,
            stdout: "ok\n".into(),
            stderr: String::new(),
        });
        assert_eq!(git.run(Path::new("/r"), &["status"]).unwrap(), "ok\n");
    }

    #[test]
    fn run_surfaces_stderr_on_failure() {
        let git = Canned(GitOutput {
            success: false,
            stdout: String::new(),
            stderr: "fatal: nope\n".into(),
        });
        let err = git.run(Path::new("/r"), &["x"]).unwrap_err();
        match err {
            Error::Subprocess { program, stderr } => {
                assert_eq!(program, "git");
                assert_eq!(stderr, "fatal: nope");
            }
            other => panic!("expected subprocess error, got {other:?}"),
        }
    }

    #[test]
    fn run_annotates_lock_contention() {
        let git = Canned(GitOutput {
            success: false,
            stdout: String::new(),
            stderr: "fatal: could not lock .git/worktrees/x/HEAD.lock".into(),
        });
        let err = git.run(Path::new("/r"), &["worktree", "add"]).unwrap_err();
        assert!(
            err.to_string()
                .contains("another git process holds the worktree lock")
        );
    }

    #[test]
    fn real_git_runs_version() {
        // Smoke test that the real binary is reachable in the test environment.
        let out = RealGit.run_raw(Path::new("."), &["--version"]).unwrap();
        assert!(out.success);
        assert!(out.stdout.contains("git version"));
    }
}
