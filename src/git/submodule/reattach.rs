//! Giving an already-populated submodule directory its own git directory,
//! without rewriting any of the files that are already there.
//!
//! This is the second half of the reflink materialization path. Once a worktree
//! has been cloned copy-on-write from another one, every submodule's *files* are
//! already in place — what is missing is the git directory behind them, since
//! the source's `.git` gitlink points into the source worktree's private
//! `modules/` tree.
//!
//! [`seed`](super::seed) cannot be used here: `git submodule update` refuses to
//! clone into a directory that is not empty, and emptying it first would throw
//! away the very files the reflink just gave us for free.
//!
//! So the git directory is built *beside* the working tree and then attached:
//!
//! 1. `git clone --local --no-checkout --separate-git-dir` into the worktree's
//!    private `modules/<name>` slot, from the repository's own mirror. Local
//!    clones hardlink their objects, so this costs no network and no disk.
//! 2. Point the working directory's `.git` gitlink at it, and set
//!    `core.worktree` back the other way.
//! 3. Adopt the existing files with `update-ref` + `read-tree` +
//!    `update-index --refresh`, which records the recorded commit and marks the
//!    already-present files clean. A `checkout` here would rewrite every file
//!    and undo the saving.
//!
//! As with seeding, this is only an accelerator: the caller still reconciles
//! afterwards, and anything this did not get right is corrected there.
//!
//! One part of that is *not* optional. The clone in step 1 records the mirror as
//! the submodule's `origin`, and an attached submodule reports as initialized —
//! so a caller that only reconciles what is still pending would skip it and
//! leave the mirror path in place, silently fetching from and pushing into the
//! superproject's own object store. Callers must run [`sync`](super::sync)
//! whenever this succeeds, whatever else they decide to do.

use std::path::Path;

use tracing::debug;

use crate::error::{Error, Result};
use crate::git::cli::GitCli;

/// Attaches a git directory to the already-populated submodule at
/// `worktree_dir/path`, cloned from `mirror`, and adopts its files at `sha`.
///
/// `private_modules` is the worktree's own `modules` directory
/// (`$GIT_COMMON_DIR/worktrees/<id>/modules`), which is where git expects a
/// linked worktree's submodule git directories to live.
pub(crate) fn attach(
    git: &dyn GitCli,
    worktree_dir: &Path,
    path: &str,
    name: &str,
    mirror: &Path,
    private_modules: &Path,
    sha: &str,
) -> Result<()> {
    let work = worktree_dir.join(path);
    if !work.is_dir() {
        return Err(Error::operation(format!(
            "submodule {path} has no directory to attach"
        )));
    }
    let gitdir = private_modules.join(name);
    if let Some(parent) = gitdir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // `clone --separate-git-dir` insists on materializing a working tree, so it
    // gets a scratch one that is removed immediately. Cloning straight into the
    // populated directory is what we are avoiding.
    let scratch = private_modules.join(format!(".wt-attach-{}", sanitize(name)));
    let _ = std::fs::remove_dir_all(&scratch);
    let result = git.run(
        worktree_dir,
        &[
            "clone",
            "--local",
            "--no-checkout",
            "--separate-git-dir",
            &gitdir.to_string_lossy(),
            &mirror.to_string_lossy(),
            &scratch.to_string_lossy(),
        ],
    );
    let _ = std::fs::remove_dir_all(&scratch);
    result?;

    // Attach the two halves to each other.
    std::fs::write(work.join(".git"), format!("gitdir: {}\n", gitdir.display()))?;
    git.run(&work, &["config", "core.worktree", &work.to_string_lossy()])?;

    // Adopt the files that are already on disk. `update-ref` records the commit
    // the superproject expects, `read-tree` fills the index from it, and
    // `update-index --refresh` stats the existing files and marks them clean —
    // none of which writes file content.
    git.run(&work, &["update-ref", "--no-deref", "HEAD", sha])?;
    git.run(&work, &["read-tree", "HEAD"])?;
    // A refresh reports "needs update" for genuinely differing files by exiting
    // non-zero; that is information for the reconcile pass, not a failure here.
    if let Err(e) = git.run(&work, &["update-index", "--refresh"]) {
        debug!(submodule = %path, error = %e, "index refresh found differences");
    }
    Ok(())
}

/// Makes a submodule name safe to use as a single directory component for the
/// scratch clone target; names may contain slashes.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    #[test]
    fn sanitize_flattens_path_separators() {
        assert_eq!(sanitize("libs/sub"), "libs-sub");
        assert_eq!(sanitize("a.b"), "a-b");
    }

    #[test]
    fn attach_refuses_a_missing_directory() {
        let repo = TestRepo::init();
        let err = attach(
            &RealGit,
            repo.root(),
            "nope",
            "nope",
            &repo.root().join(".git"),
            &repo.root().join(".git/modules"),
            "HEAD",
        )
        .unwrap_err();
        assert!(matches!(err, Error::Operation(_)));
    }

    #[test]
    fn attaches_a_gitdir_to_files_that_are_already_present() {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        repo.add_worktree("topic", "../wt-attach");
        let wt = repo.root().parent().unwrap().join("wt-attach");
        let sha = repo
            .git(&["-C", &wt.to_string_lossy(), "rev-parse", ":libs/sub"])
            .trim()
            .to_string();

        // Stand in for what a reflink copy would have produced: the submodule's
        // files present, with no git directory of their own.
        let work = wt.join("libs/sub");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::copy(repo.root().join("libs/sub/sub.txt"), work.join("sub.txt")).unwrap();

        let private = wt.join(".git");
        let private_modules = std::fs::read_to_string(&private)
            .ok()
            .and_then(|s| s.strip_prefix("gitdir: ").map(|p| p.trim().to_string()))
            .map(|p| Path::new(&p).join("modules"))
            .expect("linked worktree gitfile");

        attach(
            &RealGit,
            &wt,
            "libs/sub",
            "libs/sub",
            &repo.root().join(".git/modules/libs/sub"),
            &private_modules,
            &sha,
        )
        .unwrap();

        // The superproject now sees a populated submodule at the right commit,
        // and the file was never rewritten.
        let status = repo.git(&["-C", &wt.to_string_lossy(), "submodule", "status"]);
        assert!(
            status.starts_with(' '),
            "submodule not in sync after attach: {status:?}"
        );
        assert!(status.contains(&sha), "wrong commit: {status:?}");
        assert_eq!(
            std::fs::read_to_string(work.join("sub.txt")).unwrap(),
            "submodule\n"
        );
        // And the submodule's own working tree is clean.
        let inner = repo.git(&["-C", &work.to_string_lossy(), "status", "--short"]);
        assert!(inner.trim().is_empty(), "submodule is dirty: {inner:?}");
    }
}
