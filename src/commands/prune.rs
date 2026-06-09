//! `wt prune` — bulk cleanup of merged or stale worktrees (spec §7/§12).

use std::path::Path;

use crate::cli::PruneArgs;
use crate::commands::{candidate_label, confirm, open_session};
use crate::config::wtconfig;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::{default_branch, is_ancestor, upstream_of};
use crate::model::Worktree;
use crate::worktree_service::{build_worktrees, guard_status};

/// Selects and removes prune candidates after confirmation (spec §7/§12).
pub fn run(cx: &mut Cx, args: &PruneArgs, json: bool) -> Result<u8> {
    if !args.merged && !args.gone {
        return Err(Error::usage("prune requires --merged and/or --gone"));
    }
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let root = session.primary_root.clone();
    let worktrees = build_worktrees(&session.repo, git)?;
    let default = default_branch(session.repo.gix());

    let candidates: Vec<usize> = worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| !w.is_main && is_candidate(git, &session.repo, &root, w, args, &default))
        .map(|(i, _)| i)
        .collect();

    // `local_branches` exposes how many branches exist versus how many worktrees
    // we actually inspect — the gap is the signal when a prune surprises someone.
    tracing::debug!(
        default = ?default,
        worktrees = worktrees.len(),
        candidates = candidates.len(),
        local_branches = crate::git::local_branches(session.repo.gix()).map_or(0, |b| b.len()),
        "prune: candidate selection",
    );

    // `--dry-run` (and the `--json` form) only report the candidate set.
    if args.dry_run || json {
        // Report the empty case explicitly on stderr (stdout stays clean for
        // `--json`) so a dry-run that finds nothing is never silently blank.
        if candidates.is_empty() && !json {
            cx.err.line("nothing to prune")?;
        }
        for &index in &candidates {
            if json {
                cx.out.line(&worktrees[index].to_json_line()?)?;
            } else {
                cx.out.line(&format!(
                    "would remove {}",
                    candidate_label(&worktrees[index])
                ))?;
            }
        }
        return Ok(0);
    }

    if candidates.is_empty() {
        cx.err.line("nothing to prune")?;
        git.run(&root, &["worktree", "prune"])?;
        return Ok(0);
    }

    // Confirmation prompt (unless --force).
    if !args.force {
        cx.err.line("worktrees to remove:")?;
        for &index in &candidates {
            cx.err
                .line(&format!("  {}", candidate_label(&worktrees[index])))?;
        }
        if !confirm(cx, "Proceed? [y/N] ")? {
            cx.err.line("aborted")?;
            return Ok(0);
        }
    }

    let mut removed = 0_usize;
    for &index in &candidates {
        let worktree = &worktrees[index];
        // Dirty worktrees are skipped unless --force (spec §12).
        if !args.force && guard_status(worktree, session.config.remove_untracked_blocks).dirty {
            cx.err.line(&format!(
                "skipping dirty worktree {}",
                candidate_label(worktree)
            ))?;
            continue;
        }
        if !worktree.is_missing {
            let path = worktree.path.to_string_lossy();
            let _ = git.run_raw(&root, &["worktree", "remove", "--force", &path]);
        }
        delete_merged_branch(
            git,
            &session.repo,
            &root,
            worktree,
            &session.config,
            &default,
        );
        if let Some(branch) = &worktree.branch {
            let _ = wtconfig::clear_meta(git, &root, branch);
        }
        tracing::debug!(target_wt = %candidate_label(worktree), "prune: removed worktree");
        removed += 1;
    }

    // Reconcile Git's worktree admin metadata (equivalent to `git worktree prune`).
    git.run(&root, &["worktree", "prune"])?;
    tracing::debug!(removed, "prune: done");
    cx.err.line(&format!("pruned {removed} worktree(s)"))?;
    Ok(0)
}

/// Whether a worktree is a prune candidate for the given flags.
fn is_candidate(
    git: &dyn GitCli,
    repo: &crate::git::discover::Repo,
    root: &Path,
    worktree: &Worktree,
    args: &PruneArgs,
    default: &Option<String>,
) -> bool {
    if args.merged
        && let Some(branch) = &worktree.branch
        && let Some(default) = default
        // The default branch is an ancestor of itself; never prune a worktree
        // that is checked out on the default branch.
        && branch != default
        && is_ancestor(git, root, &format!("refs/heads/{branch}"), default)
    {
        return true;
    }
    if args.gone && (worktree.is_missing || upstream_is_gone(repo, worktree)) {
        return true;
    }
    false
}

/// Whether the worktree's upstream is configured but gone (offline check).
fn upstream_is_gone(repo: &crate::git::discover::Repo, worktree: &Worktree) -> bool {
    worktree
        .branch
        .as_deref()
        .and_then(|b| upstream_of(repo.gix(), b))
        .is_some_and(|u| u.is_gone)
}

/// Deletes a wt-created branch that is fully merged into the default branch.
fn delete_merged_branch(
    git: &dyn GitCli,
    repo: &crate::git::discover::Repo,
    root: &Path,
    worktree: &Worktree,
    config: &crate::config::Config,
    default: &Option<String>,
) {
    let Some(branch) = &worktree.branch else {
        return;
    };
    if !config.remove_delete_merged_branch {
        return;
    }
    let meta = wtconfig::read_meta(repo.gix(), branch);
    if !meta.created_by_wt {
        return;
    }
    let merged = default
        .as_deref()
        .is_some_and(|d| is_ancestor(git, root, &format!("refs/heads/{branch}"), d));
    if merged {
        let _ = git.run_raw(root, &["branch", "-D", branch]);
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{NewArgs, PruneArgs};
    use crate::hooks::RealHookRunner;
    use crate::testutil::{CannedInput, TestRepo};

    fn prune_args(merged: bool, gone: bool, dry_run: bool, force: bool) -> PruneArgs {
        PruneArgs {
            merged,
            gone,
            dry_run,
            force,
        }
    }

    fn make_wt(repo: &TestRepo, branch: &str) {
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        crate::commands::new::run(
            &mut t.cx,
            &RealHookRunner,
            &NewArgs {
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

    #[test]
    fn requires_a_mode_flag() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err =
            super::run(&mut t.cx, &prune_args(false, false, false, false), false).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn dry_run_reports_merged_candidates_without_removing() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt"); // merged into main (no new commits)
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, true, false), false).unwrap();
        assert!(t.out.contents().contains("would remove merged-wt"));
        // Still present (dry-run).
        assert!(repo.git(&["worktree", "list"]).contains("merged-wt"));
    }

    #[test]
    fn dry_run_reports_nothing_when_no_candidates() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, true, false), false).unwrap();
        // A dry-run that finds nothing says so on stderr; stdout stays empty.
        assert!(t.err.contents().contains("nothing to prune"));
        assert!(t.out.contents().is_empty());
    }

    #[test]
    fn force_prunes_merged_worktree_and_branch() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, true), false).unwrap();
        assert!(!repo.git(&["worktree", "list"]).contains("merged-wt"));
        assert!(
            repo.git(&["branch", "--list", "merged-wt"])
                .trim()
                .is_empty()
        );
    }

    #[test]
    fn confirmation_yes_prunes() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.input = Box::new(CannedInput::new(&["y"]));
        super::run(&mut t.cx, &prune_args(true, false, false, false), false).unwrap();
        assert!(t.err.contents().contains("Proceed?"));
        assert!(!repo.git(&["worktree", "list"]).contains("merged-wt"));
    }

    #[test]
    fn confirmation_no_aborts() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.input = Box::new(CannedInput::new(&["n"]));
        super::run(&mut t.cx, &prune_args(true, false, false, false), false).unwrap();
        assert!(t.err.contents().contains("aborted"));
        assert!(repo.git(&["worktree", "list"]).contains("merged-wt"));
    }

    #[test]
    fn gone_prunes_missing_worktrees() {
        let repo = TestRepo::init();
        make_wt(&repo, "goner");
        let repo_name = repo.root().file_name().unwrap().to_string_lossy();
        let wt_path = repo
            .root()
            .parent()
            .unwrap()
            .join(format!("{repo_name}.worktrees/{repo_name}-goner"));
        std::fs::remove_dir_all(&wt_path).unwrap();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(false, true, false, true), false).unwrap();
        assert!(!repo.git(&["worktree", "list"]).contains("goner"));
    }

    #[test]
    fn json_lists_candidates_without_removing() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, false), true).unwrap();
        let out = t.out.contents();
        assert_eq!(out.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["branch"], serde_json::json!("merged-wt"));
        // --json implies dry-run: still present.
        assert!(repo.git(&["worktree", "list"]).contains("merged-wt"));
    }
}
