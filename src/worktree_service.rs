//! Worktree assembly and shared safety guards.
//!
//! Builds the [`Worktree`] rows (spec §7) by combining the synchronous
//! enumeration (paths, branches, current marker) with the per-row enrichment
//! that the TUI loads asynchronously (dirty/untracked, ahead/behind, commit, PR).
//! Also defines the remove/prune guards (spec §10/§12) shared by the CLI and TUI.

use std::path::Path;

use crate::config::wtconfig::{self, WtMeta};
use crate::error::Result;
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{
    abbrev_len, ahead_behind, commit_info, enumerate, recent_commits, resolve_hex, status_of,
    upstream_of,
};
use crate::model::{Commit, Pr, PrState, Worktree};
use crate::slug::slugify;
use crate::time::iso8601;

/// Whether two paths refer to the same location, comparing canonicalized forms
/// when possible (handles `/private` symlinks on macOS).
fn same_path(a: &Path, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// Enumerates the repository's worktrees with their synchronous fields only
/// (path, branch, slug, current/main/missing/detached markers). The bare-repo
/// hub entry is excluded (it is not a checkout). Spec §10 "Synchronous".
pub fn enumerate_worktrees(repo: &Repo, git: &dyn GitCli) -> Result<Vec<Worktree>> {
    let dir = repo.current_workdir().unwrap_or_else(|| repo.git_dir());
    let raws = enumerate(git, &dir)?;
    let current = repo.current_workdir();
    let mut out = Vec::with_capacity(raws.len());
    for raw in raws {
        if raw.is_bare {
            continue;
        }
        let mut wt = Worktree::new(raw.path.clone());
        wt.is_main = raw.is_main;
        wt.is_missing = raw.is_missing;
        wt.is_detached = raw.is_detached;
        wt.branch = raw.branch.clone();
        wt.slug = raw.branch.as_deref().map(slugify);
        wt.is_current = current
            .as_deref()
            .is_some_and(|cur| same_path(cur, &raw.path));
        out.push(wt);
    }
    Ok(out)
}

/// Fills a worktree's asynchronously-loaded fields (spec §10): base ref and PR
/// from `wt.*` config, upstream + ahead/behind, dirty/untracked, and the tip
/// commit. Best-effort: a failed read leaves the field unset rather than
/// erroring. Missing worktrees keep only their admin-derived fields.
pub fn enrich_worktree(repo: &Repo, git: &dyn GitCli, abbrev: usize, wt: &mut Worktree) {
    if let Some(branch) = wt.branch.clone() {
        let meta = wtconfig::read_meta(repo.gix(), &branch);
        wt.base_ref = meta.base_ref.clone();
        wt.pr = build_pr(&meta);
        wt.pr_url = meta.pr_url.clone();
        // Upstream is read from config and is known even for a missing worktree;
        // ahead/behind needs the working directory, so it is computed only when
        // the worktree exists and the upstream is not gone.
        if let Some(upstream) = upstream_of(repo.gix(), &branch) {
            wt.upstream = Some(upstream.display.clone());
            if !wt.is_missing
                && !upstream.is_gone
                && let Ok((ahead, behind)) = ahead_behind(
                    git,
                    &wt.path,
                    &upstream.tracking_ref,
                    &format!("refs/heads/{branch}"),
                )
            {
                wt.ahead = Some(ahead);
                wt.behind = Some(behind);
            }
        }
    }

    if wt.is_missing {
        return;
    }

    if let Ok(status) = status_of(git, &wt.path) {
        wt.dirty = Some(status.dirty);
        wt.has_untracked = Some(status.has_untracked);
    }

    if let Some(oid) = tip_oid(repo, git, wt) {
        if let Ok(info) = commit_info(repo.gix(), &oid, abbrev) {
            wt.commit = Some(Commit {
                hash: info.hash,
                subject: info.subject,
                author: info.author,
                timestamp: iso8601(info.timestamp_unix),
            });
        }
        // The last few commits power the TUI detail pane (spec §10).
        wt.recent_commits = recent_commits(repo.gix(), &oid, abbrev, 5)
            .into_iter()
            .map(|info| Commit {
                hash: info.hash,
                subject: info.subject,
                author: info.author,
                timestamp: iso8601(info.timestamp_unix),
            })
            .collect();
    }
}

/// Resolves the tip commit OID of a worktree: the branch ref for a branch
/// worktree, or `git rev-parse HEAD` for a detached one.
fn tip_oid(repo: &Repo, git: &dyn GitCli, wt: &Worktree) -> Option<String> {
    match &wt.branch {
        Some(branch) => resolve_hex(repo.gix(), &format!("refs/heads/{branch}")),
        None => git
            .run(&wt.path, &["rev-parse", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

/// Builds the PR row from cached `wt.*` metadata, if a PR number is recorded.
fn build_pr(meta: &WtMeta) -> Option<Pr> {
    let number = meta.pr_number?;
    let state = meta
        .pr_state
        .as_deref()
        .and_then(PrState::parse)
        .unwrap_or(PrState::Open);
    let title = meta.pr_title.clone().unwrap_or_default();
    Some(Pr {
        number,
        state,
        title,
    })
}

/// Enumerates and fully enriches all worktrees (used by the CLI, which has no
/// async loading phase).
pub fn build_worktrees(repo: &Repo, git: &dyn GitCli) -> Result<Vec<Worktree>> {
    let abbrev = abbrev_len(repo.gix());
    let mut worktrees = enumerate_worktrees(repo, git)?;
    for wt in &mut worktrees {
        enrich_worktree(repo, git, abbrev, wt);
    }
    Ok(worktrees)
}

/// A comparable sort key value (text or numeric).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum SortValue {
    /// A textual key (e.g. branch, path).
    Text(String),
    /// A numeric key (e.g. ahead count, dirty rank, negated activity time).
    Num(i64),
}

/// Extracts the sort key for a worktree, or `None` when the worktree has no
/// value for that key (it sorts last).
fn sort_value(worktree: &Worktree, key: crate::model::SortKey) -> Option<SortValue> {
    use crate::model::SortKey;
    match key {
        SortKey::Branch => worktree.branch.clone().map(SortValue::Text),
        SortKey::Path => Some(SortValue::Text(
            worktree.path.to_string_lossy().into_owned(),
        )),
        SortKey::Ahead => worktree.ahead.map(|a| SortValue::Num(i64::from(a))),
        SortKey::Behind => worktree.behind.map(|b| SortValue::Num(i64::from(b))),
        SortKey::Dirty => dirty_rank(worktree).map(SortValue::Num),
        // Negated so that ascending order is most-recent first (spec §7).
        SortKey::Activity => worktree
            .commit
            .as_ref()
            .and_then(|c| crate::time::parse_iso8601(&c.timestamp))
            .map(|unix| SortValue::Num(-unix)),
    }
}

/// The dirty-sort rank: modified/staged (0) before untracked-only (1) before
/// clean (2); a missing worktree has no rank and sorts last.
fn dirty_rank(worktree: &Worktree) -> Option<i64> {
    match worktree.dirty {
        None => None,
        Some(true) => Some(0),
        Some(false) => Some(if worktree.has_untracked == Some(true) {
            1
        } else {
            2
        }),
    }
}

/// Sorts worktrees in place by the given spec (spec §7). Worktrees with no value
/// for the sort key sort last regardless of direction.
pub fn sort_worktrees(worktrees: &mut [Worktree], spec: crate::model::SortSpec) {
    worktrees.sort_by(|a, b| {
        let ka = sort_value(a, spec.key);
        let kb = sort_value(b, spec.key);
        match (ka, kb) {
            (Some(x), Some(y)) => {
                if spec.descending {
                    y.cmp(&x)
                } else {
                    x.cmp(&y)
                }
            }
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
}

/// The result of evaluating the remove/prune safety guards (spec §10/§12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuardStatus {
    /// Modified/staged tracked files (or untracked when `untracked_blocks`).
    pub dirty: bool,
    /// Local commits ahead of upstream, or no upstream configured.
    pub unpushed: bool,
}

impl GuardStatus {
    /// Whether either guard would block removal without `--force`.
    pub fn blocks(self) -> bool {
        self.dirty || self.unpushed
    }
}

/// Evaluates the remove/prune guards for a worktree (spec §10/§12). "Dirty" is
/// modified/staged tracked files (plus untracked when `untracked_blocks`);
/// "unpushed" is `ahead > 0`, and a branch with no upstream counts as unpushed.
pub fn guard_status(worktree: &Worktree, untracked_blocks: bool) -> GuardStatus {
    // An unknown dirty state on a *present* worktree (e.g. the status read
    // failed) is treated as dirty so the guard fails safe; a missing worktree's
    // `None` is legitimate and must not block (it skips the guards entirely).
    let dirty = worktree.dirty.unwrap_or(!worktree.is_missing)
        || (untracked_blocks && worktree.has_untracked.unwrap_or(false));
    let unpushed = worktree.ahead.is_none_or(|ahead| ahead > 0);
    GuardStatus { dirty, unpushed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::model::Worktree;
    use crate::testutil::TestRepo;
    use std::path::PathBuf;

    #[test]
    fn builds_rows_for_main_and_linked() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let r = Repo::discover(repo.root()).unwrap();
        let worktrees = build_worktrees(&r, &RealGit).unwrap();
        assert_eq!(worktrees.len(), 2);

        let main = worktrees.iter().find(|w| w.is_main).unwrap();
        assert_eq!(main.branch.as_deref(), Some("main"));
        assert!(main.is_current);
        assert_eq!(main.dirty, Some(false));
        assert!(main.commit.is_some());
        assert_eq!(main.commit.as_ref().unwrap().subject, "init");
        // The detail-pane recent-commits list is populated (spec §10).
        assert_eq!(main.recent_commits.len(), 1);
        assert_eq!(main.recent_commits[0].subject, "init");

        let feat = worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some("feature/x"))
            .unwrap();
        assert_eq!(feat.slug.as_deref(), Some("feature-x"));
        assert!(!feat.is_current);
    }

    #[test]
    fn dirty_and_untracked_are_distinguished() {
        let repo = TestRepo::init();
        repo.write("README.md", "changed\n");
        repo.write("scratch.txt", "x\n");
        let r = Repo::discover(repo.root()).unwrap();
        let worktrees = build_worktrees(&r, &RealGit).unwrap();
        let main = &worktrees[0];
        assert_eq!(main.dirty, Some(true));
        assert_eq!(main.has_untracked, Some(true));
    }

    #[test]
    fn ahead_behind_and_upstream_populated() {
        let repo = TestRepo::init();
        let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &base]);
        repo.git(&["config", "branch.main.remote", "origin"]);
        repo.git(&["config", "branch.main.merge", "refs/heads/main"]);
        repo.write("a.txt", "1\n");
        repo.commit_all("c1");
        let r = Repo::discover(repo.root()).unwrap();
        let worktrees = build_worktrees(&r, &RealGit).unwrap();
        let main = &worktrees[0];
        assert_eq!(main.upstream.as_deref(), Some("origin/main"));
        assert_eq!(main.ahead, Some(1));
        assert_eq!(main.behind, Some(0));
    }

    #[test]
    fn base_ref_and_pr_from_wt_config() {
        let repo = TestRepo::init();
        wtconfig::write_base_ref(&RealGit, repo.root(), "main", "develop").unwrap();
        wtconfig::write_pr(&RealGit, repo.root(), "main", 42, "open", "Add login").unwrap();
        let r = Repo::discover(repo.root()).unwrap();
        let worktrees = build_worktrees(&r, &RealGit).unwrap();
        let main = &worktrees[0];
        assert_eq!(main.base_ref.as_deref(), Some("develop"));
        let pr = main.pr.as_ref().unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.title, "Add login");
    }

    #[test]
    fn missing_worktree_keeps_only_admin_fields() {
        let repo = TestRepo::init();
        repo.add_worktree("gone", "../wt-gone");
        wtconfig::write_base_ref(&RealGit, repo.root(), "gone", "main").unwrap();
        let linked = repo.root().parent().unwrap().join("wt-gone");
        std::fs::remove_dir_all(&linked).unwrap();
        let r = Repo::discover(repo.root()).unwrap();
        let worktrees = build_worktrees(&r, &RealGit).unwrap();
        let gone = worktrees
            .iter()
            .find(|w| w.branch.as_deref() == Some("gone"))
            .unwrap();
        assert!(gone.is_missing);
        assert_eq!(gone.base_ref.as_deref(), Some("main")); // admin-derived
        assert!(gone.dirty.is_none());
        assert!(gone.ahead.is_none());
        assert!(gone.commit.is_none());
    }

    #[test]
    fn bare_repo_lists_no_hub_row() {
        let repo = TestRepo::init_bare();
        let r = Repo::discover(repo.root()).unwrap();
        let worktrees = build_worktrees(&r, &RealGit).unwrap();
        assert!(worktrees.is_empty());
    }

    fn guard_wt(dirty: Option<bool>, untracked: Option<bool>, ahead: Option<u32>) -> Worktree {
        let mut w = Worktree::new(PathBuf::from("/r"));
        w.dirty = dirty;
        w.has_untracked = untracked;
        w.ahead = ahead;
        w
    }

    #[test]
    fn sort_by_branch_and_direction() {
        use crate::model::{SortKey, SortSpec};
        let mut worktrees = vec![wt_named("zebra"), wt_named("alpha"), wt_named("mango")];
        sort_worktrees(
            &mut worktrees,
            SortSpec {
                key: SortKey::Branch,
                descending: false,
            },
        );
        assert_eq!(branches(&worktrees), vec!["alpha", "mango", "zebra"]);
        sort_worktrees(
            &mut worktrees,
            SortSpec {
                key: SortKey::Branch,
                descending: true,
            },
        );
        assert_eq!(branches(&worktrees), vec!["zebra", "mango", "alpha"]);
    }

    #[test]
    fn sort_nulls_last_regardless_of_direction() {
        use crate::model::{SortKey, SortSpec};
        let mut a = wt_named("has-upstream");
        a.ahead = Some(5);
        let b = wt_named("no-upstream"); // ahead None -> sorts last
        let mut worktrees = vec![b.clone(), a.clone()];
        sort_worktrees(
            &mut worktrees,
            SortSpec {
                key: SortKey::Ahead,
                descending: false,
            },
        );
        assert_eq!(branches(&worktrees), vec!["has-upstream", "no-upstream"]);
        // Descending still keeps the null last.
        sort_worktrees(
            &mut worktrees,
            SortSpec {
                key: SortKey::Ahead,
                descending: true,
            },
        );
        assert_eq!(branches(&worktrees), vec!["has-upstream", "no-upstream"]);
    }

    #[test]
    fn sort_dirty_ranks_modified_first() {
        use crate::model::{SortKey, SortSpec};
        let mut modified = wt_named("modified");
        modified.dirty = Some(true);
        let mut untracked = wt_named("untracked");
        untracked.dirty = Some(false);
        untracked.has_untracked = Some(true);
        let mut clean = wt_named("clean");
        clean.dirty = Some(false);
        clean.has_untracked = Some(false);
        let mut worktrees = vec![clean, untracked, modified];
        sort_worktrees(
            &mut worktrees,
            SortSpec {
                key: SortKey::Dirty,
                descending: false,
            },
        );
        assert_eq!(branches(&worktrees), vec!["modified", "untracked", "clean"]);
    }

    fn wt_named(branch: &str) -> Worktree {
        let mut w = Worktree::new(PathBuf::from(format!("/r/{branch}")));
        w.branch = Some(branch.to_string());
        w
    }

    fn branches(worktrees: &[Worktree]) -> Vec<&str> {
        worktrees
            .iter()
            .filter_map(|w| w.branch.as_deref())
            .collect()
    }

    #[test]
    fn guards_dirty_and_unpushed() {
        // Clean + pushed (ahead 0).
        let g = guard_status(&guard_wt(Some(false), Some(false), Some(0)), false);
        assert!(!g.dirty && !g.unpushed && !g.blocks());
        // Dirty tracked changes block.
        assert!(guard_status(&guard_wt(Some(true), Some(false), Some(0)), false).dirty);
        // Untracked only does not block by default...
        assert!(!guard_status(&guard_wt(Some(false), Some(true), Some(0)), false).dirty);
        // ...but does when untracked_blocks is set.
        assert!(guard_status(&guard_wt(Some(false), Some(true), Some(0)), true).dirty);
        // Ahead > 0 is unpushed.
        assert!(guard_status(&guard_wt(Some(false), Some(false), Some(3)), false).unpushed);
        // No upstream (ahead None) counts as unpushed.
        assert!(guard_status(&guard_wt(Some(false), Some(false), None), false).unpushed);
    }

    #[test]
    fn guard_unknown_dirty_fails_safe_for_present_worktree() {
        // A present worktree with an unknown dirty state (status read failed)
        // is treated as dirty so removal fails safe.
        let mut wt = guard_wt(None, None, Some(0));
        assert!(guard_status(&wt, false).dirty);
        // A missing worktree's `None` dirty is legitimate and must not block.
        wt.is_missing = true;
        assert!(!guard_status(&wt, false).dirty);
    }
}
