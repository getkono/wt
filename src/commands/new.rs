//! `wt new <branch>` — create a linked worktree (spec §6/§7/§8/§13).

use crate::cli::NewArgs;
use crate::commands::{Nav, finish_worktree, maybe_init_submodules_interactive, open_session};
use crate::cx::Cx;
use crate::error::Result;
use crate::git::discover::Repo;
use crate::git::{branch_ref, resolve_hex};
use crate::hooks::{HookContext, HookRunner};
use crate::worktree::{CreateOptions, HookOutcome, create_in, resolve_base};

/// Creates a linked worktree for `branch`, prompting first when the base it would
/// fork from is behind its origin counterpart (issue #56): the user can update the
/// base, proceed off the stale base, or cancel. The check is skipped offline, for
/// an existing branch, or when the base has no upstream. Delegates to [`run_core`]
/// for the actual creation.
pub(crate) fn run(cx: &mut Cx, hooks: &dyn HookRunner, args: &NewArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    // Pre-flight staleness check in its own scope so the session is dropped before
    // `run_core` opens its own.
    {
        let session = open_session(cx, git)?;
        if let Some(base) = prospective_base(cx, &session.repo, args, &session.config) {
            let dir = session
                .repo
                .current_workdir()
                .unwrap_or_else(|| session.primary_root.clone());
            if let Some(stale) =
                crate::commands::staleness::check_base_behind(cx, git, &session.repo, &dir, &base)?
            {
                let prompt = format!(
                    "base {base:?} is {} commit(s) behind {}; [u]pdate / [p]roceed / [c]ancel (default cancel): ",
                    stale.behind, stale.upstream_display
                );
                match crate::commands::choose(cx, &prompt)? {
                    crate::commands::Choice::Update => {
                        crate::commands::staleness::fast_forward_base(
                            cx,
                            git,
                            &session.repo,
                            &session.primary_root,
                            &base,
                            &stale,
                        )?
                    }
                    crate::commands::Choice::Proceed => {}
                    crate::commands::Choice::Cancel => {
                        cx.err.line("aborted: base branch is behind origin")?;
                        return Ok(1);
                    }
                }
            }
        }
    }
    // The CLI may prompt before initializing submodules (issue #50); the TUI
    // passes `false` to `run_core` and drives its own modal instead.
    run_core(cx, hooks, args, json, true)
}

/// Creates a linked worktree for `branch` (creating the branch if needed), runs
/// the copy step and post-create hook, and prints the new path (unless
/// `--no-switch`/`--json`). The base-staleness check (issue #56) is the caller's
/// responsibility — [`run`] does it for the CLI, the TUI before this is reached.
/// `prompt` enables the interactive submodule confirmation (issue #50): the CLI
/// passes `true`; the TUI passes `false` and runs its own modal afterwards.
pub(crate) fn run_core(
    cx: &mut Cx,
    hooks: &dyn HookRunner,
    args: &NewArgs,
    json: bool,
    prompt: bool,
) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let root = session.primary_root.clone();

    // Resolve the base for a new branch up front so the HEAD-fallback warning
    // (in `prospective_base`) still lands before anything is created.
    let base = prospective_base(cx, &session.repo, args, &session.config);
    let options = CreateOptions {
        branch: args.branch.clone(),
        base,
        track: args.track.clone(),
        copy_from: args.copy_from.clone(),
        // The service never prompts; the interactive policy handling below
        // (issue #50) decides about submodules on the CLI/TUI paths.
        init_submodules: false,
        no_hooks: args.no_hooks,
    };

    let env = cx.env.clone();
    let created = create_in(&session.parts(&env), git, hooks, &options)?;

    crate::commands::log_copy_outcome(cx, &created.copy);
    // The post-create hook already ran in the service; a failure is a warning,
    // not a rollback (§8).
    match &created.post_create {
        HookOutcome::ExitedNonZero(code) => {
            cx.err.line(&format!(
                "warning: post_create hook exited with status {code}"
            ))?;
        }
        HookOutcome::Failed(error) => {
            cx.err
                .line(&format!("warning: post_create hook failed: {error}"))?;
        }
        HookOutcome::Skipped | HookOutcome::Succeeded => {}
    }

    // Initialize submodules per the policy/flag, prompting (default yes) at an
    // interactive terminal when the policy is left at its default (issue #50).
    // Non-fatal — the worktree already exists. The idempotent reuse path skips
    // this, exactly as it always has.
    if !created.reused {
        maybe_init_submodules_interactive(
            cx,
            git,
            &created.path,
            session.config.submodules_init,
            args.submodule_override(),
            prompt,
        )?;
    }

    let ctx = HookContext {
        worktree_path: created.path.clone(),
        branch: created.branch.clone(),
        repo_root: root,
        base_ref: created.base_ref.clone(),
        pr_number: None,
    };
    finish_worktree(
        cx,
        hooks,
        &created.path,
        &ctx,
        Nav {
            json,
            no_switch: args.no_switch,
            // Idempotent: the worktree is already initialized, so `--start`
            // runs on the reuse path too.
            note: if created.reused {
                "worktree already exists at"
            } else {
                "created worktree at"
            },
            start: args.start.as_deref(),
        },
    )
}

/// The base ref a `new` invocation would fork from, for the pre-flight staleness
/// check (issue #56), or `None` when the branch already exists — then there is no
/// fork and no base to check.
pub(crate) fn prospective_base(
    cx: &mut Cx,
    repo: &Repo,
    args: &NewArgs,
    config: &crate::config::Config,
) -> Option<String> {
    if resolve_hex(repo.gix(), &branch_ref(&args.branch)).is_some() {
        return None;
    }
    let (base, defaulted_to_head) = resolve_base(repo, config, args.from.as_deref());
    if defaulted_to_head {
        let _ = cx
            .err
            .line("warning: no default branch; basing the new branch on HEAD");
    }
    Some(base)
}

/// Detects whether the base `args` would fork from is behind its upstream (issue
/// #56), for the TUI create pre-flight. `Ok(None)` when there is nothing to warn
/// about (existing branch, no upstream, up to date, or offline).
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub(crate) fn detect_stale_base(
    cx: &mut Cx,
    args: &NewArgs,
) -> Result<Option<crate::commands::staleness::StaleBase>> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let Some(base) = prospective_base(cx, &session.repo, args, &session.config) else {
        return Ok(None);
    };
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    crate::commands::staleness::check_base_behind(cx, git, &session.repo, &dir, &base)
}

/// Fast-forwards the base `args` would fork from to its upstream (issue #56, the
/// TUI "update" action). A no-op when there is no stale base.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub(crate) fn update_stale_base(cx: &mut Cx, args: &NewArgs) -> Result<()> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let Some(base) = prospective_base(cx, &session.repo, args, &session.config) else {
        return Ok(());
    };
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());
    if let Some(stale) =
        crate::commands::staleness::check_base_behind(cx, git, &session.repo, &dir, &base)?
    {
        crate::commands::staleness::fast_forward_base(
            cx,
            git,
            &session.repo,
            &session.primary_root,
            &base,
            &stale,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::NewArgs;
    use crate::hooks::RealHookRunner;
    use crate::testutil::TestRepo;
    use std::path::Path;

    fn args(branch: &str) -> NewArgs {
        NewArgs {
            branch: branch.to_string(),
            from: None,
            track: None,
            no_track: false,
            no_switch: false,
            no_hooks: true,
            start: None,
            copy_from: None,
            init_submodules: false,
            no_init_submodules: false,
        }
    }

    fn run(repo: &TestRepo, a: &NewArgs, json: bool) -> (u8, String, String) {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &RealHookRunner, a, json).unwrap();
        (code, t.out.contents(), t.err.contents())
    }

    /// `--start` runs in the new worktree, *after* the post-create hook, with the
    /// `WT_*` context set. stdout stays clean — it belongs to the command — and the
    /// path is handed to the shell through `$WT_CD_FILE`.
    #[test]
    fn start_runs_in_the_worktree_after_the_hook() {
        let repo = TestRepo::init();
        repo.write(
            ".wt.toml",
            "[hooks]\npost_create = \"echo hook >> order.txt\"\n",
        );
        repo.commit_all("config");

        let cd_dir = tempfile::tempdir().unwrap();
        let cd_file = cd_dir.path().join("cd");
        let mut t = crate::testutil::test_cx(
            &[("WT_CD_FILE", cd_file.to_str().unwrap())],
            repo.root().to_str().unwrap(),
        );
        let mut a = args("feat/x");
        a.no_hooks = false;
        a.start = Some("echo \"start $WT_BRANCH\" >> order.txt".into());
        let code = super::run(&mut t.cx, &RealHookRunner, &a, false).unwrap();

        assert_eq!(code, 0);
        assert!(t.out.contents().is_empty(), "stdout is the command's");
        assert!(t.err.contents().contains("created worktree at"));

        let target = std::fs::read_to_string(&cd_file).unwrap();
        assert!(Path::new(&target).is_dir());
        // The hook ran first, then `--start`, both inside the new worktree.
        let order = std::fs::read_to_string(Path::new(&target).join("order.txt")).unwrap();
        assert_eq!(order, "hook\nstart feat/x\n");
    }

    /// A failing `--start` command becomes `wt`'s exit code, and the worktree is
    /// still handed to the shell — you land in it to debug.
    #[test]
    fn start_propagates_its_exit_code_and_still_hands_over_the_path() {
        let repo = TestRepo::init();
        let cd_dir = tempfile::tempdir().unwrap();
        let cd_file = cd_dir.path().join("cd");
        let mut t = crate::testutil::test_cx(
            &[("WT_CD_FILE", cd_file.to_str().unwrap())],
            repo.root().to_str().unwrap(),
        );
        let mut a = args("feat/x");
        a.start = Some("exit 7".into());
        let code = super::run(&mut t.cx, &RealHookRunner, &a, false).unwrap();

        assert_eq!(code, 7);
        assert!(Path::new(&std::fs::read_to_string(&cd_file).unwrap()).is_dir());
    }

    /// `wt new x --start cmd` behaves the same whether or not the worktree already
    /// exists: the command runs on the idempotent path too.
    #[test]
    fn start_runs_on_the_already_exists_path() {
        let repo = TestRepo::init();
        let mut first = args("feat/x");
        first.no_switch = true;
        run(&repo, &first, false);

        let cd_dir = tempfile::tempdir().unwrap();
        let cd_file = cd_dir.path().join("cd");
        let mut t = crate::testutil::test_cx(
            &[("WT_CD_FILE", cd_file.to_str().unwrap())],
            repo.root().to_str().unwrap(),
        );
        let mut a = args("feat/x");
        a.start = Some("echo ran > marker.txt".into());
        let code = super::run(&mut t.cx, &RealHookRunner, &a, false).unwrap();

        assert_eq!(code, 0);
        assert!(t.err.contents().contains("worktree already exists at"));
        let target = std::fs::read_to_string(&cd_file).unwrap();
        assert!(Path::new(&target).join("marker.txt").exists());
    }

    #[test]
    fn creates_new_branch_and_worktree() {
        let repo = TestRepo::init();
        let (code, out, _) = run(&repo, &args("feature/login"), false);
        assert_eq!(code, 0);
        let path = out.trim();
        assert!(Path::new(path).is_dir());
        assert!(path.ends_with("feature-login"));
        assert!(
            !repo
                .git(&["rev-parse", "--verify", "refs/heads/feature/login"])
                .is_empty()
        );
        assert_eq!(
            repo.git(&["config", "--get", "wt.feature/login.createdByWt"])
                .trim(),
            "true"
        );
        assert_eq!(
            repo.git(&["config", "--get", "wt.feature/login.baseRef"])
                .trim(),
            "main"
        );
    }

    #[test]
    fn checks_out_existing_branch_without_marking_created() {
        let repo = TestRepo::init();
        repo.git(&["branch", "existing"]);
        let (code, out, _) = run(&repo, &args("existing"), false);
        assert_eq!(code, 0);
        assert!(Path::new(out.trim()).is_dir());
        let all = repo.git(&["config", "--list"]);
        assert!(!all.contains("wt.existing"), "unexpected metadata: {all}");
    }

    #[test]
    fn idempotent_when_branch_already_at_target() {
        let repo = TestRepo::init();
        run(&repo, &args("feature/x"), false);
        let (code, out, _) = run(&repo, &args("feature/x"), false);
        assert_eq!(code, 0);
        assert!(out.trim().ends_with("feature-x"));
    }

    #[test]
    fn refuses_branch_checked_out_elsewhere() {
        let repo = TestRepo::init();
        repo.add_worktree("dup", "../manual-dup");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(&mut t.cx, &RealHookRunner, &args("dup"), false).unwrap_err();
        assert!(err.to_string().contains("already checked out"));
    }

    #[test]
    fn no_switch_prints_to_stderr_not_stdout() {
        let repo = TestRepo::init();
        let mut a = args("topic");
        a.no_switch = true;
        let (code, out, err) = run(&repo, &a, false);
        assert_eq!(code, 0);
        assert!(out.is_empty());
        assert!(err.contains("created worktree at"));
    }

    #[test]
    fn json_emits_result_object() {
        let repo = TestRepo::init();
        let (code, out, _) = run(&repo, &args("feature/j"), true);
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["branch"], serde_json::json!("feature/j"));
        assert_eq!(v["base_ref"], serde_json::json!("main"));
        assert_eq!(v["schema_version"], serde_json::json!(1));
    }

    #[test]
    fn from_base_ref_is_used() {
        let repo = TestRepo::init();
        repo.write("f.txt", "x\n");
        repo.commit_all("second");
        repo.git(&["branch", "base-branch"]);
        let mut a = args("derived");
        a.from = Some("base-branch".to_string());
        let (code, _, _) = run(&repo, &a, false);
        assert_eq!(code, 0);
        assert_eq!(
            repo.git(&["config", "--get", "wt.derived.baseRef"]).trim(),
            "base-branch"
        );
    }

    /// A repo with a real `origin` remote (itself) and a fetched
    /// `refs/remotes/origin/main`, so `origin/main` is a genuine
    /// remote-tracking branch for upstream/autoSetupMerge purposes.
    fn repo_with_origin() -> TestRepo {
        let repo = TestRepo::init();
        repo.git(&["remote", "add", "origin", repo.root().to_str().unwrap()]);
        repo.git(&["fetch", "-q", "origin"]);
        repo
    }

    #[test]
    fn new_branch_does_not_inherit_base_upstream() {
        // Forking from a remote-tracking base must not make that base the new
        // branch's upstream (issue #43); git's autoSetupMerge would otherwise.
        let repo = repo_with_origin();
        let mut a = args("feat");
        a.from = Some("origin/main".to_string());
        let (code, _, _) = run(&repo, &a, false);
        assert_eq!(code, 0);
        // No upstream is configured for the new branch (`--get` exits non-zero on
        // a missing key, so check the full listing instead).
        let all = repo.git(&["config", "--list"]);
        assert!(
            !all.contains("branch.feat.remote"),
            "new branch should not track the base: {all}"
        );
    }

    #[test]
    fn track_sets_explicit_upstream() {
        // `--track <REF>` records an explicit upstream for the new branch.
        let repo = repo_with_origin();
        let mut a = args("feat");
        a.track = Some("origin/main".to_string());
        let (code, _, _) = run(&repo, &a, false);
        assert_eq!(code, 0);
        assert_eq!(
            repo.git(&["config", "--get", "branch.feat.remote"]).trim(),
            "origin"
        );
        assert_eq!(
            repo.git(&["config", "--get", "branch.feat.merge"]).trim(),
            "refs/heads/main"
        );
    }

    #[test]
    fn rolls_back_worktree_when_a_post_add_step_fails() {
        use crate::git::cli::{GitCli, GitOutput, RealGit};
        use std::path::Path as StdPath;
        use std::sync::Arc;

        struct FailConfig(RealGit);
        impl GitCli for FailConfig {
            fn run_raw(&self, repo: &StdPath, args: &[&str]) -> crate::error::Result<GitOutput> {
                if args.first() == Some(&"config") && args.iter().any(|a| a.starts_with("wt.")) {
                    return Ok(GitOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "simulated failure".into(),
                    });
                }
                self.0.run_raw(repo, args)
            }
        }

        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx_with_git(
            &[],
            repo.root().to_str().unwrap(),
            Arc::new(FailConfig(RealGit)),
        );
        let err = super::run(&mut t.cx, &RealHookRunner, &args("rollme"), false).unwrap_err();
        assert!(err.to_string().contains("simulated failure"));

        let repo_name = repo
            .root()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let target = repo
            .root()
            .parent()
            .unwrap()
            .join(format!("{repo_name}.worktrees"));
        let leaf = format!("{repo_name}-rollme");
        assert!(!target.join(leaf).exists(), "worktree not rolled back");
        assert!(repo.git(&["branch", "--list", "rollme"]).trim().is_empty());
    }

    #[test]
    fn copies_ignored_files_into_new_worktree() {
        let repo = TestRepo::init();
        std::fs::write(repo.root().join(".wt.toml"), "copy = [\".env\"]\n").unwrap();
        repo.write(".env", "SECRET=1\n");
        let (code, out, err) = run(&repo, &args("withenv"), false);
        assert_eq!(code, 0);
        let env_path = Path::new(out.trim()).join(".env");
        assert!(env_path.exists());
        assert_eq!(std::fs::read_to_string(env_path).unwrap(), "SECRET=1\n");
        // Silent at the default verbosity (spec §8).
        assert!(!err.contains("copied"));
    }

    /// A repo with a committed submodule on `main`, so a new worktree inherits
    /// the `.gitmodules` definition (uninitialized until populated).
    fn repo_with_submodule() -> TestRepo {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        repo
    }

    #[test]
    fn new_default_does_not_init_submodules() {
        let repo = repo_with_submodule();
        let (code, out, err) = run(&repo, &args("feat"), false);
        assert_eq!(code, 0);
        // No policy/flag: submodules are left alone and nothing is logged.
        assert!(!err.contains("initializing"));
        assert!(!Path::new(out.trim()).join("libs/sub/sub.txt").exists());
    }

    #[test]
    fn new_init_submodules_flag_runs_init() {
        let repo = repo_with_submodule();
        let mut a = args("feat");
        a.init_submodules = true;
        let (code, _out, err) = run(&repo, &a, false);
        // `--init-submodules` runs the init (non-fatal even if a file-protocol
        // clone is later refused), proving `new` wires the policy through.
        assert_eq!(code, 0);
        assert!(err.contains("initializing 1 submodule"));
    }

    #[test]
    fn new_no_init_submodules_flag_overrides_always_config() {
        let repo = repo_with_submodule();
        std::fs::write(
            repo.root().join(".wt.toml"),
            "[submodules]\ninit = \"always\"\n",
        )
        .unwrap();
        let mut a = args("feat");
        a.no_init_submodules = true;
        let (code, out, err) = run(&repo, &a, false);
        assert_eq!(code, 0);
        // The flag overrides `init = "always"`: no init runs.
        assert!(!err.contains("initializing"));
        assert!(!Path::new(out.trim()).join("libs/sub/sub.txt").exists());
    }

    #[test]
    fn verbose_logs_copied_files() {
        let repo = TestRepo::init();
        std::fs::write(repo.root().join(".wt.toml"), "copy = [\".env\"]\n").unwrap();
        repo.write(".env", "SECRET=1\n");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.verbose = 1;
        super::run(&mut t.cx, &RealHookRunner, &args("withenv2"), false).unwrap();
        let err = t.err.contents();
        assert!(err.contains("copied"), "expected copy log at -v: {err}");
        assert!(err.contains(".env"));
    }

    /// Runs `new` with seeded prompt answers, returning `(code, out, err)`.
    fn run_with_input(repo: &TestRepo, a: &NewArgs, inputs: &[&str]) -> (u8, String, String) {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.input = Box::new(crate::testutil::CannedInput::new(inputs));
        let code = super::run(&mut t.cx, &RealHookRunner, a, false).unwrap();
        (code, t.out.contents(), t.err.contents())
    }

    /// Runs `new` with a TTY stderr and seeded prompt answers, returning
    /// `(code, stdout, stderr)`. The TTY makes the submodule prompt fire (issue #50).
    fn run_with_tty_input(repo: &TestRepo, a: &NewArgs, inputs: &[&str]) -> (u8, String, String) {
        use crate::cx::Stream;
        use crate::testutil::{CannedInput, SharedBuf};
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = SharedBuf::new();
        t.cx.err = Stream::new(Box::new(err.clone()), true);
        t.cx.input = Box::new(CannedInput::new(inputs));
        let code = super::run(&mut t.cx, &RealHookRunner, a, false).unwrap();
        (code, t.out.contents(), err.contents())
    }

    #[test]
    fn new_prompts_and_initializes_submodules_on_yes() {
        // The default policy at a TTY asks; `y` runs the recursive init (issue #50).
        let repo = repo_with_submodule();
        let (code, _out, err) = run_with_tty_input(&repo, &args("feat"), &["y"]);
        assert_eq!(code, 0);
        assert!(err.contains("uninitialized submodule"));
        assert!(err.contains("initializing 1 submodule"));
    }

    #[test]
    fn new_prompt_defaults_to_yes_on_empty_answer() {
        let repo = repo_with_submodule();
        let (code, _out, err) = run_with_tty_input(&repo, &args("feat"), &[""]);
        assert_eq!(code, 0);
        assert!(err.contains("initializing 1 submodule"));
    }

    #[test]
    fn new_prompt_no_leaves_submodules_uninitialized() {
        let repo = repo_with_submodule();
        let (code, out, err) = run_with_tty_input(&repo, &args("feat"), &["n"]);
        assert_eq!(code, 0);
        assert!(err.contains("uninitialized submodule"));
        assert!(!err.contains("initializing"));
        assert!(!Path::new(out.trim()).join("libs/sub/sub.txt").exists());
    }

    /// Leaves local `main` one commit behind `origin/main` (with the upstream
    /// configured but no fetchable remote, so the check's fetch is skipped).
    /// Returns the `origin/main` commit.
    fn make_main_behind(repo: &TestRepo) -> String {
        let c1 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("upstream.txt", "1\n");
        repo.commit_all("ahead on origin");
        let c2 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &c2]);
        repo.git(&["reset", "-q", "--hard", &c1]);
        repo.git(&["config", "branch.main.remote", "origin"]);
        repo.git(&["config", "branch.main.merge", "refs/heads/main"]);
        c2
    }

    #[test]
    fn stale_base_cancel_aborts_create() {
        let repo = TestRepo::init();
        make_main_behind(&repo);
        // An empty answer defaults to cancel (issue #56).
        let (code, out, err) = run_with_input(&repo, &args("feature"), &[""]);
        assert_eq!(code, 1);
        assert!(out.is_empty());
        assert!(err.contains("aborted"));
        assert!(repo.git(&["branch", "--list", "feature"]).trim().is_empty());
    }

    #[test]
    fn stale_base_proceed_creates_off_stale_base() {
        let repo = TestRepo::init();
        let c2 = make_main_behind(&repo);
        let c1 = repo
            .git(&["rev-parse", "refs/heads/main"])
            .trim()
            .to_string();
        let (code, _, _) = run_with_input(&repo, &args("feature"), &["proceed"]);
        assert_eq!(code, 0);
        // Forked off the stale local main, not origin/main.
        assert_eq!(repo.git(&["rev-parse", "refs/heads/feature"]).trim(), c1);
        assert_ne!(c1, c2);
    }

    #[test]
    fn stale_base_update_fast_forwards_then_creates() {
        let repo = TestRepo::init();
        let c2 = make_main_behind(&repo);
        let (code, _, err) = run_with_input(&repo, &args("feature"), &["update"]);
        assert_eq!(code, 0);
        assert!(err.contains("updated main"));
        // main was fast-forwarded to origin/main, and feature forks from it.
        assert_eq!(repo.git(&["rev-parse", "refs/heads/main"]).trim(), c2);
        assert_eq!(repo.git(&["rev-parse", "refs/heads/feature"]).trim(), c2);
    }
}
