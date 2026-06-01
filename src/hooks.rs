//! Post-create and pre-remove hooks (spec §8). Hooks run via `sh -c` (Unix) or
//! `cmd /C` (Windows) with the new worktree as the working directory and the
//! `WT_*` variables in the environment.
//!
//! Execution policy: a failed `post_create` is a non-fatal warning; a failed
//! `pre_remove` aborts the removal unless `--force` (then it is a warning).

use std::path::PathBuf;
use std::process::Command;

use crate::cx::Cx;
use crate::error::{Error, Result};

/// The context passed to a hook as `WT_*` environment variables.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// `WT_WORKTREE_PATH` and the working directory for the hook.
    pub worktree_path: PathBuf,
    /// `WT_BRANCH`.
    pub branch: String,
    /// `WT_REPO_ROOT`.
    pub repo_root: PathBuf,
    /// `WT_BASE_REF` (set only when known).
    pub base_ref: Option<String>,
    /// `WT_PR_NUMBER` (set only for PR-originated worktrees).
    pub pr_number: Option<u64>,
}

/// Runs hook commands. Abstracted so tests can inject a fake.
pub trait HookRunner {
    /// Runs `command` with the hook context, returning its exit code.
    fn run(&self, command: &str, ctx: &HookContext) -> Result<i32>;
}

/// The production [`HookRunner`] that spawns a shell.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealHookRunner;

impl HookRunner for RealHookRunner {
    fn run(&self, command: &str, ctx: &HookContext) -> Result<i32> {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", command]);
            c
        };
        cmd.current_dir(&ctx.worktree_path);
        cmd.env("WT_WORKTREE_PATH", &ctx.worktree_path);
        cmd.env("WT_BRANCH", &ctx.branch);
        cmd.env("WT_REPO_ROOT", &ctx.repo_root);
        if let Some(base) = &ctx.base_ref {
            cmd.env("WT_BASE_REF", base);
        }
        if let Some(pr) = ctx.pr_number {
            cmd.env("WT_PR_NUMBER", pr.to_string());
        }
        let status = cmd
            .status()
            .map_err(|e| Error::operation(format!("failed to run hook: {e}")))?;
        Ok(status.code().unwrap_or(-1))
    }
}

/// Runs the `post_create` hook (spec §8). A non-zero exit (or run failure) is a
/// non-fatal warning. `no_hooks` or an absent command is a no-op.
pub fn run_post_create(
    runner: &dyn HookRunner,
    cx: &mut Cx,
    command: Option<&str>,
    ctx: &HookContext,
    no_hooks: bool,
) -> Result<()> {
    if no_hooks {
        return Ok(());
    }
    let Some(command) = command else {
        return Ok(());
    };
    match runner.run(command, ctx) {
        Ok(0) => Ok(()),
        Ok(code) => {
            cx.err.line(&format!(
                "warning: post_create hook exited with status {code}"
            ))?;
            Ok(())
        }
        Err(e) => {
            cx.err
                .line(&format!("warning: post_create hook failed: {e}"))?;
            Ok(())
        }
    }
}

/// Runs the `pre_remove` hook (spec §8). A non-zero exit aborts the removal
/// unless `force` is set, in which case it is reported as a warning and removal
/// proceeds. `no_hooks` or an absent command is a no-op.
pub fn run_pre_remove(
    runner: &dyn HookRunner,
    cx: &mut Cx,
    command: Option<&str>,
    ctx: &HookContext,
    no_hooks: bool,
    force: bool,
) -> Result<()> {
    if no_hooks {
        return Ok(());
    }
    let Some(command) = command else {
        return Ok(());
    };
    match runner.run(command, ctx) {
        Ok(0) => Ok(()),
        Ok(code) if force => {
            cx.err.line(&format!(
                "warning: pre_remove hook exited with status {code}; proceeding due to --force"
            ))?;
            Ok(())
        }
        Ok(code) => Err(Error::operation(format!(
            "pre_remove hook exited with status {code}; aborting (use --force to override)"
        ))),
        Err(e) if force => {
            cx.err.line(&format!(
                "warning: pre_remove hook failed: {e}; proceeding due to --force"
            ))?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn ctx(dir: &std::path::Path) -> HookContext {
        HookContext {
            worktree_path: dir.to_path_buf(),
            branch: "feature/x".into(),
            repo_root: dir.to_path_buf(),
            base_ref: Some("main".into()),
            pr_number: None,
        }
    }

    /// A fake runner returning a fixed code and recording the command.
    struct FakeRunner {
        code: i32,
        last: Mutex<Option<String>>,
    }
    impl HookRunner for FakeRunner {
        fn run(&self, command: &str, _ctx: &HookContext) -> Result<i32> {
            *self.last.lock().unwrap() = Some(command.to_string());
            Ok(self.code)
        }
    }

    #[test]
    fn real_runner_sets_wt_env_and_returns_code() {
        let dir = tempfile::tempdir().unwrap();
        // post_create-style: write the environment to a file.
        let code = RealHookRunner
            .run("env | grep '^WT_' > wt_env.txt", &ctx(dir.path()))
            .unwrap();
        assert_eq!(code, 0);
        let env = std::fs::read_to_string(dir.path().join("wt_env.txt")).unwrap();
        assert!(env.contains("WT_BRANCH=feature/x"));
        assert!(env.contains("WT_REPO_ROOT="));
        assert!(env.contains("WT_BASE_REF=main"));
        assert!(env.contains("WT_WORKTREE_PATH="));
        // WT_PR_NUMBER is unset when there is no PR.
        assert!(!env.contains("WT_PR_NUMBER"));
    }

    #[test]
    fn real_runner_sets_pr_number_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = ctx(dir.path());
        c.pr_number = Some(123);
        RealHookRunner
            .run("printenv WT_PR_NUMBER > pr.txt", &c)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pr.txt"))
                .unwrap()
                .trim(),
            "123"
        );
    }

    #[test]
    fn real_runner_propagates_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(RealHookRunner.run("exit 3", &ctx(dir.path())).unwrap(), 3);
    }

    #[test]
    fn post_create_failure_is_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner {
            code: 1,
            last: Mutex::new(None),
        };
        let mut t = crate::testutil::test_cx(&[], "/tmp");
        run_post_create(
            &runner,
            &mut t.cx,
            Some("do-thing"),
            &ctx(dir.path()),
            false,
        )
        .unwrap();
        assert!(
            t.err
                .contents()
                .contains("warning: post_create hook exited with status 1")
        );
    }

    #[test]
    fn post_create_skipped_when_no_hooks_or_absent() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner {
            code: 1,
            last: Mutex::new(None),
        };
        let mut t = crate::testutil::test_cx(&[], "/tmp");
        run_post_create(&runner, &mut t.cx, Some("x"), &ctx(dir.path()), true).unwrap();
        run_post_create(&runner, &mut t.cx, None, &ctx(dir.path()), false).unwrap();
        assert!(runner.last.lock().unwrap().is_none());
        assert!(t.err.contents().is_empty());
    }

    #[test]
    fn pre_remove_failure_aborts_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner {
            code: 2,
            last: Mutex::new(None),
        };
        let mut t = crate::testutil::test_cx(&[], "/tmp");
        let err = run_pre_remove(
            &runner,
            &mut t.cx,
            Some("guard"),
            &ctx(dir.path()),
            false,
            false,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn pre_remove_failure_warns_and_proceeds_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner {
            code: 2,
            last: Mutex::new(None),
        };
        let mut t = crate::testutil::test_cx(&[], "/tmp");
        run_pre_remove(
            &runner,
            &mut t.cx,
            Some("guard"),
            &ctx(dir.path()),
            false,
            true,
        )
        .unwrap();
        assert!(t.err.contents().contains("proceeding due to --force"));
    }

    #[test]
    fn pre_remove_success_proceeds() {
        let dir = tempfile::tempdir().unwrap();
        let runner = FakeRunner {
            code: 0,
            last: Mutex::new(None),
        };
        let mut t = crate::testutil::test_cx(&[], "/tmp");
        run_pre_remove(
            &runner,
            &mut t.cx,
            Some("guard"),
            &ctx(dir.path()),
            false,
            false,
        )
        .unwrap();
        assert_eq!(runner.last.lock().unwrap().as_deref(), Some("guard"));
    }
}
