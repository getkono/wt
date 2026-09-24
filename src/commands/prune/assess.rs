//! Candidate classification for `wt prune`: why each worktree and bare branch
//! qualifies for removal ([`Reason`]), and what — if anything — keeps it
//! ([`Block`]).
//!
//! Work counts as kept when it survives the removal, by SHA or by content: a
//! commit reachable from a remote-tracking ref or the default branch, or a change
//! already in the default branch under another SHA (a squash or rebase merge).

use std::collections::HashSet;
use std::path::Path;

use crate::cli::PruneArgs;
use crate::error::Result;
use crate::git::aheadbehind::is_recoverable;
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::merged::is_content_merged;
use crate::git::porcelain::RawWorktree;
use crate::git::worktrees::in_progress_op;
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
    pub(super) fn resolve(repo: &Repo) -> Self {
        let default = default_branch(repo.gix());
        let refs = default
            .as_deref()
            .map(branch_ref)
            .into_iter()
            .chain(default_tracking_ref(repo.gix()))
            .filter(|r| resolve_hex(repo.gix(), r).is_some())
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
    /// (see [`is_content_merged`]).
    fn has_content_merge(&self, git: &dyn GitCli, root: &Path, tip: &str) -> bool {
        self.refs
            .iter()
            .any(|target| is_content_merged(git, root, tip, target))
    }

    /// Whether `tip` is merged into some target, by ancestry or by content.
    pub(super) fn is_merged(&self, git: &dyn GitCli, root: &Path, repo: &Repo, tip: &str) -> bool {
        self.has_ancestor_merge(repo, tip) || self.has_content_merge(git, root, tip)
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
    /// Its branch's upstream was configured and is now gone.
    UpstreamGone,
    /// A worktree whose directory is gone.
    Missing,
    /// Every commit is on a remote or the default branch.
    Pushed,
}

impl Reason {
    pub(super) fn label(self) -> &'static str {
        match self {
            Reason::Merged => "merged",
            Reason::MergedByContent => "merged by content",
            Reason::UpstreamGone => "upstream gone",
            Reason::Missing => "missing",
            Reason::Pushed => "pushed",
        }
    }
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
    /// Locked, with its reason if one was given. Overridden by `--locked`.
    Locked(Option<String>),
    /// Uncommitted changes. Overridden by `--force`.
    Dirty,
    /// A branch holding work found on no remote and not in the default branch.
    /// Overridden by `--force`.
    Unsafe,
}

impl Block {
    /// The explanation printed after `skipping <label>: `.
    pub(super) fn message(&self) -> String {
        match self {
            Block::Current => "it is the current worktree".into(),
            Block::InProgress(op) => format!("{op} in progress"),
            Block::Unreadable => "cannot read its git state".into(),
            Block::Locked(Some(reason)) => format!("locked ({reason}); use --locked"),
            Block::Locked(None) => "locked; use --locked".into(),
            Block::Dirty => "uncommitted changes; use --force".into(),
            Block::Unsafe => {
                "has work on no remote and not in the default branch; use --force".into()
            }
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
    /// ancestry or content; `safe` whether deleting it loses no work.
    Branch {
        name: String,
        merged: bool,
        safe: bool,
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
pub(super) struct Assessor<'a> {
    pub(super) git: &'a dyn GitCli,
    pub(super) root: &'a Path,
    pub(super) repo: &'a Repo,
    pub(super) args: &'a PruneArgs,
    pub(super) targets: &'a MergeTargets,
}

impl Assessor<'_> {
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
            self.recoverable(tip) || self.targets.is_merged(self.git, self.root, self.repo, tip)
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
            && self.targets.has_content_merge(self.git, self.root, tip)
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
        if !worktree.is_missing {
            match in_progress_op(self.git, &worktree.path) {
                Ok(Some(op)) => return Some(Block::InProgress(op)),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(target_wt = %worktree.path.display(), %error, "prune: cannot read worktree state");
                    return Some(Block::Unreadable);
                }
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
            && self.targets.has_content_merge(self.git, self.root, &tip);
        let by_mode = if args.includes_merged() && ancestor {
            Some(Reason::Merged)
        } else if content {
            Some(Reason::MergedByContent)
        } else if args.includes_gone() && gone {
            Some(Reason::UpstreamGone)
        } else {
            None
        };
        if by_mode.is_none() && !args.includes_pushed() {
            return None;
        }
        let recoverable = self.recoverable(&tip);
        let reason = by_mode.or_else(|| recoverable.then_some(Reason::Pushed))?;
        // The content check above only runs under `--merged`; for a branch picked
        // otherwise, run it now, so `merged` (reported in `--json`) and safety do
        // not depend on which flags were passed.
        let merged = ancestor
            || content
            || (!args.includes_merged()
                && self.targets.has_content_merge(self.git, self.root, &tip));
        let safe = recoverable || merged;
        let block = (!safe && !args.force).then_some(Block::Unsafe);
        tracing::trace!(branch = %name, ?reason, ancestor, content, gone, recoverable, safe, "prune: branch classified");
        Some(Verdict {
            subject: Subject::Branch { name, merged, safe },
            reason,
            block,
        })
    }

    /// Whether deleting local branch `name` loses no work — re-checked under the
    /// repo lock, since the branch may have moved since selection.
    pub(super) fn branch_is_safe(&self, name: &str) -> bool {
        let tip = branch_ref(name);
        self.recoverable(&tip) || self.targets.is_merged(self.git, self.root, self.repo, &tip)
    }

    /// Whether `branch`'s upstream is configured but its tracking ref is gone.
    fn upstream_gone(&self, branch: Option<&str>) -> bool {
        branch
            .and_then(|b| upstream_of(self.repo.gix(), b))
            .is_some_and(|u| u.is_gone)
    }

    /// [`is_recoverable`] for `tip`, keeping the local default branch, and
    /// treating a failed check as unrecoverable so a git error can never be the
    /// reason work is deleted.
    pub(super) fn recoverable(&self, tip: &str) -> bool {
        let keep: Vec<&str> = self.targets.local_default_ref().into_iter().collect();
        is_recoverable(self.git, self.root, tip, &keep).unwrap_or_else(|e| {
            tracing::warn!(tip, error = %e, "prune: recoverability check failed");
            false
        })
    }
}
