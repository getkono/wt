//! `wt remove <query>` — remove a linked worktree (spec §7/§8/§10/§12).

use std::path::Path;

use crate::cli::RemoveArgs;
use crate::commands::{Resolution, open_session, resolve_query};
use crate::config::Config;
use crate::config::wtconfig::{self, WtMeta};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::default_branch;
use crate::hooks::{HookContext, HookRunner, run_pre_remove};
use crate::model::{RemovedResult, Worktree};
use crate::worktree_service::{build_worktrees, guard_status};

/// Removes the worktree matching `query`, applying the safety guards, running
/// the pre-remove hook, and optionally deleting a fully-merged wt-created branch.
pub fn run(cx: &mut Cx, hooks: &dyn HookRunner, args: &RemoveArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let root = session.primary_root.clone();
    let worktrees = build_worktrees(&session.repo, git)?;

    let index = match resolve_query(cx, &worktrees, &args.query) {
        Resolution::Found(index) => index,
        Resolution::Ambiguous => return Ok(3),
        Resolution::NotFound => {
            return Err(Error::NotFound {
                query: args.query.clone(),
            });
        }
    };
    let worktree = worktrees[index].clone();

    if worktree.is_main {
        return Err(Error::operation("refusing to remove the primary worktree"));
    }

    let meta = worktree
        .branch
        .as_deref()
        .map(|b| wtconfig::read_meta(session.repo.gix(), b))
        .unwrap_or_default();
    let default = default_branch(session.repo.gix());

    // A missing worktree: prune the admin record; no guards or hook apply.
    if worktree.is_missing {
        git.run(&root, &["worktree", "prune"])?;
        let deleted = maybe_delete_branch(
            git,
            &root,
            &worktree,
            &meta,
            &session.config,
            args,
            &default,
        );
        clear_metadata(git, &root, &worktree);
        return finish(cx, &worktree, json, deleted);
    }

    // Safety guards (spec §10/§12).
    let guard = guard_status(&worktree, session.config.remove_untracked_blocks);
    if guard.blocks() && !args.force {
        let mut reasons = Vec::new();
        if guard.dirty {
            reasons.push("has uncommitted changes");
        }
        if guard.unpushed {
            reasons.push("has unpushed work");
        }
        return Err(Error::operation(format!(
            "worktree {}; use --force to remove anyway",
            reasons.join(" and ")
        )));
    }
    if guard.blocks() && args.force {
        cx.err
            .line("warning: removing with uncommitted or unpushed work; data may be lost")?;
    }

    // Pre-remove hook (may abort).
    let ctx = HookContext {
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone().unwrap_or_default(),
        repo_root: root.clone(),
        base_ref: meta.base_ref.clone(),
        pr_number: meta.pr_number,
    };
    run_pre_remove(
        hooks,
        cx,
        session.config.hooks_pre_remove.as_deref(),
        &ctx,
        args.no_hooks,
        args.force,
    )?;

    // Remove the worktree.
    let path = worktree.path.to_string_lossy().into_owned();
    if args.force {
        git.run(&root, &["worktree", "remove", "--force", &path])?;
    } else {
        git.run(&root, &["worktree", "remove", &path])?;
    }

    let deleted = maybe_delete_branch(
        git,
        &root,
        &worktree,
        &meta,
        &session.config,
        args,
        &default,
    );
    clear_metadata(git, &root, &worktree);
    finish(cx, &worktree, json, deleted)
}

/// Deletes the branch if it is wt-created and either fully merged (and the
/// config allows it) or `--force` (for an unmerged branch). Returns whether the
/// branch was deleted.
fn maybe_delete_branch(
    git: &dyn GitCli,
    root: &Path,
    worktree: &Worktree,
    meta: &WtMeta,
    config: &Config,
    args: &RemoveArgs,
    default: &Option<String>,
) -> bool {
    let Some(branch) = &worktree.branch else {
        return false;
    };
    if args.keep_branch || !meta.created_by_wt {
        return false;
    }
    let base = meta.base_ref.clone().or_else(|| default.clone());
    let merged = base
        .as_deref()
        .is_some_and(|b| is_ancestor(git, root, &format!("refs/heads/{branch}"), b));
    let should_delete = if merged {
        config.remove_delete_merged_branch
    } else {
        args.force
    };
    if !should_delete {
        return false;
    }
    git.run_raw(root, &["branch", "-D", branch]).is_ok()
}

/// Whether `a` is an ancestor of `b` (i.e. `a` is fully merged into `b`).
fn is_ancestor(git: &dyn GitCli, root: &Path, a: &str, b: &str) -> bool {
    git.run_raw(root, &["merge-base", "--is-ancestor", a, b])
        .map(|o| o.success)
        .unwrap_or(false)
}

/// Clears the worktree's `wt.*` metadata, best-effort.
fn clear_metadata(git: &dyn GitCli, root: &Path, worktree: &Worktree) {
    if let Some(branch) = &worktree.branch {
        let _ = wtconfig::clear_meta(git, root, branch);
    }
}

/// Emits the removal result.
fn finish(cx: &mut Cx, worktree: &Worktree, json: bool, branch_deleted: bool) -> Result<u8> {
    if json {
        let result = RemovedResult {
            worktree: worktree.clone(),
            removed: true,
        };
        cx.out.line(&serde_json::to_string(&result)?)?;
    } else {
        let suffix = if branch_deleted {
            " (branch deleted)"
        } else {
            ""
        };
        cx.err.line(&format!(
            "removed worktree at {}{suffix}",
            worktree.path.display()
        ))?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::cli::RemoveArgs;
    use crate::error::Result;
    use crate::hooks::RealHookRunner;
    use crate::testutil::TestRepo;

    fn args(query: &str, force: bool, keep_branch: bool) -> RemoveArgs {
        RemoveArgs {
            query: query.to_string(),
            force,
            keep_branch,
            no_hooks: true,
        }
    }

    fn run(repo: &TestRepo, a: &RemoveArgs, json: bool) -> Result<(u8, String, String)> {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &RealHookRunner, a, json)?;
        Ok((code, t.out.contents(), t.err.contents()))
    }

    /// Creates a wt-managed worktree via the real `new` command.
    fn make_wt(repo: &TestRepo, branch: &str) {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        crate::commands::new::run(
            &mut t.cx,
            &RealHookRunner,
            &crate::cli::NewArgs {
                branch: branch.to_string(),
                from: None,
                no_switch: true,
                no_hooks: true,
                copy_from: None,
            },
            false,
        )
        .unwrap();
    }

    /// Gives `branch` an upstream at its current tip (ahead/behind 0), so the
    /// no-upstream "unpushed" guard does not apply.
    fn give_upstream(repo: &TestRepo, branch: &str) {
        repo.git(&[
            "update-ref",
            &format!("refs/remotes/origin/{branch}"),
            &format!("refs/heads/{branch}"),
        ]);
        repo.git(&["config", &format!("branch.{branch}.remote"), "origin"]);
        repo.git(&[
            "config",
            &format!("branch.{branch}.merge"),
            &format!("refs/heads/{branch}"),
        ]);
    }

    fn wt_dir(repo: &TestRepo, branch: &str) -> std::path::PathBuf {
        repo.root().parent().unwrap().join(format!(
            "{}.worktrees/{}",
            repo.root().file_name().unwrap().to_string_lossy(),
            branch
        ))
    }

    #[test]
    fn removes_clean_worktree_and_deletes_merged_branch() {
        let repo = TestRepo::init();
        make_wt(&repo, "featurex");
        give_upstream(&repo, "featurex"); // not unpushed
        let (code, _, err) = run(&repo, &args("featurex", false, false), false).unwrap();
        assert_eq!(code, 0);
        assert!(err.contains("removed worktree"));
        assert!(err.contains("branch deleted"));
        assert!(!repo.git(&["worktree", "list"]).contains("featurex"));
        assert!(
            repo.git(&["branch", "--list", "featurex"])
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn no_upstream_branch_is_unpushed_and_blocks() {
        let repo = TestRepo::init();
        make_wt(&repo, "topic"); // no upstream -> treated as unpushed
        let err = run(&repo, &args("topic", false, false), false).unwrap_err();
        assert!(err.to_string().contains("unpushed"));
        // --force removes it with a data-loss warning.
        let (code, _, e) = run(&repo, &args("topic", true, false), false).unwrap();
        assert_eq!(code, 0);
        assert!(e.contains("data may be lost"));
    }

    #[test]
    fn refuses_dirty_even_with_upstream() {
        let repo = TestRepo::init();
        make_wt(&repo, "dirtywt");
        give_upstream(&repo, "dirtywt");
        std::fs::write(wt_dir(&repo, "dirtywt").join("README.md"), "changed\n").unwrap();
        let err = run(&repo, &args("dirtywt", false, false), false).unwrap_err();
        assert!(err.to_string().contains("uncommitted"));
        assert!(err.to_string().contains("--force"));
    }

    #[test]
    fn refuses_primary_worktree() {
        let repo = TestRepo::init();
        let err = run(&repo, &args("main", false, false), false).unwrap_err();
        assert!(err.to_string().contains("primary"));
    }

    #[test]
    fn keep_branch_preserves_branch() {
        let repo = TestRepo::init();
        make_wt(&repo, "kept");
        give_upstream(&repo, "kept");
        run(&repo, &args("kept", false, true), false).unwrap();
        assert!(!repo.git(&["branch", "--list", "kept"]).trim().is_empty());
    }

    #[test]
    fn missing_worktree_is_pruned_without_force() {
        let repo = TestRepo::init();
        make_wt(&repo, "gone");
        std::fs::remove_dir_all(wt_dir(&repo, "gone")).unwrap();
        // No --force needed for a missing worktree (guards skipped).
        let (code, _, _) = run(&repo, &args("gone", false, false), false).unwrap();
        assert_eq!(code, 0);
        assert!(!repo.git(&["worktree", "list"]).contains("gone"));
    }

    #[test]
    fn json_result_has_removed_flag() {
        let repo = TestRepo::init();
        make_wt(&repo, "featurej");
        give_upstream(&repo, "featurej");
        let (code, out, _) = run(&repo, &args("featurej", false, false), true).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["removed"], serde_json::json!(true));
        assert_eq!(v["branch"], serde_json::json!("featurej"));
    }
}
