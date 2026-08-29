//! Per-worktree metadata stored in Git config under the `wt.*` namespace (spec
//! §3/§7/§11): the base ref, originating PR number, and a "created by wt" flag.
//!
//! Metadata is keyed by branch (`[wt "<branch>"]`), so it is shared across the
//! repo yet unambiguous per worktree. Reads use `gix`; writes use `git config`
//! (a sanctioned §4 fallback — `gix`'s config file-writing is not yet stable).
//!
//! # The metadata contract
//!
//! Every key lives under `wt.<branch>.*` in the repository's git config, and
//! all of them are optional:
//!
//! | Key | Type | Meaning |
//! | --- | --- | --- |
//! | `baseRef` | string | The ref the branch was created from |
//! | `createdByWt` | bool | `wt` created the branch, so `wt` may delete it |
//! | `prNumber` | integer | The originating pull request |
//! | `prState` | string | Cached PR state, so listing works offline |
//! | `prTitle` | string | Cached PR title |
//! | `prUrl` | string | Cached PR URL |
//! | `issueNumber` | integer | The linked GitHub issue |
//! | `issueTitle` | string | Cached issue title |
//! | `issueUrl` | string | Cached issue URL |
//! | `issueBrief` | string | The generated implementation brief |
//!
//! Two rules make the namespace safe to share with an embedder: [`read_meta`]
//! maps a missing key to `None`, and it ignores keys it does not know. So
//! *adding* a key never breaks an older reader, and an embedder may keep its
//! own keys in its own namespace without `wt` disturbing them. What is **not**
//! safe is changing what an existing key means — that is what
//! [`SCHEMA_VERSION`] exists to gate, and why [`ensure_schema_supported`]
//! should run before reading or writing.
//!
//! [`clear_meta`] removes the whole `wt.<branch>` section, so it also removes
//! keys this build has never heard of.

use std::path::Path;

use crate::error::{Error, Result};
use crate::git::cli::GitCli;

/// The metadata schema version this build reads and writes (issue #99).
///
/// The version is a single repo-level `wt.schema` integer, deliberately
/// minimal: a repository with no `wt.schema` is version `1` (every repository
/// initialized to date), readers accept equal-or-lower values, and a *higher*
/// value is refused with an actionable error — it means the metadata's key
/// meanings may have changed and reading them could silently misinterpret
/// them. Purely *additive* keys never need a bump: [`read_meta`] ignores
/// unknown keys and maps missing keys to `None`. `wt` never writes
/// `wt.schema` at version 1; the first meaning-changing version will.
///
/// Embedders (karet) should compare their supported version against
/// [`schema_version`] (or just call [`ensure_schema_supported`]) *before*
/// mutating anything.
pub const SCHEMA_VERSION: u64 = 1;

/// Reads the repository's `wt.schema`, treating a missing key as version `1`.
/// A present but non-positive or unparseable value is a configuration error.
pub fn schema_version(repo: &gix::Repository) -> Result<u64> {
    let config = repo.config_snapshot();
    let Some(raw) = config.string("wt.schema") else {
        return Ok(1);
    };
    raw.to_string()
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|v| *v >= 1)
        .ok_or_else(|| Error::Config {
            file: "git config".into(),
            key: "wt.schema".into(),
            reason: format!("expected a positive integer, got {raw:?}"),
        })
}

/// Fails with [`Error::SchemaTooNew`] when the repository's `wt.schema` is
/// higher than [`SCHEMA_VERSION`]. Call before reading or writing `wt.*`
/// metadata.
pub fn ensure_schema_supported(repo: &gix::Repository) -> Result<()> {
    let found = schema_version(repo)?;
    if found > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Per-worktree metadata recorded by `wt`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WtMeta {
    /// Base ref the branch was created from (§3).
    pub base_ref: Option<String>,
    /// Originating PR number, for PR-checkout worktrees (§7).
    pub pr_number: Option<u64>,
    /// Cached PR state, so `wt list` can show it offline (§3).
    pub pr_state: Option<String>,
    /// Cached PR title.
    pub pr_title: Option<String>,
    /// Cached PR URL, for the TUI detail pane (§10).
    pub pr_url: Option<String>,
    /// Whether the branch/worktree was created by `wt` (§10).
    pub created_by_wt: bool,
    /// Linked GitHub issue number, for issue worktrees (issue #100).
    pub issue_number: Option<u64>,
    /// Cached issue title, so `wt list` can show it offline.
    pub issue_title: Option<String>,
    /// Cached issue URL.
    pub issue_url: Option<String>,
    /// The generated implementation brief. Persisted so an embedder (karet)
    /// can read it instead of regenerating it.
    pub issue_brief: Option<String>,
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
    let pr_state = config
        .string(key(branch, "prState").as_str())
        .map(|v| v.to_string());
    let pr_title = config
        .string(key(branch, "prTitle").as_str())
        .map(|v| v.to_string());
    let pr_url = config
        .string(key(branch, "prUrl").as_str())
        .map(|v| v.to_string());
    let created_by_wt = config
        .boolean(key(branch, "createdByWt").as_str())
        .unwrap_or(false);
    let issue_number = config
        .string(key(branch, "issueNumber").as_str())
        .and_then(|v| v.to_string().parse::<u64>().ok());
    let issue_title = config
        .string(key(branch, "issueTitle").as_str())
        .map(|v| v.to_string());
    let issue_url = config
        .string(key(branch, "issueUrl").as_str())
        .map(|v| v.to_string());
    let issue_brief = config
        .string(key(branch, "issueBrief").as_str())
        .map(|v| v.to_string());
    WtMeta {
        base_ref,
        pr_number,
        pr_state,
        pr_title,
        pr_url,
        created_by_wt,
        issue_number,
        issue_title,
        issue_url,
        issue_brief,
    }
}

/// Records the full cached PR snapshot (number, state, title) for `branch`.
pub fn write_pr(
    git: &dyn GitCli,
    repo_root: &Path,
    branch: &str,
    number: u64,
    state: &str,
    title: &str,
) -> Result<()> {
    write_pr_number(git, repo_root, branch, number)?;
    write_pr_state(git, repo_root, branch, state)?;
    write_pr_title(git, repo_root, branch, title)?;
    Ok(())
}

/// Records the cached PR state for `branch`, so `wt list` can show it offline.
pub fn write_pr_state(git: &dyn GitCli, repo_root: &Path, branch: &str, state: &str) -> Result<()> {
    git.run(repo_root, &["config", &key(branch, "prState"), state])?;
    Ok(())
}

/// Records the cached PR title for `branch`.
pub fn write_pr_title(git: &dyn GitCli, repo_root: &Path, branch: &str, title: &str) -> Result<()> {
    git.run(repo_root, &["config", &key(branch, "prTitle"), title])?;
    Ok(())
}

/// Records the PR URL for `branch` (shown in the TUI detail pane).
pub fn write_pr_url(git: &dyn GitCli, repo_root: &Path, branch: &str, url: &str) -> Result<()> {
    git.run(repo_root, &["config", &key(branch, "prUrl"), url])?;
    Ok(())
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

/// Records the linked issue number for `branch`.
pub fn write_issue_number(
    git: &dyn GitCli,
    repo_root: &Path,
    branch: &str,
    number: u64,
) -> Result<()> {
    git.run(
        repo_root,
        &["config", &key(branch, "issueNumber"), &number.to_string()],
    )?;
    Ok(())
}

/// Records the cached issue title for `branch`.
pub fn write_issue_title(
    git: &dyn GitCli,
    repo_root: &Path,
    branch: &str,
    title: &str,
) -> Result<()> {
    git.run(repo_root, &["config", &key(branch, "issueTitle"), title])?;
    Ok(())
}

/// Records the issue URL for `branch`.
pub fn write_issue_url(git: &dyn GitCli, repo_root: &Path, branch: &str, url: &str) -> Result<()> {
    git.run(repo_root, &["config", &key(branch, "issueUrl"), url])?;
    Ok(())
}

/// Records the generated implementation brief for `branch`.
pub fn write_issue_brief(
    git: &dyn GitCli,
    repo_root: &Path,
    branch: &str,
    brief: &str,
) -> Result<()> {
    git.run(repo_root, &["config", &key(branch, "issueBrief"), brief])?;
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
    fn write_pr_caches_number_state_and_title() {
        let repo = TestRepo::init();
        write_pr(&RealGit, repo.root(), "main", 99, "open", "Add feature").unwrap();
        let m = meta(&repo, "main");
        assert_eq!(m.pr_number, Some(99));
        assert_eq!(m.pr_state.as_deref(), Some("open"));
        assert_eq!(m.pr_title.as_deref(), Some("Add feature"));
    }

    /// Opens a fresh gix handle on the repo (config is snapshotted at open).
    fn gix_of(repo: &TestRepo) -> gix::Repository {
        gix::discover(repo.root()).unwrap()
    }

    #[test]
    fn missing_schema_is_version_one_and_supported() {
        // Every repository initialized to date has no wt.schema.
        let repo = TestRepo::init();
        assert_eq!(schema_version(&gix_of(&repo)).unwrap(), 1);
        ensure_schema_supported(&gix_of(&repo)).unwrap();
    }

    #[test]
    fn equal_schema_is_supported() {
        let repo = TestRepo::init();
        repo.git(&["config", "wt.schema", &SCHEMA_VERSION.to_string()]);
        assert_eq!(schema_version(&gix_of(&repo)).unwrap(), SCHEMA_VERSION);
        ensure_schema_supported(&gix_of(&repo)).unwrap();
    }

    #[test]
    fn future_schema_is_refused_with_an_upgrade_error() {
        let repo = TestRepo::init();
        repo.git(&["config", "wt.schema", "2"]);
        let err = ensure_schema_supported(&gix_of(&repo)).unwrap_err();
        assert!(matches!(
            err,
            Error::SchemaTooNew {
                found: 2,
                supported: SCHEMA_VERSION,
            }
        ));
        let message = err.to_string();
        assert!(message.contains("wt.schema = 2"), "{message}");
        assert!(message.contains("upgrade wt"), "{message}");
    }

    #[test]
    fn garbage_schema_is_a_config_error() {
        for bad in ["banana", "0", "-3"] {
            let repo = TestRepo::init();
            repo.git(&["config", "wt.schema", bad]);
            let err = schema_version(&gix_of(&repo)).unwrap_err();
            assert!(
                matches!(&err, Error::Config { key, .. } if key == "wt.schema"),
                "{bad}: {err:?}"
            );
        }
    }

    #[test]
    fn issue_link_round_trips() {
        let repo = TestRepo::init();
        write_issue_number(&RealGit, repo.root(), "topic", 7).unwrap();
        write_issue_title(&RealGit, repo.root(), "topic", "Add login").unwrap();
        write_issue_url(&RealGit, repo.root(), "topic", "https://example.com/7").unwrap();
        write_issue_brief(&RealGit, repo.root(), "topic", "Wire up the form.").unwrap();
        let got = meta(&repo, "topic");
        assert_eq!(got.issue_number, Some(7));
        assert_eq!(got.issue_title.as_deref(), Some("Add login"));
        assert_eq!(got.issue_url.as_deref(), Some("https://example.com/7"));
        assert_eq!(got.issue_brief.as_deref(), Some("Wire up the form."));
    }

    #[test]
    fn issue_keys_are_absent_until_written() {
        // The issue keys are purely additive: a repository written by an older
        // build has none of them, and every one must read back as `None` rather
        // than fail. This is what makes them safe without a `wt.schema` bump.
        let repo = TestRepo::init();
        write_base_ref(&RealGit, repo.root(), "topic", "main").unwrap();
        let got = meta(&repo, "topic");
        assert_eq!(got.issue_number, None);
        assert_eq!(got.issue_title, None);
        assert_eq!(got.issue_url, None);
        assert_eq!(got.issue_brief, None);
    }

    #[test]
    fn clear_removes_all_metadata() {
        let repo = TestRepo::init();
        write_base_ref(&RealGit, repo.root(), "topic", "main").unwrap();
        mark_created_by_wt(&RealGit, repo.root(), "topic").unwrap();
        // Every key the section can hold, so `--remove-section` is proven to
        // clear the issue keys too and not just the ones it predates.
        write_pr(&RealGit, repo.root(), "topic", 42, "open", "Title").unwrap();
        write_pr_url(&RealGit, repo.root(), "topic", "https://example.com/42").unwrap();
        write_issue_number(&RealGit, repo.root(), "topic", 7).unwrap();
        write_issue_title(&RealGit, repo.root(), "topic", "Add login").unwrap();
        write_issue_url(&RealGit, repo.root(), "topic", "https://example.com/7").unwrap();
        write_issue_brief(&RealGit, repo.root(), "topic", "Wire up the form.").unwrap();
        clear_meta(&RealGit, repo.root(), "topic").unwrap();
        assert_eq!(meta(&repo, "topic"), WtMeta::default());
        // Clearing again (no section) is not an error.
        clear_meta(&RealGit, repo.root(), "topic").unwrap();
    }
}
