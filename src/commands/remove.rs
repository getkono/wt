//! `wt remove <query>` — remove a linked worktree (spec §7/§8/§10/§12).

use std::path::Path;

use crate::cli::RemoveArgs;
use crate::commands::{Resolution, open_session, resolve_query};
use crate::config::Config;
use crate::config::wtconfig::{self, WtMeta};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::{branch_ref, default_branch, is_ancestor, ops, resolve_hex};
use crate::hooks::{HookContext, HookRunner, run_pre_remove};
use crate::model::{RemovedResult, Worktree};
use crate::worktree_service::{build_worktrees, enumerate_worktrees, guard_status};

/// Options controlling a removal. The worktree-removal force (skip the
/// dirty/unpushed guards) is decoupled from the branch-deletion force (delete a
/// branch that is not fully merged). The CLI's `--force` sets both; the TUI
/// confirm dialog sets only `force_remove` — the dialog is itself the guard, so
/// `y` may remove a dirty/unpushed worktree, but it must never silently
/// force-delete an unmerged branch (spec §10/§12).
pub(crate) struct RemoveOptions {
    /// Skip the dirty/unpushed guards and pass `--force` to `git worktree remove`.
    pub(crate) force_remove: bool,
    /// Permit deleting a branch that is not fully merged into its base.
    pub(crate) force_branch: bool,
    /// Always keep the local branch.
    pub(crate) keep_branch: bool,
    /// Skip the pre-remove hook.
    pub(crate) no_hooks: bool,
}

impl RemoveOptions {
    /// Builds options from the CLI flags, where `--force` forces both removal
    /// and unmerged-branch deletion.
    pub(crate) fn from_args(args: &RemoveArgs) -> Self {
        RemoveOptions {
            force_remove: args.force,
            force_branch: args.force,
            keep_branch: args.keep_branch,
            no_hooks: args.no_hooks,
        }
    }
}

/// Removes the worktree matching `args.query`, applying the safety guards,
/// running the pre-remove hook, and optionally deleting a fully-merged
/// wt-created branch.
pub(crate) fn run(
    cx: &mut Cx,
    hooks: &dyn HookRunner,
    args: &RemoveArgs,
    json: bool,
) -> Result<u8> {
    remove_query(
        cx,
        hooks,
        &args.query,
        &RemoveOptions::from_args(args),
        json,
    )
}

/// Resolves `query` to a worktree and removes it under the given options.
/// Shared by the CLI (`run`) and the TUI confirm-remove dialog, which differ
/// only in their [`RemoveOptions`].
pub(crate) fn remove_query(
    cx: &mut Cx,
    hooks: &dyn HookRunner,
    query: &str,
    opts: &RemoveOptions,
    json: bool,
) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let root = session.primary_root.clone();
    let worktrees = build_worktrees(&session.repo, git)?;

    let index = match resolve_query(cx, &worktrees, query) {
        Resolution::Found(index) => index,
        Resolution::Ambiguous => return Ok(3),
        Resolution::NotFound => {
            return Err(Error::NotFound {
                query: query.to_string(),
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
        ops::worktree_prune(git, &root)?;
        let deleted = maybe_delete_branch(
            git,
            &root,
            &worktree,
            &meta,
            &session.config,
            opts,
            &default,
        );
        clear_metadata(git, &root, &worktree);
        return finish(cx, &worktree, json, deleted);
    }

    // Safety guards (spec §10/§12).
    let guard = guard_status(&worktree, session.config.remove_untracked_blocks);
    if guard.blocks() && !opts.force_remove {
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
    if guard.blocks() && opts.force_remove {
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
        opts.no_hooks,
        opts.force_remove,
    )?;

    // Remove the worktree.
    let path = worktree.path.to_string_lossy().into_owned();
    ops::worktree_remove(git, &root, &path, opts.force_remove)?;

    let deleted = maybe_delete_branch(
        git,
        &root,
        &worktree,
        &meta,
        &session.config,
        opts,
        &default,
    );
    clear_metadata(git, &root, &worktree);
    finish(cx, &worktree, json, deleted)
}

/// Deletes a local branch that has no worktree — a TUI "branch row" (issue #53),
/// for which there is no worktree to remove, only the branch itself. Runs a safe
/// `git branch -d` unless `force` is set (then `git branch -D`, to delete a branch
/// that is not fully merged). Errors if the branch does not exist or is currently
/// checked out in a worktree (the user should remove that worktree first). When a
/// safe delete is refused because the branch is unmerged, the returned error
/// message contains the stable substring "not fully merged", which the TUI keys on
/// to offer a force-delete.
pub(crate) fn delete_branch_query(
    cx: &mut Cx,
    branch: &str,
    force: bool,
    json: bool,
) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let root = session.primary_root.clone();

    // The branch must exist as a local ref.
    if resolve_hex(session.repo.gix(), &branch_ref(branch)).is_none() {
        return Err(Error::NotFound {
            query: branch.to_string(),
        });
    }

    // A branch checked out in a worktree cannot be deleted directly — git refuses
    // it anyway, and the user means to remove that worktree first.
    let worktrees = enumerate_worktrees(&session.repo, git)?;
    if worktrees
        .iter()
        .any(|w| w.branch.as_deref() == Some(branch))
    {
        return Err(Error::operation(format!(
            "branch {branch:?} is checked out; remove its worktree first"
        )));
    }

    let out = ops::delete_branch(git, &root, branch, force)?;
    if !out.success {
        // `git branch -d` refuses an unmerged branch; preserve the "not fully
        // merged" sentinel so the TUI can re-prompt to force-delete (issue #53).
        if !force && out.stderr.contains("not fully merged") {
            return Err(Error::operation(format!(
                "branch {branch:?} is not fully merged; not deleted"
            )));
        }
        return Err(Error::operation(format!(
            "failed to delete branch {branch:?}: {}",
            out.stderr.trim()
        )));
    }
    // Best-effort: clear any `wt.*` metadata recorded for this branch.
    let _ = wtconfig::clear_meta(git, &root, branch);

    if json {
        cx.out.line(&serde_json::to_string(&serde_json::json!({
            "branch": branch,
            "deleted": true,
        }))?)?;
    } else {
        cx.err.line(&format!("deleted branch {branch}"))?;
    }
    Ok(0)
}

/// Deletes the branch if it is wt-created and either fully merged (and the
/// config allows it) or `force_branch` (for an unmerged branch). Returns whether
/// the branch was deleted.
fn maybe_delete_branch(
    git: &dyn GitCli,
    root: &Path,
    worktree: &Worktree,
    meta: &WtMeta,
    config: &Config,
    opts: &RemoveOptions,
    default: &Option<String>,
) -> bool {
    let Some(branch) = &worktree.branch else {
        return false;
    };
    if opts.keep_branch || !meta.created_by_wt {
        return false;
    }
    let base = meta.base_ref.clone().or_else(|| default.clone());
    let merged = base
        .as_deref()
        .is_some_and(|b| is_ancestor(git, root, &branch_ref(branch), b));
    let should_delete = if merged {
        config.remove_delete_merged_branch
    } else {
        opts.force_branch
    };
    if !should_delete {
        return false;
    }
    ops::delete_branch(git, root, branch, true).is_ok()
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
    use crate::testutil::{TestRepo, give_upstream, make_wt, wt_dir};

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

    /// Commits a new file on a worktree's branch so it is no longer merged into
    /// its base.
    fn make_unmerged(repo: &TestRepo, branch: &str) {
        let wt = wt_dir(repo, branch);
        std::fs::write(wt.join("change.txt"), "x\n").unwrap();
        let dir = wt.to_string_lossy().into_owned();
        repo.git(&["-C", &dir, "add", "-A"]);
        repo.git(&["-C", &dir, "commit", "-q", "-m", "unmerged change"]);
    }

    #[test]
    fn tui_force_remove_keeps_unmerged_branch() {
        // The TUI confirm dialog removes a dirty/unpushed worktree (force_remove)
        // but must never force-delete an unmerged branch (force_branch = false).
        let repo = TestRepo::init();
        make_wt(&repo, "tuionly");
        make_unmerged(&repo, "tuionly");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let opts = super::RemoveOptions {
            force_remove: true,
            force_branch: false,
            keep_branch: false,
            no_hooks: true,
        };
        let code =
            super::remove_query(&mut t.cx, &RealHookRunner, "tuionly", &opts, false).unwrap();
        assert_eq!(code, 0);
        assert!(!repo.git(&["worktree", "list"]).contains("tuionly"));
        // The unmerged branch survives (no data loss).
        assert!(
            !repo.git(&["branch", "--list", "tuionly"]).trim().is_empty(),
            "unmerged branch must not be force-deleted by the TUI"
        );
    }

    #[test]
    fn cli_force_remove_deletes_unmerged_branch() {
        // By contrast, the CLI `--force` deletes the unmerged branch.
        let repo = TestRepo::init();
        make_wt(&repo, "cliforce");
        make_unmerged(&repo, "cliforce");
        let (code, _, _) = run(&repo, &args("cliforce", true, false), false).unwrap();
        assert_eq!(code, 0);
        assert!(
            repo.git(&["branch", "--list", "cliforce"])
                .trim()
                .is_empty(),
            "--force should delete the unmerged branch"
        );
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

    /// Runs `delete_branch_query` against the repo, returning `(code, out, err)`.
    fn delete_branch(repo: &TestRepo, branch: &str, force: bool) -> Result<(u8, String, String)> {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::delete_branch_query(&mut t.cx, branch, force, false)?;
        Ok((code, t.out.contents(), t.err.contents()))
    }

    #[test]
    fn deletes_unattached_merged_branch() {
        let repo = TestRepo::init();
        // A branch at HEAD with no worktree (a TUI branch row); it is merged.
        repo.git(&["branch", "merged-topic"]);
        let (code, _, err) = delete_branch(&repo, "merged-topic", false).unwrap();
        assert_eq!(code, 0);
        assert!(err.contains("deleted branch merged-topic"));
        assert!(
            repo.git(&["branch", "--list", "merged-topic"])
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn refuses_to_delete_checked_out_branch() {
        let repo = TestRepo::init();
        make_wt(&repo, "active");
        let err = delete_branch(&repo, "active", false).unwrap_err();
        assert!(err.to_string().contains("checked out"));
        assert!(!repo.git(&["branch", "--list", "active"]).trim().is_empty());
    }

    #[test]
    fn safe_delete_refuses_unmerged_then_force_deletes() {
        let repo = TestRepo::init();
        make_wt(&repo, "unmerged");
        make_unmerged(&repo, "unmerged");
        // Drop the worktree but keep the branch -> a worktree-less unmerged branch.
        let dir = wt_dir(&repo, "unmerged").to_string_lossy().into_owned();
        repo.git(&["worktree", "remove", "--force", &dir]);
        // A safe delete refuses an unmerged branch; the branch survives. Assert
        // the specific sentinel message (not just "not fully merged", which git's
        // own stderr also contains) so the issue #53 TUI re-prompt keys on it.
        let err = delete_branch(&repo, "unmerged", false).unwrap_err();
        assert!(err.to_string().contains("is not fully merged; not deleted"));
        assert!(
            !repo
                .git(&["branch", "--list", "unmerged"])
                .trim()
                .is_empty()
        );
        // Force delete removes it.
        let (code, _, _) = delete_branch(&repo, "unmerged", true).unwrap();
        assert_eq!(code, 0);
        assert!(
            repo.git(&["branch", "--list", "unmerged"])
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn delete_unknown_branch_is_not_found() {
        let repo = TestRepo::init();
        let err = delete_branch(&repo, "ghost", false).unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }
}
