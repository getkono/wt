//! Candidate classification for `wt prune`: why each worktree and bare branch
//! qualifies for removal ([`Reason`]), and what — if anything — keeps it
//! ([`Block`]).
//!
//! Work counts as kept when it survives the removal, by SHA or by content: a
//! commit reachable from a remote-tracking ref or the default branch, or a change
//! already in the default branch under another SHA (a squash or rebase merge).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::cli::PruneArgs;
use crate::error::Result;
use crate::gh::{GhClient, MergedPr, remote_slug};
use crate::git::aheadbehind::recoverable_tips;
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::merged::ContentCheck;
use crate::git::porcelain::RawWorktree;
use crate::git::worktrees::{admin_dir, in_progress_op_in};
use crate::git::{
    branch_ref, default_branch, default_tracking_ref, is_ancestor, is_clean_for_removal,
    local_branches, resolve_hex, upstream_of,
};
use crate::model::Worktree;

/// The refs work counts as merged into: the local default branch and, when
/// `origin/HEAD` is set, its remote-tracking ref — so work merged on the remote
/// is seen even while the local default lags behind.
pub(super) struct MergeTargets {
    /// The default branch's short name (never itself a prune candidate).
    pub(super) default: Option<String>,
    /// Full refs to test against, each known to resolve.
    pub(super) refs: Vec<String>,
}

impl MergeTargets {
    /// A target at the same commit as an earlier one is dropped: every check
    /// against it would repeat the earlier one's answer. The local default
    /// comes first, so it is the one kept.
    pub(super) fn resolve(repo: &Repo) -> Self {
        let default = default_branch(repo.gix());
        let mut seen: HashSet<String> = HashSet::new();
        let refs = default
            .as_deref()
            .map(branch_ref)
            .into_iter()
            .chain(default_tracking_ref(repo.gix()))
            .filter(|r| resolve_hex(repo.gix(), r).is_some_and(|id| seen.insert(id)))
            .collect();
        MergeTargets { default, refs }
    }

    /// Whether `branch` is the default branch, which is never pruned (it is
    /// trivially merged into itself).
    pub(super) fn is_default(&self, branch: &str) -> bool {
        self.default.as_deref() == Some(branch)
    }

    /// Whether `tip` (a ref or commit id) is an ancestor of any target.
    fn has_ancestor_merge(&self, repo: &Repo, tip: &str) -> bool {
        self.refs
            .iter()
            .any(|target| is_ancestor(repo.gix(), tip, target))
    }

    /// Whether every change on `tip` is already in some target under other SHAs
    /// (see [`ContentCheck`]).
    fn has_content_merge(&self, content: &ContentCheck<'_>, tip: &str) -> bool {
        self.refs
            .iter()
            .any(|target| content.is_merged(tip, target))
    }

    /// Whether `tip` is merged into some target, by ancestry or by content.
    pub(super) fn is_merged(&self, content: &ContentCheck<'_>, repo: &Repo, tip: &str) -> bool {
        self.has_ancestor_merge(repo, tip) || self.has_content_merge(content, tip)
    }

    /// The local default branch ref, if it resolves — the one non-remote ref
    /// whose commits count as kept by the recoverability check.
    fn local_default_ref(&self) -> Option<&str> {
        let local = branch_ref(self.default.as_deref()?);
        self.refs.iter().find(|r| **r == local).map(String::as_str)
    }
}

/// Why a subject qualifies for removal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reason {
    /// An ancestor of the default branch.
    Merged,
    /// Not an ancestor, but every change is already in the default branch
    /// (a squash or rebase merge, or work split across several merges).
    MergedByContent,
    /// An older prefix of the head of the merged pull request with this
    /// number, whose head is itself merged into the default branch (see
    /// [`PrProof`]).
    MergedViaPr(u64),
    /// Its branch's upstream was configured and is now gone.
    UpstreamGone,
    /// A worktree whose directory is gone.
    Missing,
    /// Every commit is on a remote or the default branch.
    Pushed,
}

impl Reason {
    pub(super) fn label(self) -> String {
        match self {
            Reason::Merged => "merged".into(),
            Reason::MergedByContent => "merged by content".into(),
            Reason::MergedViaPr(number) => format!("merged via #{number}"),
            Reason::UpstreamGone => "upstream gone".into(),
            Reason::Missing => "missing".into(),
            Reason::Pushed => "pushed".into(),
        }
    }
}

/// A merged pull request that vouches for a branch no other check could: the
/// branch's tip is an ancestor of `head`, the commit the pull request's head
/// pointed at when it merged, and `head` is itself merged into the default
/// branch. Every commit on the tip is then reachable from `head`, whose work
/// is in the default branch; `refs/pull/<number>/head` keeps it recoverable
/// after the branch is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrProof {
    /// The pull request's number.
    pub(super) number: u64,
    /// Its head commit, fetched from `refs/pull/<number>/head` and equal to
    /// the head the forge reported.
    pub(super) head: String,
}

/// What keeps a qualifying subject, after the flags that override it
/// (`--force`, `--locked`) have been applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Block {
    /// The worktree the command runs in. Never overridden.
    Current,
    /// A rebase, merge, cherry-pick, revert, or bisect is mid-flight. Never
    /// overridden.
    InProgress(&'static str),
    /// The worktree's git state could not be read, so it fails safe. Never
    /// overridden.
    Unreadable,
    /// A bare branch while the named worktrees' rebase or bisect state cannot
    /// be read, so it may be the branch one of them returns to. Never
    /// overridden.
    HolderUnreadable(String),
    /// A bare branch an in-progress rebase or bisect will return to, with what
    /// is happening to it and the worktree doing it. Never overridden.
    Held {
        activity: &'static str,
        worktree: String,
    },
    /// Locked, with its reason if one was given. Overridden by `--locked`.
    Locked(Option<String>),
    /// Uncommitted changes. Overridden by `--force`.
    Dirty,
    /// A branch holding work found on no remote whose changes could not be
    /// confirmed in the default branch. The content check is one-sided — a
    /// miss means unproven, not unmerged — so the message says only that.
    /// Overridden by `--force`.
    Unsafe,
    /// A bare branch that moved after its safety was re-checked under the
    /// lock, so the check vouches for a commit it no longer names. Only arises
    /// when there was a check, so without `--force`.
    Moved,
}

impl Block {
    /// The explanation printed after `skipping <label>: `.
    pub(super) fn message(&self) -> String {
        match self {
            Block::Current => "it is the current worktree".into(),
            Block::InProgress(op) => format!("{op} in progress"),
            Block::Unreadable => "cannot read its git state".into(),
            Block::HolderUnreadable(worktrees) => {
                format!("cannot read the rebase or bisect state of {worktrees}")
            }
            Block::Held { activity, worktree } => format!("{activity} in {worktree}"),
            Block::Locked(Some(reason)) => format!("locked ({reason}); use --locked"),
            Block::Locked(None) => "locked; use --locked".into(),
            Block::Dirty => "uncommitted changes; use --force".into(),
            Block::Unsafe => "has work on no remote and could not confirm its changes are in \
                              the default branch; use --force"
                .into(),
            Block::Moved => "moved since it was checked; run prune again".into(),
        }
    }
}

/// What a verdict is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Subject {
    /// An existing worktree, by its index in the worktree list. `locked` records
    /// whether its removal must override a lock (only reachable with `--locked`).
    /// `head` is a detached worktree's HEAD as assessed, so the removal can
    /// refuse a worktree that committed since; `None` for a branch worktree,
    /// whose commits survive on the branch.
    Worktree {
        index: usize,
        locked: bool,
        head: Option<String>,
    },
    /// A local branch with no worktree (or whose worktree is being removed under
    /// `--all`). `merged` records whether it is merged into the default branch by
    /// ancestry, content, or a merged pull request; `safe` whether deleting it
    /// loses no work. `via` is the pull request that proved it merged when
    /// nothing else did, so the removal can re-check that proof under the lock.
    Branch {
        name: String,
        merged: bool,
        safe: bool,
        via: Option<PrProof>,
    },
}

/// A subject that qualified for removal, with the reason and any block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Verdict {
    pub(super) subject: Subject,
    pub(super) reason: Reason,
    /// `None` when the subject will be removed; otherwise why it is skipped.
    pub(super) block: Option<Block>,
}

/// Classifies worktrees and branches for one prune run.
///
/// What every subject's checks share is read once: the content check's
/// merge-driver config and target trees, each commit's recoverability (see
/// [`Assessor::prefetch_recoverable`]), and each worktree's admin directory.
/// The caches hold for selection only; the re-checks under the repo lock read
/// fresh.
pub(super) struct Assessor<'a> {
    pub(super) git: &'a dyn GitCli,
    /// Asked only for a branch every local check refused (see
    /// [`Assessor::merged_via_pr`]).
    gh: &'a dyn GhClient,
    pub(super) root: &'a Path,
    pub(super) repo: &'a Repo,
    pub(super) args: &'a PruneArgs,
    pub(super) targets: &'a MergeTargets,
    pub(super) content: ContentCheck<'a>,
    /// Recoverability by commit id.
    recoverable: RefCell<HashMap<String, bool>>,
    /// Admin directory by worktree path.
    admin_dirs: RefCell<HashMap<PathBuf, PathBuf>>,
}

impl<'a> Assessor<'a> {
    pub(super) fn new(
        git: &'a dyn GitCli,
        gh: &'a dyn GhClient,
        root: &'a Path,
        repo: &'a Repo,
        args: &'a PruneArgs,
        targets: &'a MergeTargets,
    ) -> Self {
        Assessor {
            git,
            gh,
            root,
            repo,
            args,
            targets,
            content: ContentCheck::new(git, root),
            recoverable: RefCell::new(HashMap::new()),
            admin_dirs: RefCell::new(HashMap::new()),
        }
    }

    /// Answers the recoverability of every tip in `tips` (refs or commit ids)
    /// in one batched check, so the per-subject [`Self::recoverable`] calls
    /// that follow spawn nothing. A tip that does not resolve is left out (it
    /// reads as unrecoverable).
    pub(super) fn prefetch_recoverable<'t>(&self, tips: impl IntoIterator<Item = &'t str>) {
        let ids: Vec<String> = tips
            .into_iter()
            .filter_map(|tip| self.commit_of(tip))
            .filter(|id| !self.recoverable.borrow().contains_key(id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let answers = self.recoverable_now(&ids);
        let mut cache = self.recoverable.borrow_mut();
        for id in ids {
            let recoverable = answers.contains(&id);
            cache.insert(id, recoverable);
        }
    }

    /// The subset of `ids` (commit ids) that is recoverable, read now, keeping
    /// the local default branch. When the batch fails, each id is checked on
    /// its own, so one bad commit costs only its own answer; an id whose check
    /// fails is unrecoverable, so a git error can never be the reason work is
    /// deleted.
    pub(super) fn recoverable_now(&self, ids: &[String]) -> HashSet<String> {
        if ids.is_empty() {
            return HashSet::new();
        }
        let keep: Vec<&str> = self.targets.local_default_ref().into_iter().collect();
        let tips: Vec<&str> = ids.iter().map(String::as_str).collect();
        recoverable_tips(self.git, self.root, &tips, &keep).unwrap_or_else(|e| {
            tracing::warn!(tips = tips.len(), error = %e, "prune: batched recoverability check failed; checking each");
            tips.iter()
                .filter(|tip| {
                    recoverable_tips(self.git, self.root, &[tip], &keep)
                        .inspect_err(|e| tracing::warn!(tip, error = %e, "prune: recoverability check failed"))
                        .is_ok_and(|recoverable| recoverable.contains(**tip))
                })
                .map(|tip| tip.to_string())
                .collect()
        })
    }

    /// The commit `tip` (a ref or commit id) peels to: a branch may point at
    /// an annotated tag, whose own id no reachability check ever prints.
    fn commit_of(&self, tip: &str) -> Option<String> {
        resolve_hex(self.repo.gix(), &format!("{tip}^{{commit}}"))
    }

    /// The admin directory of `worktree`, resolved once per run.
    pub(super) fn admin_dir(&self, worktree: &Worktree) -> Result<PathBuf> {
        if let Some(dir) = self.admin_dirs.borrow().get(&worktree.path) {
            return Ok(dir.clone());
        }
        let dir = admin_dir(self.git, self.root, &worktree.path, worktree.is_missing)?;
        self.admin_dirs
            .borrow_mut()
            .insert(worktree.path.clone(), dir.clone());
        Ok(dir)
    }

    /// The verdict for one worktree, or `None` when no selected mode picks it.
    /// The primary worktree is never a candidate. `raw` is the worktree's
    /// porcelain record, which carries the lock and a detached HEAD's commit.
    pub(super) fn worktree(
        &self,
        index: usize,
        worktree: &Worktree,
        raw: Option<&RawWorktree>,
    ) -> Option<Verdict> {
        if worktree.is_main {
            return None;
        }
        let tip = match &worktree.branch {
            Some(branch) => Some(branch_ref(branch)),
            None => raw.and_then(|r| r.head.clone()),
        };
        let reason = self.worktree_reason(worktree, tip.as_deref())?;
        let locked = raw.is_some_and(|r| r.is_locked);
        let block = self
            .worktree_block(worktree, raw)
            .or_else(|| self.detached_unsafe(worktree, tip.as_deref()));
        tracing::trace!(target_wt = %worktree.path.display(), ?reason, ?block, "prune: worktree classified");
        let head = if worktree.branch.is_none() { tip } else { None };
        Some(Verdict {
            subject: Subject::Worktree {
                index,
                locked,
                head,
            },
            reason,
            block,
        })
    }

    /// [`Block::Unsafe`] for a detached worktree whose HEAD is on no remote and
    /// not in the default branch (without `--force`). Only its own HEAD holds
    /// those commits, so removing it — even a missing one, whose admin entry is
    /// that HEAD — orphans them. A branch worktree's commits survive on the
    /// branch, so it never gets this block.
    fn detached_unsafe(&self, worktree: &Worktree, tip: Option<&str>) -> Option<Block> {
        if self.args.force || worktree.branch.is_some() {
            return None;
        }
        let safe = tip.is_some_and(|tip| {
            self.recoverable(tip) || self.targets.is_merged(&self.content, self.repo, tip)
        });
        (!safe).then_some(Block::Unsafe)
    }

    /// The first selected mode that picks `worktree`, cheapest checks first.
    /// `tip` is its branch ref or detached commit.
    fn worktree_reason(&self, worktree: &Worktree, tip: Option<&str>) -> Option<Reason> {
        let args = self.args;
        // A worktree on the default branch is never "merged" into itself.
        let mergeable = tip.filter(|_| {
            !worktree
                .branch
                .as_deref()
                .is_some_and(|b| self.targets.is_default(b))
        });
        if args.includes_merged()
            && let Some(tip) = mergeable
            && self.targets.has_ancestor_merge(self.repo, tip)
        {
            return Some(Reason::Merged);
        }
        if args.includes_gone() {
            if worktree.is_missing {
                return Some(Reason::Missing);
            }
            if self.upstream_gone(worktree.branch.as_deref()) {
                return Some(Reason::UpstreamGone);
            }
        }
        if args.includes_merged()
            && let Some(tip) = mergeable
            && self.targets.has_content_merge(&self.content, tip)
        {
            return Some(Reason::MergedByContent);
        }
        // A checked-out branch is active work even when pushed; a detached
        // worktree has no branch to keep, so it is the only holder of its HEAD.
        if args.includes_pushed()
            && worktree.is_detached
            && let Some(tip) = tip
            && self.recoverable(tip)
        {
            return Some(Reason::Pushed);
        }
        None
    }

    /// What keeps a qualifying worktree, most fundamental first, skipping any
    /// block a flag overrides.
    fn worktree_block(&self, worktree: &Worktree, raw: Option<&RawWorktree>) -> Option<Block> {
        if worktree.is_current {
            return Some(Block::Current);
        }
        // Without its porcelain record the lock and a detached HEAD are unknown.
        let Some(raw) = raw else {
            tracing::warn!(target_wt = %worktree.path.display(), "prune: no worktree record");
            return Some(Block::Unreadable);
        };
        // A missing worktree can be mid-operation too (its admin entry still
        // holds the state), and is kept like a present one.
        match self.admin_dir(worktree).map(|dir| in_progress_op_in(&dir)) {
            Ok(Some(op)) => return Some(Block::InProgress(op)),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(target_wt = %worktree.path.display(), %error, "prune: cannot read worktree state");
                return Some(Block::Unreadable);
            }
        }
        if !self.args.locked && raw.is_locked {
            return Some(Block::Locked(raw.lock_reason.clone()));
        }
        // Untracked files always count, whatever `remove.untracked_blocks` or
        // the repository's status config says: removal passes `--force`
        // (submodules and locks need it), so git would otherwise delete them —
        // and a worktree prune selects on its own is not one the user named.
        // A missing worktree has nothing on disk to lose.
        if !self.args.force
            && !worktree.is_missing
            && !is_clean_for_removal(self.git, &worktree.path)
        {
            return Some(Block::Dirty);
        }
        None
    }

    /// Verdicts for local branches that qualify: every local branch except the
    /// default, the `current` one, and those in `excluded` (branches that keep a
    /// worktree, which the worktree path handles).
    pub(super) fn branches(
        &self,
        current: Option<&str>,
        excluded: &HashSet<String>,
    ) -> Result<Vec<Verdict>> {
        let mut out = Vec::new();
        for name in local_branches(self.repo.gix())? {
            if excluded.contains(&name)
                || self.targets.is_default(&name)
                || current == Some(name.as_str())
            {
                continue;
            }
            if let Some(verdict) = self.branch(name) {
                out.push(verdict);
            }
        }
        Ok(out)
    }

    /// The verdict for one local branch, or `None` when no selected mode picks
    /// it. Subprocess checks run only when some mode could still pick it.
    fn branch(&self, name: String) -> Option<Verdict> {
        let args = self.args;
        let tip = branch_ref(&name);
        let ancestor = self.targets.has_ancestor_merge(self.repo, &tip);
        let gone = self.upstream_gone(Some(&name));
        let content = !ancestor
            && args.includes_merged()
            && self.targets.has_content_merge(&self.content, &tip);
        let by_mode = if args.includes_merged() && ancestor {
            Some(Reason::Merged)
        } else if content {
            Some(Reason::MergedByContent)
        } else if args.includes_gone() && gone {
            Some(Reason::UpstreamGone)
        } else {
            None
        };
        // Under `--merged` a gone branch no check has picked yet may still be
        // proven merged through its pull request, below.
        if by_mode.is_none() && !args.includes_pushed() && !(args.includes_merged() && gone) {
            return None;
        }
        let recoverable = self.recoverable(&tip);
        // The content check above only runs under `--merged`; for a branch picked
        // otherwise, run it now, so `merged` (reported in `--json`) and safety do
        // not depend on which flags were passed.
        let merged_locally = ancestor
            || content
            || (!args.includes_merged() && self.targets.has_content_merge(&self.content, &tip));
        // The pull-request fallback runs only for a branch every check above
        // refused — merged by neither ancestry nor content, and with work on
        // no remote — whose upstream is gone (the shape a merge that deleted
        // the remote branch leaves; a branch never pushed has no pull request
        // to ask about), and that some selected mode would take once proven.
        let via = (gone
            && !merged_locally
            && !recoverable
            && (args.includes_merged() || by_mode.is_some()))
        .then(|| self.merged_via_pr(&name, &tip))
        .flatten();
        let by_mode = match &via {
            Some(proof) if args.includes_merged() => Some(Reason::MergedViaPr(proof.number)),
            _ => by_mode,
        };
        let reason = by_mode
            .or_else(|| (args.includes_pushed() && recoverable).then_some(Reason::Pushed))?;
        let merged = merged_locally || via.is_some();
        let safe = recoverable || merged;
        let block = (!safe && !args.force).then_some(Block::Unsafe);
        tracing::trace!(branch = %name, ?reason, ancestor, content, gone, recoverable, via = ?via.as_ref().map(|p| p.number), safe, "prune: branch classified");
        Some(Verdict {
            subject: Subject::Branch {
                name,
                merged,
                safe,
                via,
            },
            reason,
            block,
        })
    }

    /// The merged pull request that proves `name` (whose tip is `tip`) merged,
    /// or `None` — the refusal stands — when there is none, or when it cannot
    /// be asked: `--no-fetch`, no default branch, no hosted remote the branch's
    /// upstream was on, or `gh` missing, unauthenticated, or offline. Unknown
    /// is never read as merged.
    ///
    /// The forge only names candidates: merged pull requests whose head is
    /// this branch on the repository its upstream was on (so a fork's same-named
    /// branch never matches) and whose base is the default branch. Each one's
    /// head is fetched from `refs/pull/<n>/head` and must equal the head the
    /// forge reported; the rest is proven locally (see
    /// [`Self::pr_head_holds`]).
    pub(super) fn merged_via_pr(&self, name: &str, tip: &str) -> Option<PrProof> {
        if self.args.no_fetch {
            tracing::debug!(branch = %name, "prune: --no-fetch; merged-PR lookup skipped");
            return None;
        }
        let default = self.targets.default.as_deref()?;
        let tip = self.commit_of(tip)?;
        let Some((remote, (owner, repo_name))) = self.upstream_remote(name) else {
            tracing::debug!(branch = %name, "prune: no hosted remote; merged-PR lookup skipped");
            return None;
        };
        let prs = match self.gh.merged_prs_for_branch(self.root, name) {
            Ok(prs) => prs,
            Err(error) => {
                tracing::debug!(branch = %name, %error, "prune: merged-PR lookup failed");
                return None;
            }
        };
        prs.iter()
            .filter(|pr| {
                let matches = pr.state.eq_ignore_ascii_case("merged")
                    && pr.head_ref_name == name
                    && pr.base_ref_name == default
                    && !pr.is_cross_repository
                    && pr.head_slug().is_some_and(|(o, n)| {
                        o.eq_ignore_ascii_case(&owner) && n.eq_ignore_ascii_case(&repo_name)
                    });
                if !matches {
                    tracing::debug!(branch = %name, pr = pr.number, "prune: merged PR filtered out");
                }
                matches
            })
            .find_map(|pr| {
                let head = self.pin_pr_head(&remote, pr)?;
                let holds = self.pr_head_holds(&self.content, &tip, &head);
                tracing::debug!(branch = %name, pr = pr.number, %head, holds, "prune: merged PR checked");
                holds.then_some(PrProof {
                    number: pr.number,
                    head,
                })
            })
    }

    /// The remote `name`'s upstream was on (`branch.<name>.remote`), with the
    /// `(owner, name)` its URL names; `None` when there is no such remote or
    /// its URL is not a hosted one.
    fn upstream_remote(&self, name: &str) -> Option<(String, (String, String))> {
        let config = self.repo.gix().config_snapshot();
        let read = |key: String| config.string(key.as_str()).map(|v| v.to_string());
        let remote = read(format!("branch.{name}.remote"))?;
        let url = read(format!("remote.{remote}.url"))?;
        let slug = remote_slug(&url)?;
        Some((remote, slug))
    }

    /// Fetches `refs/pull/<n>/head` from `remote` and returns the commit it
    /// names, when that is the head the forge reported for `pr`. A failed
    /// fetch, or a head that moved or differs, reads as `None`.
    fn pin_pr_head(&self, remote: &str, pr: &MergedPr) -> Option<String> {
        let refspec = format!("refs/pull/{}/head", pr.number);
        if let Err(error) = self.git.run(
            self.root,
            &["fetch", "--quiet", "--no-tags", remote, &refspec],
        ) {
            tracing::debug!(pr = pr.number, %error, "prune: cannot fetch the PR head");
            return None;
        }
        let fetched = self
            .git
            .run(
                self.root,
                &["rev-parse", "--verify", "--quiet", "FETCH_HEAD^{commit}"],
            )
            .ok()?;
        let fetched = fetched.trim();
        if pr.head_ref_oid.is_empty() || !fetched.eq_ignore_ascii_case(&pr.head_ref_oid) {
            tracing::debug!(pr = pr.number, fetched, reported = %pr.head_ref_oid, "prune: PR head differs from the reported one");
            return None;
        }
        Some(fetched.to_string())
    }

    /// Whether `head` vouches for `tip` (commit ids), proven from local objects
    /// alone: `tip` is an ancestor of `head`, `head` is merged into a default
    /// target by ancestry or content ([`MergeTargets::is_merged`]), and `tip`'s
    /// net diff is not empty. Any failure reads as `false`.
    pub(super) fn pr_head_holds(&self, content: &ContentCheck<'_>, tip: &str, head: &str) -> bool {
        let ancestor = self
            .git
            .run_raw(self.root, &["merge-base", "--is-ancestor", tip, head])
            .is_ok_and(|out| out.success);
        ancestor && self.targets.is_merged(content, self.repo, head) && self.changes_something(tip)
    }

    /// Whether `tip`'s tree differs from its merge base's with every target —
    /// a branch whose net effect is empty may hold work no merge carried, so
    /// it is never vouched for. Any failure reads as `false`.
    fn changes_something(&self, tip: &str) -> bool {
        !self.targets.refs.is_empty()
            && self.targets.refs.iter().all(|target| {
                let Ok(base) = self.git.run(self.root, &["merge-base", target, tip]) else {
                    return false;
                };
                let base = base.trim();
                self.git
                    .run(
                        self.root,
                        &[
                            "rev-parse",
                            &format!("{base}^{{tree}}"),
                            &format!("{tip}^{{tree}}"),
                        ],
                    )
                    .is_ok_and(|trees| {
                        matches!(trees.split_whitespace().collect::<Vec<_>>()[..],
                                 [base_tree, tip_tree] if base_tree != tip_tree)
                    })
            })
    }

    /// Whether `branch`'s upstream is configured but its tracking ref is gone.
    fn upstream_gone(&self, branch: Option<&str>) -> bool {
        branch
            .and_then(|b| upstream_of(self.repo.gix(), b))
            .is_some_and(|u| u.is_gone)
    }

    /// Whether every commit on `tip` is on a remote or the local default
    /// branch, from the selection cache (checked now when not prefetched). A
    /// tip that does not resolve, or a failed check, reads as unrecoverable so
    /// a git error can never be the reason work is deleted.
    pub(super) fn recoverable(&self, tip: &str) -> bool {
        let Some(id) = self.commit_of(tip) else {
            tracing::warn!(tip, "prune: recoverability check on an unknown tip");
            return false;
        };
        self.prefetch_recoverable([id.as_str()]);
        self.recoverable.borrow().get(&id).copied().unwrap_or(false)
    }
}
