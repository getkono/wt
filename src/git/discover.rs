//! Repository discovery and identity via `gix` (spec §4 read operations):
//! discovery from a directory upward, bare detection, and the current worktree.
//!
//! Resolving the *primary* worktree root is done from `git worktree list`
//! output (see [`crate::git::worktrees::primary_root`]) rather than from `gix`,
//! because `gix`'s `common_dir()` does not resolve the linked-worktree
//! indirection reliably — a fallback the spec §4 explicitly sanctions.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// A discovered Git repository, wrapping the `gix` handle.
pub struct Repo {
    inner: gix::Repository,
}

impl Repo {
    /// Discovers the repository containing `start`, searching upward. Returns
    /// [`Error::NotInRepo`] when `start` is not inside a repository.
    pub fn discover(start: &Path) -> Result<Repo> {
        match gix::discover(start) {
            Ok(inner) => Ok(Repo { inner }),
            Err(_) => Err(Error::NotInRepo),
        }
    }

    /// Borrows the underlying `gix` repository for other read modules.
    pub fn gix(&self) -> &gix::Repository {
        &self.inner
    }

    /// Whether this is a bare repository (no working tree).
    pub fn is_bare(&self) -> bool {
        self.inner.workdir().is_none()
    }

    /// The working directory of the worktree `wt` was invoked from, or `None`
    /// for a bare repository.
    pub fn current_workdir(&self) -> Option<PathBuf> {
        self.inner.workdir().map(Path::to_path_buf)
    }

    /// The git directory for the current worktree (`.git`, or
    /// `.git/worktrees/<name>` for a linked worktree).
    pub fn git_dir(&self) -> PathBuf {
        self.inner.git_dir().to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TestRepo;

    #[test]
    fn discovers_from_root_and_subdir() {
        let repo = TestRepo::init();
        let r = Repo::discover(repo.root()).unwrap();
        assert!(!r.is_bare());
        assert_eq!(canon(&r.current_workdir().unwrap()), canon(repo.root()));

        // From a nested subdirectory, discovery still finds the same worktree.
        let sub = repo.root().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        let r2 = Repo::discover(&sub).unwrap();
        assert_eq!(canon(&r2.current_workdir().unwrap()), canon(repo.root()));
    }

    #[test]
    fn current_workdir_from_linked_worktree() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let linked = repo.root().parent().unwrap().join("wt-x");
        let r = Repo::discover(&linked).unwrap();
        assert_eq!(canon(&r.current_workdir().unwrap()), canon(&linked));
    }

    #[test]
    fn not_in_repo_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(Repo::discover(dir.path()), Err(Error::NotInRepo)));
    }

    #[test]
    fn bare_repo_is_detected() {
        let repo = TestRepo::init_bare();
        let r = Repo::discover(repo.root()).unwrap();
        assert!(r.is_bare());
        assert!(r.current_workdir().is_none());
    }

    /// Canonicalizes a path so comparisons ignore `/private` symlink prefixes on
    /// macOS temp dirs.
    fn canon(p: &Path) -> PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    }
}
