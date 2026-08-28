//! Copy-on-write file cloning, for materializing a worktree without paying for
//! a second copy of its bytes.
//!
//! On a CoW filesystem (btrfs, XFS with `reflink=1`, APFS, ReFS) a file can be
//! cloned by sharing its extents: the copy is near-instant, consumes almost no
//! disk, and diverges only where one side is later written. Everywhere else the
//! operation is simply unavailable — this module reports that rather than
//! silently falling back to a byte copy, because a caller that wanted a cheap
//! clone and got an expensive one has lost the reason it asked.
//!
//! Nothing here is used unless the caller opted in; see `[create] reflink`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Whether `dir`'s filesystem supports reflinks, tested by actually performing
/// one rather than by inspecting the filesystem type.
///
/// Filesystem-type detection is not enough: XFS only supports reflinks when
/// formatted with `reflink=1`, and a mount can be anything. The probe writes two
/// temporary files inside `dir` and removes them.
pub fn is_supported(dir: &Path) -> bool {
    let src = dir.join(".wt-reflink-probe");
    let dst = dir.join(".wt-reflink-probe-clone");
    // Best-effort cleanup of both paths regardless of where we bail out.
    let cleanup = || {
        let _ = std::fs::remove_file(&src);
        let _ = std::fs::remove_file(&dst);
    };
    cleanup();
    if std::fs::write(&src, b"wt").is_err() {
        cleanup();
        return false;
    }
    let ok = reflink_copy::reflink(&src, &dst).is_ok();
    cleanup();
    ok
}

/// Reflinks the file at `src` to `dst`, creating parent directories.
///
/// Fails rather than degrading to a byte copy; the caller decides what to do
/// when CoW is unavailable.
pub fn clone_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    reflink_copy::reflink(src, dst).map_err(|e| {
        Error::operation(format!(
            "could not reflink {} to {}: {e}",
            src.display(),
            dst.display()
        ))
    })
}

/// Recursively reflinks the tree at `src` into `dst`, skipping every entry whose
/// path relative to `src` is in `skip`.
///
/// Skipping a directory skips its whole subtree. Paths are relative and exact,
/// so `.git` skips only the tree's own git directory and never a nested
/// repository's.
///
/// Symlinks are recreated as symlinks (never followed — following them would
/// copy content from outside the tree and turn a relative link into a wrong
/// absolute one). File permissions ride along with the clone.
pub fn clone_tree(src: &Path, dst: &Path, skip: &HashSet<PathBuf>) -> Result<()> {
    clone_into(src, dst, Path::new(""), skip)
}

/// Recursive worker for [`clone_tree`]. `rel` is the current directory's path
/// relative to the clone root, which is what `skip` is matched against.
fn clone_into(src: &Path, dst: &Path, rel: &Path, skip: &HashSet<PathBuf>) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let child_rel = rel.join(&name);
        if skip.contains(&child_rel) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        // `file_type` on the DirEntry does not follow symlinks, which is what we
        // want: a symlink is recreated, not resolved.
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            let target = std::fs::read_link(&from)?;
            symlink(&target, &to)?;
        } else if file_type.is_dir() {
            clone_into(&from, &to, &child_rel, skip)?;
        } else if file_type.is_file() {
            clone_file(&from, &to)?;
        }
        // Sockets, fifos and devices have no place in a worktree; skip them
        // rather than failing the whole clone.
    }
    Ok(())
}

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(windows)]
fn symlink(target: &Path, link: &Path) -> Result<()> {
    // A worktree symlink may point at either; pick by what the target is now.
    if target.is_dir() {
        std::os::windows::fs::symlink_dir(target, link)?;
    } else {
        std::os::windows::fs::symlink_file(target, link)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A temporary directory on a reflink-capable filesystem, or `None`.
    ///
    /// The system temp dir is very often tmpfs, which has no reflink support, so
    /// falling back to `target/` — on whatever filesystem the checkout lives on
    /// — is what makes these tests actually run on a developer machine. When
    /// neither supports it the behavioural tests skip, which is the honest
    /// outcome: there is nothing to assert about CoW without CoW.
    fn cow_dir() -> Option<tempfile::TempDir> {
        if let Ok(dir) = tempfile::tempdir()
            && is_supported(dir.path())
        {
            return Some(dir);
        }
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        std::fs::create_dir_all(&target).ok()?;
        let dir = tempfile::tempdir_in(&target).ok()?;
        is_supported(dir.path()).then_some(dir)
    }

    #[test]
    fn probe_leaves_no_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        // Whatever the answer, the probe must not litter the directory.
        let _ = is_supported(dir.path());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn probe_is_false_for_a_missing_directory() {
        assert!(!is_supported(Path::new("/definitely/not/here")));
    }

    #[test]
    fn clone_file_reports_an_error_instead_of_copying() {
        // A missing source must surface as a typed error, never a silent success.
        let dir = tempfile::tempdir().unwrap();
        let err = clone_file(&dir.path().join("nope"), &dir.path().join("out")).unwrap_err();
        assert!(matches!(err, Error::Operation(_)));
    }

    #[test]
    fn clones_a_tree_with_contents_symlinks_and_skips() {
        let Some(dir) = cow_dir() else {
            // No CoW filesystem here; the behaviour is exercised where there is.
            return;
        };
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::create_dir_all(src.join(".git")).unwrap();
        std::fs::write(src.join("a.txt"), "alpha").unwrap();
        std::fs::write(src.join("nested/b.txt"), "beta").unwrap();
        std::fs::write(src.join(".git/config"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();

        let dst = dir.path().join("dst");
        clone_tree(&src, &dst, &HashSet::from([PathBuf::from(".git")])).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
        assert_eq!(
            std::fs::read_to_string(dst.join("nested/b.txt")).unwrap(),
            "beta"
        );
        assert!(!dst.join(".git").exists(), "skip list was not honoured");
        #[cfg(unix)]
        {
            let meta = std::fs::symlink_metadata(dst.join("link")).unwrap();
            assert!(meta.file_type().is_symlink(), "symlink was followed");
            assert_eq!(
                std::fs::read_link(dst.join("link")).unwrap(),
                Path::new("a.txt")
            );
        }
    }

    #[test]
    fn cloned_files_do_not_share_later_writes() {
        // Copy-on-write, not a hardlink: writing to one side must not touch the
        // other. Getting this wrong would corrupt the source worktree.
        let Some(dir) = cow_dir() else {
            return;
        };
        let src = dir.path().join("one.txt");
        let dst = dir.path().join("two.txt");
        std::fs::write(&src, "original").unwrap();
        clone_file(&src, &dst).unwrap();
        std::fs::write(&dst, "changed").unwrap();
        assert_eq!(std::fs::read_to_string(&src).unwrap(), "original");
    }

    #[test]
    fn skips_nested_paths_and_whole_subtrees() {
        let Some(dir) = cow_dir() else {
            return;
        };
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("keep")).unwrap();
        std::fs::create_dir_all(src.join("drop/deep")).unwrap();
        std::fs::write(src.join("keep/yes.txt"), "yes").unwrap();
        std::fs::write(src.join("keep/no.txt"), "no").unwrap();
        std::fs::write(src.join("drop/deep/gone.txt"), "gone").unwrap();

        let dst = dir.path().join("dst");
        let skip = HashSet::from([PathBuf::from("keep/no.txt"), PathBuf::from("drop")]);
        clone_tree(&src, &dst, &skip).unwrap();

        assert!(dst.join("keep/yes.txt").exists());
        assert!(
            !dst.join("keep/no.txt").exists(),
            "a nested skip was ignored"
        );
        // Skipping a directory takes its subtree with it, and does not leave the
        // directory itself behind either.
        assert!(!dst.join("drop").exists(), "a skipped subtree was cloned");
    }

    #[test]
    fn skip_list_applies_only_at_the_top_level() {
        let Some(dir) = cow_dir() else {
            return;
        };
        let src = dir.path().join("src");
        std::fs::create_dir_all(src.join("sub/.git")).unwrap();
        std::fs::write(src.join("sub/.git/keep"), "nested").unwrap();
        let dst = dir.path().join("dst");
        clone_tree(&src, &dst, &HashSet::from([PathBuf::from(".git")])).unwrap();
        // A nested repo's gitdir is part of the tree being cloned; only the
        // superproject's own `.git` is the caller's business.
        assert!(dst.join("sub/.git/keep").exists());
    }
}
