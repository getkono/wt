//! Worktree assembly and shared safety guards.
//!
//! Builds the [`Worktree`] rows (spec §7) by combining the synchronous
//! enumeration (paths, branches, current marker) with the per-row enrichment
//! that the TUI loads asynchronously (dirty/untracked, ahead/behind, commit, PR).
//! Also defines the remove/prune guards (spec §10/§12) shared by the CLI and TUI.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::wtconfig::{self, WtMeta};
use crate::error::Result;
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{
    Upstream, abbrev_len, ahead_behind, commit_info, default_branch, enumerate, is_ancestor,
    local_branches, recent_commits, resolve_hex, status_of, upstream_of,
};
use crate::model::{Commit, MergeState, Pr, PrState, Worktree};
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
        let upstream = upstream_of(repo.gix(), &branch);
        if let Some(up) = &upstream {
            wt.upstream = Some(up.display.clone());
            if !wt.is_missing
                && !up.is_gone
                && let Ok((ahead, behind)) = ahead_behind(
                    git,
                    &wt.path,
                    &up.tracking_ref,
                    &format!("refs/heads/{branch}"),
                )
            {
                wt.ahead = Some(ahead);
                wt.behind = Some(behind);
            }
        }
        // Offline merge-state for delete-safety messaging; unknowable for a
        // missing worktree (no checkout to query), left `None` there.
        if !wt.is_missing {
            let state = compute_merge_state(repo, git, wt, &branch, upstream.as_ref());
            wt.merge_state = state;
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

/// Determines a branch worktree's offline merge state (spec §10) for the TUI's
/// delete-safety messaging. The checks are tried in order — first match wins —
/// so the result is the strongest available evidence:
///
/// 1. a live upstream → [`MergeState::Tracked`] (ahead/behind carries detail);
/// 2. the tip is an ancestor of the recorded `base_ref`, then the default branch
///    → [`MergeState::Merged`] naming that ref (a regular merge / fast-forward);
/// 3. a recorded merged PR → [`MergeState::Merged`] with no ref (catches a
///    squash/rebase merge, whose commit hash differs so ancestry cannot prove it);
/// 4. an upstream configured but gone → [`MergeState::UpstreamGone`];
/// 5. otherwise → [`MergeState::NoUpstreamLocal`].
///
/// Callers must skip missing worktrees (there is no checkout to query).
fn compute_merge_state(
    repo: &Repo,
    git: &dyn GitCli,
    wt: &Worktree,
    branch: &str,
    upstream: Option<&Upstream>,
) -> Option<MergeState> {
    // A live (present) upstream: ahead/behind already describes the state.
    if let Some(up) = upstream
        && !up.is_gone
    {
        return Some(MergeState::Tracked);
    }
    let branch_ref = format!("refs/heads/{branch}");
    // Ancestry needs a resolvable tip; without one, fall through to the
    // upstream/no-upstream signals below.
    if resolve_hex(repo.gix(), &branch_ref).is_some() {
        let mut tried: Vec<String> = Vec::new();
        for target in [wt.base_ref.clone(), default_branch(repo.gix())]
            .into_iter()
            .flatten()
        {
            // The branch is trivially an ancestor of itself; never report that.
            if target == branch || tried.contains(&target) {
                continue;
            }
            if is_ancestor(git, &wt.path, &branch_ref, &target) {
                return Some(MergeState::Merged { into: Some(target) });
            }
            tried.push(target);
        }
    }
    // A merged PR proves the merge even when ancestry cannot (squash/rebase).
    if wt.pr.as_ref().map(|pr| pr.state) == Some(PrState::Merged) {
        return Some(MergeState::Merged { into: None });
    }
    // Reaching here with an upstream means it was configured but gone (a present
    // upstream returned `Tracked` above) — a strong "remote branch deleted after
    // merge" hint, distinct from never having had an upstream.
    if upstream.is_some() {
        return Some(MergeState::UpstreamGone);
    }
    Some(MergeState::NoUpstreamLocal)
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

/// The virtual path key for a worktree-less branch row (issue #47). A branch row
/// has no checkout, so this stable, non-filesystem sentinel exists only to key
/// the row uniquely (selection and async-load tracking are path-keyed); it is
/// never shown to the user or used as a real path.
fn branch_row_path(branch: &str) -> PathBuf {
    PathBuf::from(format!("branch://{branch}"))
}

/// The local branches that have no worktree, given the already-enumerated
/// worktrees (issue #47). Best-effort: an empty list when branch enumeration
/// fails.
fn branchless_local_branches(repo: &Repo, worktrees: &[Worktree]) -> Vec<String> {
    let checked_out: HashSet<&str> = worktrees
        .iter()
        .filter_map(|w| w.branch.as_deref())
        .collect();
    local_branches(repo.gix())
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !checked_out.contains(name.as_str()))
        .collect()
}

/// A bare (synchronous) branch row: branch name + slug only, marked
/// worktree-less. The async fields (ahead/behind, commit) load later via
/// [`enrich_branch_row`], so it renders with spinners first like a worktree row.
fn branch_row(branch: &str) -> Worktree {
    let mut row = Worktree::new(branch_row_path(branch));
    row.has_worktree = false;
    row.branch = Some(branch.to_string());
    row.slug = Some(slugify(branch));
    row
}

/// Fills a worktree-less branch row's asynchronously-loaded fields (issue #47):
/// base ref + PR from `wt.*` config, the configured upstream (informational
/// only), the tip commit, and — the point of the row — its ahead/behind relative
/// to its base. The base is the recorded `wt.<branch>.baseRef`, falling back to
/// the repo's default branch; ahead/behind is left unset when no base resolves.
/// Best-effort, mirroring [`enrich_worktree`].
fn enrich_branch_row(repo: &Repo, git: &dyn GitCli, abbrev: usize, row: &mut Worktree) {
    let Some(branch) = row.branch.clone() else {
        return;
    };
    let meta = wtconfig::read_meta(repo.gix(), &branch);
    row.base_ref = meta.base_ref.clone();
    row.pr = build_pr(&meta);
    row.pr_url = meta.pr_url.clone();
    if let Some(up) = upstream_of(repo.gix(), &branch) {
        row.upstream = Some(up.display);
    }

    // Ahead/behind relative to the base (recorded base, else the default
    // branch); refs resolve repo-globally, so it runs from any worktree dir.
    let branch_ref = format!("refs/heads/{branch}");
    let dir = repo.current_workdir().unwrap_or_else(|| repo.git_dir());
    if let Some(base) = row.base_ref.clone().or_else(|| default_branch(repo.gix()))
        && base != branch
        && resolve_hex(repo.gix(), &base).is_some()
        && let Ok((ahead, behind)) = ahead_behind(git, &dir, &base, &branch_ref)
    {
        row.ahead = Some(ahead);
        row.behind = Some(behind);
    }

    if let Some(oid) = resolve_hex(repo.gix(), &branch_ref) {
        if let Ok(info) = commit_info(repo.gix(), &oid, abbrev) {
            row.commit = Some(Commit {
                hash: info.hash,
                subject: info.subject,
                author: info.author,
                timestamp: iso8601(info.timestamp_unix),
            });
        }
        row.recent_commits = recent_commits(repo.gix(), &oid, abbrev, 5)
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

/// Enumerates worktrees plus the synchronous, unenriched branch rows for every
/// local branch without a worktree (issue #47) — the immediate listing the TUI
/// paints before async enrichment fills in ahead/behind. Branch rows render with
/// spinners until [`build_rows`] replaces them.
pub fn enumerate_rows(repo: &Repo, git: &dyn GitCli) -> Result<Vec<Worktree>> {
    let mut rows = enumerate_worktrees(repo, git)?;
    let names = branchless_local_branches(repo, &rows);
    rows.extend(names.iter().map(|name| branch_row(name)));
    Ok(rows)
}

/// Builds fully-enriched worktrees plus enriched worktree-less branch rows
/// (issue #47): the TUI's complete listing, loaded off the event loop. The CLI
/// uses [`build_worktrees`] (worktrees only); branch rows are TUI-only.
pub fn build_rows(repo: &Repo, git: &dyn GitCli) -> Result<Vec<Worktree>> {
    let mut rows = build_worktrees(repo, git)?;
    let abbrev = abbrev_len(repo.gix());
    for name in branchless_local_branches(repo, &rows) {
        let mut row = branch_row(&name);
        enrich_branch_row(repo, git, abbrev, &mut row);
        rows.push(row);
    }
    Ok(rows)
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

/// Sorts worktrees by `spec`, groups any worktree-less branch rows (issue #47)
/// below the real worktrees, then pins the primary ("base") worktree to the
/// front so the TUI always shows it first regardless of the active sort
/// (issue #4). The CLI's `list` calls [`sort_worktrees`] directly and keeps the
/// pure `--sort` order (it never has branch rows).
pub fn sort_worktrees_base_first(worktrees: &mut [Worktree], spec: crate::model::SortSpec) {
    sort_worktrees(worktrees, spec);
    // Branch rows always sit beneath the real worktrees, each group keeping its
    // sorted order (`sort_by_key` is stable; `false` < `true` ranks worktrees
    // first). A no-op when there are no branch rows (the CLI path).
    worktrees.sort_by_key(|w| !w.has_worktree);
    // Stable move-to-front of the primary worktree, preserving the sorted order
    // of the remaining rows (the primary is always a real worktree).
    if let Some(pos) = worktrees.iter().position(|w| w.is_main) {
        worktrees[..=pos].rotate_right(1);
    }
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
        // Merge state is unknowable without a checkout to query.
        assert!(gone.merge_state.is_none());
    }

    /// Builds a worktree row pointing at `repo` for a direct `compute_merge_state`
    /// call: branch + path set, with optional recorded base and PR.
    fn merge_wt(
        repo: &TestRepo,
        branch: &str,
        base: Option<&str>,
        pr: Option<PrState>,
    ) -> Worktree {
        let mut wt = Worktree::new(repo.root().to_path_buf());
        wt.branch = Some(branch.to_string());
        wt.base_ref = base.map(str::to_string);
        wt.pr = pr.map(|state| Pr {
            number: 1,
            state,
            title: String::new(),
        });
        wt
    }

    /// Checks out a new branch off the current tip, adds a commit so it diverges,
    /// and returns to `main`. The branch is then NOT an ancestor of `main`.
    fn divergent_branch(repo: &TestRepo, branch: &str) {
        repo.git(&["checkout", "-q", "-b", branch]);
        repo.write(&format!("{branch}.txt"), "x\n");
        repo.commit_all("diverge");
        repo.git(&["checkout", "-q", "main"]);
    }

    #[test]
    fn merge_state_tracked_with_live_upstream() {
        let repo = TestRepo::init();
        let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &head]);
        repo.git(&["config", "branch.main.remote", "origin"]);
        repo.git(&["config", "branch.main.merge", "refs/heads/main"]);
        let r = Repo::discover(repo.root()).unwrap();
        let up = upstream_of(r.gix(), "main");
        let wt = merge_wt(&repo, "main", None, None);
        assert_eq!(
            compute_merge_state(&r, &RealGit, &wt, "main", up.as_ref()),
            Some(MergeState::Tracked)
        );
    }

    #[test]
    fn merge_state_merged_into_base() {
        let repo = TestRepo::init();
        repo.git(&["branch", "feat"]); // at main's tip → ancestor of main
        let r = Repo::discover(repo.root()).unwrap();
        let wt = merge_wt(&repo, "feat", Some("main"), None);
        assert_eq!(
            compute_merge_state(&r, &RealGit, &wt, "feat", None),
            Some(MergeState::Merged {
                into: Some("main".into())
            })
        );
    }

    #[test]
    fn merge_state_merged_into_default_branch() {
        let repo = TestRepo::init();
        repo.git(&["branch", "feat"]); // ancestor of main (the default branch)
        let r = Repo::discover(repo.root()).unwrap();
        // No base recorded: the default-branch fallback must still detect it.
        let wt = merge_wt(&repo, "feat", None, None);
        assert_eq!(
            compute_merge_state(&r, &RealGit, &wt, "feat", None),
            Some(MergeState::Merged {
                into: Some("main".into())
            })
        );
    }

    #[test]
    fn merge_state_merged_via_pr_when_not_ancestor() {
        let repo = TestRepo::init();
        divergent_branch(&repo, "feat"); // NOT an ancestor of main
        let r = Repo::discover(repo.root()).unwrap();
        let wt = merge_wt(&repo, "feat", Some("main"), Some(PrState::Merged));
        assert_eq!(
            compute_merge_state(&r, &RealGit, &wt, "feat", None),
            Some(MergeState::Merged { into: None })
        );
    }

    #[test]
    fn merge_state_upstream_gone() {
        let repo = TestRepo::init();
        divergent_branch(&repo, "feat");
        // Configure an upstream whose tracking ref does not exist → gone.
        repo.git(&["config", "branch.feat.remote", "origin"]);
        repo.git(&["config", "branch.feat.merge", "refs/heads/feat"]);
        let r = Repo::discover(repo.root()).unwrap();
        let up = upstream_of(r.gix(), "feat");
        assert!(up.as_ref().unwrap().is_gone);
        let wt = merge_wt(&repo, "feat", Some("main"), None);
        assert_eq!(
            compute_merge_state(&r, &RealGit, &wt, "feat", up.as_ref()),
            Some(MergeState::UpstreamGone)
        );
    }

    #[test]
    fn merge_state_no_upstream_local() {
        let repo = TestRepo::init();
        divergent_branch(&repo, "feat");
        let r = Repo::discover(repo.root()).unwrap();
        let wt = merge_wt(&repo, "feat", Some("main"), None);
        assert_eq!(
            compute_merge_state(&r, &RealGit, &wt, "feat", None),
            Some(MergeState::NoUpstreamLocal)
        );
    }

    #[test]
    fn merge_state_ancestry_wins_over_merged_pr() {
        let repo = TestRepo::init();
        repo.git(&["branch", "feat"]); // ancestor of main
        let r = Repo::discover(repo.root()).unwrap();
        // Both an ancestor of base AND a merged PR: the named ancestry wins.
        let wt = merge_wt(&repo, "feat", Some("main"), Some(PrState::Merged));
        assert_eq!(
            compute_merge_state(&r, &RealGit, &wt, "feat", None),
            Some(MergeState::Merged {
                into: Some("main".into())
            })
        );
    }

    #[test]
    fn bare_repo_lists_no_hub_row() {
        let repo = TestRepo::init_bare();
        let r = Repo::discover(repo.root()).unwrap();
        let worktrees = build_worktrees(&r, &RealGit).unwrap();
        assert!(worktrees.is_empty());
    }

    #[test]
    fn build_rows_includes_branchless_branches_with_ahead_behind() {
        let repo = TestRepo::init();
        // `feat` is one commit ahead of `main` (the default branch) and has no
        // worktree (issue #47).
        divergent_branch(&repo, "feat");
        let r = Repo::discover(repo.root()).unwrap();
        let rows = build_rows(&r, &RealGit).unwrap();

        let main = rows.iter().find(|w| w.is_main).unwrap();
        assert!(main.has_worktree);
        let feat = rows
            .iter()
            .find(|w| w.branch.as_deref() == Some("feat"))
            .unwrap();
        assert!(!feat.has_worktree);
        assert_eq!(feat.slug.as_deref(), Some("feat"));
        // Ahead/behind is measured against the base (here the default branch).
        assert_eq!(feat.ahead, Some(1));
        assert_eq!(feat.behind, Some(0));
        // The tip commit loads for the detail pane.
        assert_eq!(feat.commit.as_ref().unwrap().subject, "diverge");
    }

    #[test]
    fn build_rows_excludes_checked_out_branches() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let r = Repo::discover(repo.root()).unwrap();
        let rows = build_rows(&r, &RealGit).unwrap();
        // `feature/x` has a worktree, so it appears once — as a worktree row, never
        // also as a branch row.
        let matches: Vec<_> = rows
            .iter()
            .filter(|w| w.branch.as_deref() == Some("feature/x"))
            .collect();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].has_worktree);
    }

    #[test]
    fn branch_row_ahead_behind_uses_recorded_base_not_default() {
        // main advances past the fork point; `topic` forks from `develop` and adds
        // a commit. Measured against the recorded base (develop) topic is 1/0;
        // against the default branch (main) it would be 1/1 — so the counts prove
        // the recorded base is honored (issue #47).
        let repo = TestRepo::init(); // main @ c0
        repo.git(&["branch", "develop"]); // develop @ c0
        repo.write("m.txt", "1\n");
        repo.commit_all("c1"); // main @ c1
        repo.git(&["checkout", "-q", "-b", "topic", "develop"]); // topic @ c0
        repo.write("t.txt", "x\n");
        repo.commit_all("t1"); // topic @ t1 (parent c0)
        repo.git(&["checkout", "-q", "main"]);
        wtconfig::write_base_ref(&RealGit, repo.root(), "topic", "develop").unwrap();

        let r = Repo::discover(repo.root()).unwrap();
        let rows = build_rows(&r, &RealGit).unwrap();
        let topic = rows
            .iter()
            .find(|w| w.branch.as_deref() == Some("topic"))
            .unwrap();
        assert_eq!(topic.base_ref.as_deref(), Some("develop"));
        assert_eq!(topic.ahead, Some(1));
        assert_eq!(topic.behind, Some(0));
    }

    #[test]
    fn enumerate_rows_adds_unenriched_branch_rows() {
        let repo = TestRepo::init();
        repo.git(&["branch", "lonely"]);
        let r = Repo::discover(repo.root()).unwrap();
        let rows = enumerate_rows(&r, &RealGit).unwrap();
        let lonely = rows
            .iter()
            .find(|w| w.branch.as_deref() == Some("lonely"))
            .unwrap();
        assert!(!lonely.has_worktree);
        // The synchronous pass carries name + slug only; ahead/behind and the tip
        // commit load later (the row shows spinners until then).
        assert_eq!(lonely.slug.as_deref(), Some("lonely"));
        assert!(lonely.ahead.is_none());
        assert!(lonely.commit.is_none());
    }

    #[test]
    fn sort_base_first_groups_branch_rows_below_worktrees() {
        use crate::model::{SortKey, SortSpec};
        let mut main = wt_named("main");
        main.is_main = true;
        let zebra = wt_named("zebra"); // a real worktree
        let mut br_a = wt_named("aaa-branch");
        br_a.has_worktree = false;
        let mut br_z = wt_named("zzz-branch");
        br_z.has_worktree = false;
        let mut rows = vec![br_z, zebra, br_a, main];
        sort_worktrees_base_first(
            &mut rows,
            SortSpec {
                key: SortKey::Branch,
                descending: false,
            },
        );
        // Base first, then the other worktree, then the branch rows (sorted) last.
        assert_eq!(
            branches(&rows),
            vec!["main", "zebra", "aaa-branch", "zzz-branch"]
        );
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

    #[test]
    fn base_first_pins_primary_regardless_of_sort_direction() {
        use crate::model::{SortKey, SortSpec};
        let mut base = wt_named("main");
        base.is_main = true;
        // The base sits in the middle of the input and would sort to the
        // middle by branch name; it must still come out first.
        let mut worktrees = vec![wt_named("zebra"), base, wt_named("alpha")];
        sort_worktrees_base_first(
            &mut worktrees,
            SortSpec {
                key: SortKey::Branch,
                descending: false,
            },
        );
        // Base first, then the rest in ascending order.
        assert_eq!(branches(&worktrees), vec!["main", "alpha", "zebra"]);
        sort_worktrees_base_first(
            &mut worktrees,
            SortSpec {
                key: SortKey::Branch,
                descending: true,
            },
        );
        // Base still first, the rest now descending.
        assert_eq!(branches(&worktrees), vec!["main", "zebra", "alpha"]);
    }

    #[test]
    fn base_first_is_plain_sort_when_no_primary() {
        use crate::model::{SortKey, SortSpec};
        let spec = SortSpec {
            key: SortKey::Branch,
            descending: false,
        };
        let mut pinned = vec![wt_named("zebra"), wt_named("alpha"), wt_named("mango")];
        let mut plain = pinned.clone();
        sort_worktrees_base_first(&mut pinned, spec);
        sort_worktrees(&mut plain, spec);
        // With no `is_main` worktree, base-first is identical to a plain sort.
        assert_eq!(branches(&pinned), branches(&plain));
        assert_eq!(branches(&pinned), vec!["alpha", "mango", "zebra"]);
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
