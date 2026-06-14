//! `wt new <branch>` — create a linked worktree (spec §6/§7/§8/§13).

use std::path::{Path, PathBuf};

use crate::cli::NewArgs;
use crate::commands::{
    emit_worktree, maybe_init_submodules, open_session, render_target, resolve_target,
    rollback_worktree, same_path,
};
use crate::config::wtconfig;
use crate::copy::copy_ignored_files;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{branch_ref, default_branch, ops, resolve_hex};
use crate::hooks::{HookContext, HookRunner, run_post_create};
use crate::model::Worktree;
use crate::query::{self, Resolved};
use crate::slug::slugify_with_fallback;
use crate::worktree_service::enumerate_worktrees;

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
    run_core(cx, hooks, args, json)
}

/// Creates a linked worktree for `branch` (creating the branch if needed), runs
/// the copy step and post-create hook, and prints the new path (unless
/// `--no-switch`/`--json`). The base-staleness check (issue #56) is the caller's
/// responsibility — [`run`] does it for the CLI, the TUI before this is reached.
pub(crate) fn run_core(
    cx: &mut Cx,
    hooks: &dyn HookRunner,
    args: &NewArgs,
    json: bool,
) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let repo = &session.repo;
    let root = session.primary_root.clone();
    let branch = args.branch.clone();

    let worktrees = enumerate_worktrees(repo, git)?;
    let branch_exists = resolve_hex(repo.gix(), &branch_ref(&branch)).is_some();

    let base_ref = if branch_exists {
        None
    } else {
        Some(resolve_base_ref(
            cx,
            repo,
            args.from.as_deref(),
            &session.config.default_base,
        ))
    };
    let base_commit = match &base_ref {
        Some(base) => resolve_hex(repo.gix(), base)
            .ok_or_else(|| Error::operation(format!("base ref {base:?} not found")))?,
        None => resolve_hex(repo.gix(), &branch_ref(&branch)).unwrap_or_default(),
    };
    let short_hash = base_commit.get(..7).unwrap_or(&base_commit).to_string();
    let slug = slugify_with_fallback(&branch, &short_hash);

    // If the branch is already checked out, either no-op (same target) or refuse.
    if let Some(existing) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch.as_str()))
    {
        let preview = render_target(&session.config, &root, &branch, &slug, &cx.env)?;
        if same_path(&existing.path, &preview) {
            let path = existing.path.clone();
            return emit_worktree(
                cx,
                &path,
                json,
                args.no_switch,
                "worktree already exists at",
            );
        }
        return Err(Error::operation(format!(
            "branch {branch:?} is already checked out at {}",
            existing.path.display()
        )));
    }

    let target = resolve_target(
        &session.config,
        &root,
        &branch,
        &slug,
        &short_hash,
        &cx.env,
        repo.is_bare(),
    )?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Create the worktree (git is atomic here).
    let target_str = target.to_string_lossy().into_owned();
    if let Some(base) = &base_ref {
        // `--no-track` keeps the new branch from inheriting the base as its
        // upstream (issue #43): git's `branch.autoSetupMerge` would otherwise make
        // a remote-tracking base the upstream. `--track` opts into an explicit one.
        ops::worktree_add_branch(git, &root, &branch, &target_str, base, true)?;
    } else {
        ops::worktree_add(git, &root, &target_str, &branch)?;
    }

    // Steps after creation but before the hook are rolled back on failure (§13).
    let copy_outcome = match post_create_steps(
        git,
        repo,
        &worktrees,
        &session.config,
        &root,
        &branch,
        &base_ref,
        &target,
        args.track.as_deref(),
        args.copy_from.as_deref(),
    ) {
        Ok(outcome) => outcome,
        Err(e) => {
            // Metadata is written only for a wt-created branch, so delete the
            // branch and clear metadata together on that condition.
            let created = base_ref.is_some();
            rollback_worktree(git, &root, &target, &branch, created, created);
            return Err(e);
        }
    };
    crate::commands::log_copy_outcome(cx, &copy_outcome);

    // The post-create hook: a failure is a warning, not a rollback (§8).
    let ctx = HookContext {
        worktree_path: target.clone(),
        branch: branch.clone(),
        repo_root: root.clone(),
        base_ref: base_ref.clone(),
        pr_number: None,
    };
    run_post_create(
        hooks,
        cx,
        session.config.hooks_post_create.as_deref(),
        &ctx,
        args.no_hooks,
    )?;

    // Initialize submodules when the policy (or `--init-submodules`) asks for it
    // (issue #50). Non-fatal — the worktree already exists.
    maybe_init_submodules(
        cx,
        git,
        &target,
        session.config.submodules_init,
        args.submodule_override(),
    )?;

    emit_worktree(cx, &target, json, args.no_switch, "created worktree at")
}

/// Records metadata and runs the copy step (rolled back on error), returning the
/// copy outcome for `-v` logging.
#[allow(clippy::too_many_arguments)]
fn post_create_steps(
    git: &dyn GitCli,
    repo: &Repo,
    worktrees: &[Worktree],
    config: &crate::config::Config,
    root: &Path,
    branch: &str,
    base_ref: &Option<String>,
    target: &Path,
    track: Option<&str>,
    copy_from: Option<&str>,
) -> Result<crate::copy::CopyOutcome> {
    if let Some(base) = base_ref {
        // A wt-created branch records its base and "created by wt" (§3/§10).
        wtconfig::write_base_ref(git, root, branch, base)?;
        wtconfig::mark_created_by_wt(git, root, branch)?;
    }
    // `--track <REF>` sets an explicit upstream (issue #43); a bad ref fails here,
    // inside the rolled-back region, so the half-created worktree is torn down.
    if let Some(upstream) = track {
        ops::set_upstream(git, root, branch, upstream)?;
    }
    let source = copy_source(repo, worktrees, copy_from, root)?;
    copy_ignored_files(git, &source, target, &config.copy)
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
    Some(resolve_base_ref(
        cx,
        repo,
        args.from.as_deref(),
        &config.default_base,
    ))
}

/// Detects whether the base `args` would fork from is behind its upstream (issue
/// #56), for the TUI create pre-flight. `Ok(None)` when there is nothing to warn
/// about (existing branch, no upstream, up to date, or offline).
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

/// Resolves the base ref for a new branch: `--from`, then `default_base`, then
/// the repo default branch, then `HEAD` (warning when falling back).
fn resolve_base_ref(
    cx: &mut Cx,
    repo: &Repo,
    from: Option<&str>,
    default_base: &Option<String>,
) -> String {
    if let Some(from) = from {
        return from.to_string();
    }
    if let Some(base) = default_base {
        return base.clone();
    }
    if let Some(branch) = default_branch(repo.gix()) {
        return branch;
    }
    let _ = cx
        .err
        .line("warning: no default branch; basing the new branch on HEAD");
    "HEAD".to_string()
}

/// Resolves the copy source worktree: `--copy-from`, else the current worktree,
/// else the primary root (spec §8).
fn copy_source(
    repo: &Repo,
    worktrees: &[Worktree],
    copy_from: Option<&str>,
    root: &Path,
) -> Result<PathBuf> {
    if let Some(query) = copy_from {
        return match query::resolve(worktrees, query) {
            Resolved::One(index) => Ok(worktrees[index].path.clone()),
            Resolved::Ambiguous(_) => Err(Error::operation(format!(
                "--copy-from {query:?} is ambiguous"
            ))),
            Resolved::NotFound => Err(Error::NotFound {
                query: query.to_string(),
            }),
        };
    }
    Ok(repo.current_workdir().unwrap_or_else(|| root.to_path_buf()))
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
