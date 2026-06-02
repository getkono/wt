//! `wt new <branch>` — create a linked worktree (spec §6/§7/§8/§13).

use std::path::{Path, PathBuf};

use crate::cli::NewArgs;
use crate::commands::{
    emit_worktree, open_session, render_target, resolve_target, rollback_worktree, same_path,
};
use crate::config::wtconfig;
use crate::copy::copy_ignored_files;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{default_branch, resolve_hex};
use crate::hooks::{HookContext, HookRunner, run_post_create};
use crate::model::Worktree;
use crate::query::{self, Resolved};
use crate::slug::slugify_with_fallback;
use crate::worktree_service::enumerate_worktrees;

/// Creates a linked worktree for `branch` (creating the branch if needed), runs
/// the copy step and post-create hook, and prints the new path (unless
/// `--no-switch`/`--json`).
pub fn run(cx: &mut Cx, hooks: &dyn HookRunner, args: &NewArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let repo = &session.repo;
    let root = session.primary_root.clone();
    let branch = args.branch.clone();

    let worktrees = enumerate_worktrees(repo, git)?;
    let branch_exists = resolve_hex(repo.gix(), &format!("refs/heads/{branch}")).is_some();

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
        None => resolve_hex(repo.gix(), &format!("refs/heads/{branch}")).unwrap_or_default(),
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
        git.run(
            &root,
            &["worktree", "add", "-b", &branch, &target_str, base],
        )?;
    } else {
        git.run(&root, &["worktree", "add", &target_str, &branch])?;
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
    copy_from: Option<&str>,
) -> Result<crate::copy::CopyOutcome> {
    if let Some(base) = base_ref {
        // A wt-created branch records its base and "created by wt" (§3/§10).
        wtconfig::write_base_ref(git, root, branch, base)?;
        wtconfig::mark_created_by_wt(git, root, branch)?;
    }
    let source = copy_source(repo, worktrees, copy_from, root)?;
    copy_ignored_files(git, &source, target, &config.copy)
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
            no_switch: false,
            no_hooks: true,
            copy_from: None,
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
}
