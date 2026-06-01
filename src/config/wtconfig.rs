//! Per-worktree metadata stored in Git config under the `wt.*` namespace (spec
//! §3/§7/§11): the base ref, originating PR number, and a "created by wt" flag.
//!
//! Metadata is keyed by branch (`[wt "<branch>"]`), so it is shared across the
//! repo yet unambiguous per worktree. Reads use `gix`; writes use `git config`
//! (a sanctioned §4 fallback — `gix`'s config file-writing is not yet stable).

use std::path::Path;

use crate::error::Result;
use crate::git::cli::GitCli;

/// Per-worktree metadata recorded by `wt`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WtMeta {
    /// Base ref the branch was created from (§3).
    pub base_ref: Option<String>,
    /// Originating PR number, for PR-checkout worktrees (§7).
    pub pr_number: Option<u64>,
    /// Whether the branch/worktree was created by `wt` (§10).
    pub created_by_wt: bool,
}

/// The config key for `wt.<branch>.<name>`.
fn key(branch: &str, name: &str) -> String {
    format!("wt.{branch}.{name}")
}

/// Reads the `wt.*` metadata for `branch` via `gix`.
pub fn read_meta(repo: &gix::Repository, branch: &str) -> WtMeta {
    let config = repo.config_snapshot();
    let base_ref = config
        .string(key(branch, "baseRef").as_str())
        .map(|v| v.to_string());
    let pr_number = config
        .string(key(branch, "prNumber").as_str())
        .and_then(|v| v.to_string().parse::<u64>().ok());
    let created_by_wt = config
        .boolean(key(branch, "createdByWt").as_str())
        .unwrap_or(false);
    WtMeta {
        base_ref,
        pr_number,
        created_by_wt,
    }
}

/// Records the base ref for `branch`.
pub fn write_base_ref(
    git: &dyn GitCli,
    repo_root: &Path,
    branch: &str,
    base_ref: &str,
) -> Result<()> {
    git.run(repo_root, &["config", &key(branch, "baseRef"), base_ref])?;
    Ok(())
}

/// Records the originating PR number for `branch`.
pub fn write_pr_number(
    git: &dyn GitCli,
    repo_root: &Path,
    branch: &str,
    number: u64,
) -> Result<()> {
    git.run(
        repo_root,
        &["config", &key(branch, "prNumber"), &number.to_string()],
    )?;
    Ok(())
}

/// Marks `branch` as created by `wt`.
pub fn mark_created_by_wt(git: &dyn GitCli, repo_root: &Path, branch: &str) -> Result<()> {
    git.run(repo_root, &["config", &key(branch, "createdByWt"), "true"])?;
    Ok(())
}

/// Removes all `wt.*` metadata for `branch` (e.g. after removing its worktree).
/// A missing section is not an error.
pub fn clear_meta(git: &dyn GitCli, repo_root: &Path, branch: &str) -> Result<()> {
    let section = format!("wt.{branch}");
    // `--remove-section` exits non-zero if the section is absent; ignore that.
    git.run_raw(repo_root, &["config", "--remove-section", &section])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::git::discover::Repo;
    use crate::testutil::TestRepo;

    fn meta(repo: &TestRepo, branch: &str) -> WtMeta {
        let r = Repo::discover(repo.root()).unwrap();
        read_meta(r.gix(), branch)
    }

    #[test]
    fn unset_metadata_is_empty() {
        let repo = TestRepo::init();
        assert_eq!(meta(&repo, "main"), WtMeta::default());
    }

    #[test]
    fn base_ref_round_trips() {
        let repo = TestRepo::init();
        write_base_ref(&RealGit, repo.root(), "main", "develop").unwrap();
        assert_eq!(meta(&repo, "main").base_ref.as_deref(), Some("develop"));
    }

    #[test]
    fn pr_number_round_trips() {
        let repo = TestRepo::init();
        write_pr_number(&RealGit, repo.root(), "main", 42).unwrap();
        assert_eq!(meta(&repo, "main").pr_number, Some(42));
    }

    #[test]
    fn created_by_wt_round_trips() {
        let repo = TestRepo::init();
        assert!(!meta(&repo, "main").created_by_wt);
        mark_created_by_wt(&RealGit, repo.root(), "main").unwrap();
        assert!(meta(&repo, "main").created_by_wt);
    }

    #[test]
    fn metadata_works_for_slashed_branch_names() {
        let repo = TestRepo::init();
        write_base_ref(&RealGit, repo.root(), "feature/login", "main").unwrap();
        write_pr_number(&RealGit, repo.root(), "feature/login", 7).unwrap();
        mark_created_by_wt(&RealGit, repo.root(), "feature/login").unwrap();
        let m = meta(&repo, "feature/login");
        assert_eq!(m.base_ref.as_deref(), Some("main"));
        assert_eq!(m.pr_number, Some(7));
        assert!(m.created_by_wt);
    }

    #[test]
    fn clear_removes_all_metadata() {
        let repo = TestRepo::init();
        write_base_ref(&RealGit, repo.root(), "topic", "main").unwrap();
        mark_created_by_wt(&RealGit, repo.root(), "topic").unwrap();
        clear_meta(&RealGit, repo.root(), "topic").unwrap();
        assert_eq!(meta(&repo, "topic"), WtMeta::default());
        // Clearing again (no section) is not an error.
        clear_meta(&RealGit, repo.root(), "topic").unwrap();
    }
}
