//! `wt prune` — bulk cleanup of merged or stale worktrees, and the local
//! branches they leave behind (spec §7/§12).

use std::collections::HashSet;
use std::path::Path;

use crate::cli::PruneArgs;
use crate::commands::{Session, candidate_label, confirm, open_session, run_best_effort};
use crate::config::wtconfig;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{
    branch_ref, current_branch, default_branch, is_ancestor, local_branches, ops, upstream_of,
};
use crate::model::Worktree;
use crate::worktree_service::{build_worktrees, guard_status};

/// A prune target: an existing worktree (an index into the worktree list) or a
/// bare local branch — one with no worktree — that qualified for removal.
enum Candidate {
    /// An existing worktree, identified by its index in the worktree list.
    Worktree(usize),
    /// A local branch with no worktree. `merged` records whether it is an
    /// ancestor of the default branch (a safe `git branch -d`); a gone-only
    /// branch (`merged == false`) may hold unmerged commits and needs `--force`.
    Branch { name: String, merged: bool },
}

/// Selects and removes prune candidates after confirmation (spec §7/§12).
pub(crate) fn run(cx: &mut Cx, args: &PruneArgs, json: bool) -> Result<u8> {
    if !args.merged && !args.gone {
        return Err(Error::usage("prune requires --merged and/or --gone"));
    }
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let root = session.primary_root.clone();
    let worktrees = build_worktrees(&session.repo, git)?;
    let default = default_branch(session.repo.gix());
    let current = current_branch(session.repo.gix());

    // Branches that already have a worktree (the primary checkout and any branch
    // checked out elsewhere) are handled by the worktree path; the bare-branch
    // path skips them so a branch is never counted — or deleted — twice.
    let worktree_branches: HashSet<String> =
        worktrees.iter().filter_map(|w| w.branch.clone()).collect();

    let mut candidates: Vec<Candidate> = worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| !w.is_main && is_candidate(&session.repo, w, args, &default))
        .map(|(i, _)| Candidate::Worktree(i))
        .collect();
    candidates.extend(branch_candidates(
        &session.repo,
        args,
        &default,
        &current,
        &worktree_branches,
    )?);

    // The worktree/branch gap is the signal when a prune surprises someone.
    tracing::debug!(
        default = ?default,
        worktrees = worktrees.len(),
        candidates = candidates.len(),
        local_branches = local_branches(session.repo.gix()).map_or(0, |b| b.len()),
        "prune: candidate selection",
    );

    // `--dry-run` (and the `--json` form) only report the candidate set.
    if args.dry_run || json {
        // Report the empty case explicitly on stderr (stdout stays clean for
        // `--json`) so a dry-run that finds nothing is never silently blank.
        if candidates.is_empty() && !json {
            cx.err.line("nothing to prune")?;
        }
        for candidate in &candidates {
            if json {
                cx.out.line(&candidate_json(&worktrees, candidate)?)?;
            } else {
                cx.out.line(&format!(
                    "would remove {}",
                    candidate_text(&worktrees, candidate)
                ))?;
            }
        }
        return Ok(0);
    }

    if candidates.is_empty() {
        cx.err.line("nothing to prune")?;
        ops::worktree_prune(git, &root)?;
        return Ok(0);
    }

    // Confirmation prompt (unless --force).
    if !args.force {
        cx.err.line("to remove:")?;
        for candidate in &candidates {
            cx.err
                .line(&format!("  {}", candidate_text(&worktrees, candidate)))?;
        }
        if !confirm(cx, "Proceed? [y/N] ")? {
            cx.err.line("aborted")?;
            return Ok(0);
        }
    }

    let mut removed = 0_usize;
    for candidate in &candidates {
        let pruned = match candidate {
            Candidate::Worktree(index) => {
                remove_worktree(cx, git, &session, &root, &worktrees[*index], args, &default)?
            }
            Candidate::Branch { name, merged } => {
                remove_branch(cx, git, &root, name, *merged, args.force)?
            }
        };
        if pruned {
            removed += 1;
        }
    }

    // Reconcile Git's worktree admin metadata (equivalent to `git worktree prune`).
    ops::worktree_prune(git, &root)?;
    tracing::debug!(removed, "prune: done");
    cx.err.line(&format!("pruned {removed} item(s)"))?;
    Ok(0)
}

/// Selects local branches that have no worktree but qualify for pruning: merged
/// into the default branch (`--merged`) or with a gone upstream (`--gone`). The
/// default branch and the current branch are never selected, and branches that
/// already have a worktree are left to the worktree path.
fn branch_candidates(
    repo: &Repo,
    args: &PruneArgs,
    default: &Option<String>,
    current: &Option<String>,
    worktree_branches: &HashSet<String>,
) -> Result<Vec<Candidate>> {
    let mut out = Vec::new();
    for branch in local_branches(repo.gix())? {
        if worktree_branches.contains(&branch)
            || default.as_deref() == Some(branch.as_str())
            || current.as_deref() == Some(branch.as_str())
        {
            continue;
        }
        let merged = default
            .as_deref()
            .is_some_and(|d| is_ancestor(repo.gix(), &branch_ref(&branch), d));
        let gone = upstream_of(repo.gix(), &branch).is_some_and(|u| u.is_gone);
        tracing::trace!(branch = %branch, merged, gone, "prune: branch classified");
        if (args.merged && merged) || (args.gone && gone) {
            out.push(Candidate::Branch {
                name: branch,
                merged,
            });
        }
    }
    Ok(out)
}

/// Removes one worktree candidate, returning whether it was removed (a dirty
/// worktree is skipped unless `--force`). This is the per-worktree body of the
/// prune loop (spec §12).
fn remove_worktree(
    cx: &mut Cx,
    git: &dyn GitCli,
    session: &Session,
    root: &Path,
    worktree: &Worktree,
    args: &PruneArgs,
    default: &Option<String>,
) -> Result<bool> {
    // Dirty worktrees are skipped unless --force (spec §12).
    if !args.force && guard_status(worktree, session.config.remove_untracked_blocks).dirty {
        cx.err.line(&format!(
            "skipping dirty worktree {}",
            candidate_label(worktree)
        ))?;
        return Ok(false);
    }
    if !worktree.is_missing {
        let path = worktree.path.to_string_lossy();
        run_best_effort(
            git,
            root,
            &["worktree", "remove", "--force", &path],
            "prune: worktree remove",
        );
    }
    delete_merged_branch(git, &session.repo, root, worktree, &session.config, default);
    if let Some(branch) = &worktree.branch {
        let _ = wtconfig::clear_meta(git, root, branch);
    }
    tracing::debug!(target_wt = %candidate_label(worktree), "prune: removed worktree");
    Ok(true)
}

/// Deletes one bare-branch candidate, returning whether it was deleted. A merged
/// branch is removed with `git branch -d`; a gone-only branch (not merged into
/// the default) may hold unmerged commits, so it is skipped unless `--force`,
/// which force-deletes it (`git branch -D`). A delete failure is reported and
/// skipped rather than aborting the whole prune.
fn remove_branch(
    cx: &mut Cx,
    git: &dyn GitCli,
    root: &Path,
    name: &str,
    merged: bool,
    force: bool,
) -> Result<bool> {
    if !merged && !force {
        cx.err.line(&format!(
            "skipping {name}: branch may have unmerged commits; use --force"
        ))?;
        tracing::debug!(branch = %name, "prune: skip protected gone branch");
        return Ok(false);
    }
    match ops::delete_branch(git, root, name, !merged) {
        Ok(out) if out.success => {
            let _ = wtconfig::clear_meta(git, root, name);
            tracing::debug!(branch = %name, merged, "prune: deleted branch");
            Ok(true)
        }
        Ok(out) => {
            cx.err
                .line(&format!("could not delete {name}: {}", out.stderr.trim()))?;
            tracing::warn!(branch = %name, "prune: branch delete failed");
            Ok(false)
        }
        Err(error) => {
            cx.err.line(&format!("could not delete {name}: {error}"))?;
            tracing::warn!(branch = %name, "prune: branch delete errored");
            Ok(false)
        }
    }
}

/// A human label for a prune candidate (worktree or bare branch).
fn candidate_text(worktrees: &[Worktree], candidate: &Candidate) -> String {
    match candidate {
        Candidate::Worktree(index) => candidate_label(&worktrees[*index]),
        Candidate::Branch { name, .. } => format!("{name} (branch)"),
    }
}

/// A machine-readable (`--json`) line for a prune candidate. A worktree emits its
/// full row; a bare branch emits a small object tagged `"kind": "branch"`.
fn candidate_json(worktrees: &[Worktree], candidate: &Candidate) -> Result<String> {
    match candidate {
        Candidate::Worktree(index) => worktrees[*index].to_json_line(),
        Candidate::Branch { name, merged } => Ok(serde_json::json!({
            "branch": name,
            "kind": "branch",
            "merged": merged,
        })
        .to_string()),
    }
}

/// Whether a worktree is a prune candidate for the given flags.
fn is_candidate(
    repo: &crate::git::discover::Repo,
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
        && is_ancestor(repo.gix(), &branch_ref(branch), default)
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
        .is_some_and(|d| is_ancestor(repo.gix(), &branch_ref(branch), d));
    if merged {
        run_best_effort(
            git,
            root,
            &["branch", "-D", branch],
            "prune: delete merged branch",
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::PruneArgs;
    use crate::testutil::{CannedInput, TestRepo, make_wt, wt_dir};

    fn prune_args(merged: bool, gone: bool, dry_run: bool, force: bool) -> PruneArgs {
        PruneArgs {
            merged,
            gone,
            dry_run,
            force,
        }
    }

    /// A branch at the current tip — an ancestor of the default branch (merged),
    /// with no worktree.
    fn bare_branch(repo: &TestRepo, name: &str) {
        repo.git(&["branch", name]);
    }

    /// A branch carrying its own commit, so it is NOT an ancestor of the default
    /// branch; leaves the repo back on `main`.
    fn diverged_branch(repo: &TestRepo, name: &str) {
        repo.git(&["checkout", "-q", "-b", name]);
        repo.write(&format!("{name}.txt"), "x\n");
        repo.commit_all("diverge");
        repo.git(&["checkout", "-q", "main"]);
    }

    /// Configures an upstream for `name` whose tracking ref does not exist, so
    /// `upstream_of(...).is_gone` is true.
    fn give_gone_upstream(repo: &TestRepo, name: &str) {
        repo.git(&["config", &format!("branch.{name}.remote"), "origin"]);
        repo.git(&[
            "config",
            &format!("branch.{name}.merge"),
            &format!("refs/heads/{name}"),
        ]);
    }

    /// Creates a worktree on `branch` and commits in it so the branch diverges
    /// from main — i.e. it is not merged.
    fn make_unmerged_wt(repo: &TestRepo, branch: &str) {
        make_wt(repo, branch);
        let wt = wt_dir(repo, branch);
        std::fs::write(wt.join("change.txt"), "x\n").unwrap();
        let dir = wt.to_string_lossy().into_owned();
        repo.git(&["-C", &dir, "add", "-A"]);
        repo.git(&["-C", &dir, "commit", "-q", "-m", "unmerged change"]);
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

    #[test]
    fn merged_bare_branch_is_pruned() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old"); // no worktree, merged into main
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, true), false).unwrap();
        assert!(repo.git(&["branch", "--list", "old"]).trim().is_empty());
        // The default/current branch is never touched.
        assert!(repo.git(&["branch", "--list", "main"]).contains("main"));
    }

    #[test]
    fn gone_merged_bare_branch_is_pruned() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old");
        give_gone_upstream(&repo, "old"); // merged AND upstream gone
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(false, true, false, true), false).unwrap();
        assert!(repo.git(&["branch", "--list", "old"]).trim().is_empty());
    }

    #[test]
    fn gone_unmerged_branch_needs_force() {
        let repo = TestRepo::init();
        diverged_branch(&repo, "wip"); // not an ancestor of main
        give_gone_upstream(&repo, "wip");
        // Without --force the protected branch is skipped (confirmation says yes).
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.input = Box::new(CannedInput::new(&["y"]));
        super::run(&mut t.cx, &prune_args(false, true, false, false), false).unwrap();
        assert!(t.err.contents().contains("use --force"));
        assert!(repo.git(&["branch", "--list", "wip"]).contains("wip"));
        // With --force it is force-deleted.
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(false, true, false, true), false).unwrap();
        assert!(repo.git(&["branch", "--list", "wip"]).trim().is_empty());
    }

    #[test]
    fn default_and_current_never_deleted() {
        let repo = TestRepo::init(); // only `main` (default + current)
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, true, false, true), false).unwrap();
        assert!(t.err.contents().contains("nothing to prune"));
        assert!(repo.git(&["branch", "--list", "main"]).contains("main"));
    }

    #[test]
    fn merged_only_skips_unmerged_worktree() {
        // A worktree whose branch is not merged into main must never be a
        // `--merged` prune candidate — a guard against pruning live work.
        let repo = TestRepo::init();
        make_unmerged_wt(&repo, "wip");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, true, false), false).unwrap();
        assert!(t.err.contents().contains("nothing to prune"));
    }

    #[test]
    fn dry_run_lists_bare_branch_without_deleting() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, true, false), false).unwrap();
        assert!(t.out.contents().contains("would remove old (branch)"));
        assert!(repo.git(&["branch", "--list", "old"]).contains("old"));
    }

    #[test]
    fn branch_with_worktree_uses_worktree_path() {
        let repo = TestRepo::init();
        make_wt(&repo, "merged-wt"); // branch + worktree
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, true), false).unwrap();
        // Pruned once via the worktree path — not double-counted as a bare branch.
        assert!(!repo.git(&["worktree", "list"]).contains("merged-wt"));
        assert!(
            repo.git(&["branch", "--list", "merged-wt"])
                .trim()
                .is_empty()
        );
        assert!(t.err.contents().contains("pruned 1 item(s)"));
    }

    #[test]
    fn json_lists_bare_branch() {
        let repo = TestRepo::init();
        bare_branch(&repo, "old");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &prune_args(true, false, false, false), true).unwrap();
        let out = t.out.contents();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["branch"], serde_json::json!("old"));
        assert_eq!(v["kind"], serde_json::json!("branch"));
        // --json implies dry-run: still present.
        assert!(repo.git(&["branch", "--list", "old"]).contains("old"));
    }
}
