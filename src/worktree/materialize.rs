//! Copy-on-write materialization of a new worktree from an existing one.
//!
//! The default path pays for a full checkout: `git worktree add` writes every
//! tracked file, and submodules are populated separately. On a large
//! superproject that is gigabytes of writes for content that already exists,
//! byte for byte, in a sibling worktree.
//!
//! On a CoW filesystem those bytes can be shared instead. This module drives:
//!
//! 1. `git worktree add --no-checkout` — administrative files, no content.
//! 2. `read-tree HEAD` — the index, which must exist before anything else.
//! 3. A reflink clone of the source worktree's files.
//! 4. `update-index --refresh` to adopt them, then `checkout -- .` to correct
//!    any file that does not match the index (a dirty source file, say). Files
//!    that already match are skipped, so this writes almost nothing.
//! 5. [`reattach`](crate::git::submodule::reattach) for each submodule, giving
//!    the copied files a git directory of their own.
//!
//! # Why the trees must match
//!
//! The copied files are the *source's* content. If the source's tree differed
//! from the new worktree's, step 4 would have to rewrite every differing file,
//! and any file tracked only in the source would linger as a stray. Rather than
//! reconcile that, this path simply declines unless the two trees are identical
//! — which is the common case it exists for, branching from the same base as a
//! worktree you already have. Anything else falls back to a normal checkout.
//!
//! # It cannot change the outcome
//!
//! Every failure path falls back to materializing the tree with git
//! (`checkout-index -a -f`), and submodules still go through the usual
//! `sync` + `update --init --recursive` reconcile afterwards. The worst case is
//! that this was a waste of time, never that the worktree is wrong.

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::error::Result;
use crate::git::cli::GitCli;
use crate::git::submodule;
use crate::util::reflink;

/// A source worktree that can be copy-on-write cloned into a new one.
#[derive(Debug, Clone)]
pub(crate) struct ReflinkPlan {
    /// The worktree whose files will be cloned.
    pub(crate) source: PathBuf,
}

/// Decides whether `target` can be materialized by cloning `source`, given the
/// commit the new worktree will be checked out at.
///
/// `None` means "use the normal checkout", and every `None` is a legitimate
/// answer rather than an error: no source, trees differ, or the filesystem has
/// no reflink support.
pub(crate) fn plan(
    git: &dyn GitCli,
    source: Option<&Path>,
    target_parent: &Path,
    start_commit: &str,
) -> Option<ReflinkPlan> {
    let source = source?;
    if !source.is_dir() {
        return None;
    }
    if !reflink::is_supported(target_parent) {
        debug!("no reflink support at the target; using a normal checkout");
        return None;
    }
    let source_tree = tree_of(git, source, "HEAD")?;
    let target_tree = tree_of(git, source, start_commit)?;
    if source_tree != target_tree {
        debug!(
            source = %source.display(),
            "source worktree is at a different tree; using a normal checkout"
        );
        return None;
    }
    Some(ReflinkPlan {
        source: source.to_path_buf(),
    })
}

/// Resolves `rev`'s tree object id, as seen from `dir`.
fn tree_of(git: &dyn GitCli, dir: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{tree}}");
    git.run(dir, &["rev-parse", &spec])
        .ok()
        .map(|s| s.trim().to_string())
}

/// Materializes `target`'s content from the plan's source worktree.
///
/// Falls back to a stock checkout on any failure, so the caller always ends up
/// with a correctly populated worktree. Returns whether the CoW path was the one
/// that produced it.
pub(crate) fn apply(git: &dyn GitCli, target: &Path, plan: &ReflinkPlan) -> Result<bool> {
    // The index has to exist before anything reads it — `--no-checkout` leaves
    // it empty, and `submodule` commands then fail with "pathspec did not match
    // any file(s) known to git".
    git.run(target, &["read-tree", "HEAD"])?;

    if let Err(e) = clone_content(git, target, plan) {
        debug!(error = %e, "reflink materialization failed; checking out normally");
        checkout_everything(git, target)?;
        return Ok(false);
    }
    Ok(true)
}

/// The CoW half: clone the files in, adopt what matches, correct what does not.
fn clone_content(git: &dyn GitCli, target: &Path, plan: &ReflinkPlan) -> Result<()> {
    // `.git` is the worktree's own administrative link and must never be
    // overwritten with the source's.
    reflink::clone_tree(&plan.source, target, &[".git"])?;
    // Mark the files that already match the index as clean. A non-zero exit
    // just means some file differs, which the checkout below fixes.
    if let Err(e) = git.run(target, &["update-index", "--refresh"]) {
        debug!(error = %e, "index refresh found differences to correct");
    }
    // Restore anything that does not match the index — a file the source had
    // modified, most likely. Entries marked up to date above are skipped, so
    // this writes only what it must.
    git.run(target, &["checkout", "--", "."])?;
    Ok(())
}

/// Writes the whole tree out with git, the way `worktree add` would have.
fn checkout_everything(git: &dyn GitCli, target: &Path) -> Result<()> {
    git.run(target, &["checkout-index", "-a", "-f"])?;
    Ok(())
}

/// Gives each copied submodule directory a git directory of its own, so the
/// files the reflink brought over are recognized instead of re-cloned.
///
/// Best-effort throughout: whatever this does not manage is left to the caller's
/// `sync` + `update --init --recursive` reconcile pass. Returns the submodule
/// paths that were attached.
pub(crate) fn attach_submodules(git: &dyn GitCli, target: &Path, common_dir: &Path) -> Vec<String> {
    let Some(private_modules) = private_modules_dir(git, target) else {
        return Vec::new();
    };
    let mut attached = Vec::new();
    walk(
        git,
        target,
        &common_dir.join("modules"),
        &private_modules,
        "",
        &mut attached,
    );
    attached
}

/// One level of submodules, then into each one that was attached.
fn walk(
    git: &dyn GitCli,
    dir: &Path,
    mirror_prefix: &Path,
    private_modules: &Path,
    rel_prefix: &str,
    attached: &mut Vec<String>,
) {
    let Ok(declared) = submodule::declared(git, dir) else {
        return;
    };
    for sub in declared {
        let rel = if rel_prefix.is_empty() {
            sub.path.clone()
        } else {
            format!("{rel_prefix}/{}", sub.path)
        };
        let work = dir.join(&sub.path);
        // Only adopt a directory the copy actually populated.
        if !work.is_dir() || is_empty_dir(&work) {
            continue;
        }
        // The copy brought the *source's* `.git` gitlink along, and it points
        // into the source worktree's private modules tree — from here that path
        // does not exist, and git refuses to touch the directory at all
        // ("fatal: not a git repository"). A gitlink that still resolves belongs
        // to something else and is left alone.
        match gitlink_state(&work) {
            GitlinkState::Valid => continue,
            GitlinkState::Stale => {
                if let Err(e) = std::fs::remove_file(work.join(".git")) {
                    debug!(submodule = %rel, error = %e, "could not clear the stale gitlink");
                    continue;
                }
            }
            GitlinkState::Absent => {}
        }
        let mirror = mirror_prefix.join(&sub.name);
        if !mirror.is_dir() {
            continue;
        }
        let Ok(sha) = git.run(dir, &["rev-parse", &format!(":{}", sub.path)]) else {
            continue;
        };
        match submodule::reattach::attach(
            git,
            dir,
            &sub.path,
            &sub.name,
            &mirror,
            private_modules,
            sha.trim(),
        ) {
            Ok(()) => {
                debug!(submodule = %rel, "attached copied submodule");
                attached.push(rel.clone());
                walk(
                    git,
                    &work,
                    &mirror.join("modules"),
                    &private_modules.join(&sub.name).join("modules"),
                    &rel,
                    attached,
                );
            }
            Err(e) => debug!(submodule = %rel, error = %e, "could not attach; leaving it"),
        }
    }
}

/// What the `.git` entry of a copied submodule directory is.
#[derive(Debug, PartialEq, Eq)]
enum GitlinkState {
    /// No `.git` at all — the directory is ready to be attached.
    Absent,
    /// A `.git` that resolves to a real git directory; not ours to replace.
    Valid,
    /// A `.git` gitlink pointing at a path that does not exist, which is what a
    /// copy of another worktree's submodule always produces.
    Stale,
}

/// Classifies the `.git` entry inside a copied submodule working directory.
fn gitlink_state(work: &Path) -> GitlinkState {
    let dot_git = work.join(".git");
    if dot_git.is_dir() {
        // A whole repository was copied in; leave it be.
        return GitlinkState::Valid;
    }
    let Ok(text) = std::fs::read_to_string(&dot_git) else {
        return GitlinkState::Absent;
    };
    let Some(target) = text.trim().strip_prefix("gitdir:") else {
        // Not a gitlink at all; treat it as something we did not write.
        return GitlinkState::Valid;
    };
    // A gitlink path may be relative to the working directory that holds it.
    let target = Path::new(target.trim());
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        work.join(target)
    };
    if resolved.is_dir() {
        GitlinkState::Valid
    } else {
        GitlinkState::Stale
    }
}

/// Whether `dir` has no entries.
fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

/// The linked worktree's own `modules` directory, where git expects its
/// submodule git directories.
fn private_modules_dir(git: &dyn GitCli, worktree_dir: &Path) -> Option<PathBuf> {
    let out = git
        .run(
            worktree_dir,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        )
        .ok()?;
    Some(PathBuf::from(out.trim()).join("modules"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    /// Whether the repo's filesystem supports reflinks; the CoW assertions are
    /// meaningless without it.
    fn cow(repo: &TestRepo) -> bool {
        reflink::is_supported(repo.root())
    }

    #[test]
    fn plan_declines_without_a_source() {
        let repo = TestRepo::init();
        assert!(plan(&RealGit, None, repo.root(), "HEAD").is_none());
    }

    #[test]
    fn plan_declines_when_the_trees_differ() {
        let repo = TestRepo::init();
        let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("new.txt", "content\n");
        repo.commit_all("diverge");
        // The source worktree is now ahead of `base`, so its files are not the
        // content a worktree at `base` should have.
        assert!(plan(&RealGit, Some(repo.root()), repo.root(), &base).is_none());
    }

    #[test]
    fn plan_accepts_a_source_at_the_same_tree() {
        let repo = TestRepo::init();
        if !cow(&repo) {
            return;
        }
        let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        assert!(plan(&RealGit, Some(repo.root()), repo.root(), &head).is_some());
    }

    #[test]
    fn gitlink_state_distinguishes_stale_from_valid() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("sub");
        std::fs::create_dir_all(&work).unwrap();
        assert_eq!(gitlink_state(&work), GitlinkState::Absent);

        // A copied gitlink pointing at a path that does not exist here.
        std::fs::write(work.join(".git"), "gitdir: ../../.git/modules/sub\n").unwrap();
        assert_eq!(gitlink_state(&work), GitlinkState::Stale);

        // The same gitlink once its target exists.
        std::fs::create_dir_all(dir.path().join(".git/modules/sub")).unwrap();
        std::fs::write(
            work.join(".git"),
            format!(
                "gitdir: {}\n",
                dir.path().join(".git/modules/sub").display()
            ),
        )
        .unwrap();
        assert_eq!(gitlink_state(&work), GitlinkState::Valid);

        // A real embedded repository is never ours to replace.
        std::fs::remove_file(work.join(".git")).unwrap();
        std::fs::create_dir_all(work.join(".git")).unwrap();
        assert_eq!(gitlink_state(&work), GitlinkState::Valid);
    }

    #[test]
    fn is_empty_dir_reports_missing_and_empty_alike() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_empty_dir(dir.path()));
        assert!(is_empty_dir(&dir.path().join("absent")));
        std::fs::write(dir.path().join("f"), "x").unwrap();
        assert!(!is_empty_dir(dir.path()));
    }
}
