//! `wt drop` — remove the worktree containing the current directory (issue #81).
//!
//! Unlike `wt remove <query>`, `drop` takes no query: it targets whichever
//! worktree the current directory belongs to (resolved via the `is_current`
//! marker, so it works from any depth below the worktree root). It always keeps
//! the branch, refuses the primary/root worktree, and on success prints the
//! primary root to stdout so the shell wrapper `cd`s back into it.

use crate::cli::DropArgs;
use crate::commands::remove::{RemoveOptions, remove_resolved};
use crate::commands::{candidate_label, open_session};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::hooks::HookRunner;
use crate::model::RemovedResult;
use crate::worktree_service::build_worktrees;

/// Removes the worktree that contains the current directory, keeping its branch.
/// The primary/root worktree is refused, and a bare repository (no current
/// worktree) yields [`Error::NoCurrentWorktree`]. On success the primary root is
/// printed to stdout (the shell wrapper navigates there); with `--json` a
/// [`RemovedResult`] is printed instead.
pub(crate) fn run(cx: &mut Cx, hooks: &dyn HookRunner, args: &DropArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let git = git.as_ref();
    let session = open_session(cx, git)?;
    let root = session.primary_root.clone();
    let worktrees = build_worktrees(&session.repo, git)?;

    let index = worktrees
        .iter()
        .position(|w| w.is_current)
        .ok_or(Error::NoCurrentWorktree)?;
    let worktree = worktrees[index].clone();

    if worktree.is_main {
        return Err(Error::operation("refusing to drop the primary worktree"));
    }

    let opts = RemoveOptions {
        force_remove: args.force,
        force_branch: false,
        keep_branch: true,
        no_hooks: args.no_hooks,
    };
    remove_resolved(cx, git, hooks, &session, &root, &worktree, &opts)?;

    if json {
        let result = RemovedResult {
            worktree: worktree.clone(),
            removed: true,
        };
        cx.out.line(&serde_json::to_string(&result)?)?;
    } else {
        cx.err
            .line(&format!("dropped worktree {}", candidate_label(&worktree)))?;
        // The primary root is an existing directory; the shell wrapper `cd`s into
        // it so the user is not stranded in the just-removed worktree.
        cx.out.line(&root.to_string_lossy())?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::cli::DropArgs;
    use crate::error::Result;
    use crate::hooks::RealHookRunner;
    use crate::testutil::{TestRepo, give_upstream, make_wt, wt_dir};

    fn args(force: bool) -> DropArgs {
        DropArgs {
            force,
            no_hooks: true,
        }
    }

    /// Runs `wt drop` with the context's working directory set to `cwd`.
    fn run_in(cwd: &Path, a: &DropArgs, json: bool) -> Result<(u8, String, String)> {
        let mut t = crate::testutil::test_cx(&[], cwd.to_str().unwrap());
        let code = super::run(&mut t.cx, &RealHookRunner, a, json)?;
        Ok((code, t.out.contents(), t.err.contents()))
    }

    #[test]
    fn drops_current_worktree_and_keeps_branch() {
        let repo = TestRepo::init();
        make_wt(&repo, "featurex");
        give_upstream(&repo, "featurex"); // not unpushed
        let wt = wt_dir(&repo, "featurex");
        let (code, out, err) = run_in(&wt, &args(false), false).unwrap();
        assert_eq!(code, 0);
        assert!(err.contains("dropped worktree featurex"));
        // The worktree is gone but the branch is kept.
        assert!(!repo.git(&["worktree", "list"]).contains("featurex"));
        assert!(
            !repo
                .git(&["branch", "--list", "featurex"])
                .trim()
                .is_empty()
        );
        // stdout is the primary root — the cd target.
        assert!(crate::commands::same_path(
            Path::new(out.trim()),
            repo.root()
        ));
    }

    #[test]
    fn drops_from_arbitrarily_deep_subdir() {
        let repo = TestRepo::init();
        make_wt(&repo, "deepwt");
        give_upstream(&repo, "deepwt");
        // Drop from a directory several levels below the worktree root.
        let deep = wt_dir(&repo, "deepwt").join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        let (code, _, err) = run_in(&deep, &args(false), false).unwrap();
        assert_eq!(code, 0);
        assert!(err.contains("dropped worktree deepwt"));
        assert!(!repo.git(&["worktree", "list"]).contains("deepwt"));
        assert!(!repo.git(&["branch", "--list", "deepwt"]).trim().is_empty());
    }

    #[test]
    fn refuses_primary_worktree() {
        let repo = TestRepo::init();
        // The repo root is the primary worktree.
        let err = run_in(repo.root(), &args(false), false).unwrap_err();
        assert!(err.to_string().contains("primary"));
        assert!(repo.git(&["worktree", "list"]).contains("main"));
    }

    #[test]
    fn dirty_blocks_without_force_then_force_removes() {
        let repo = TestRepo::init();
        make_wt(&repo, "dirtywt");
        give_upstream(&repo, "dirtywt");
        let wt = wt_dir(&repo, "dirtywt");
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();
        // Blocked without --force.
        let err = run_in(&wt, &args(false), false).unwrap_err();
        assert!(err.to_string().contains("uncommitted"));
        assert!(err.to_string().contains("--force"));
        // --force removes it with a data-loss warning; the branch survives.
        let (code, _, e) = run_in(&wt, &args(true), false).unwrap();
        assert_eq!(code, 0);
        assert!(e.contains("data may be lost"));
        assert!(!repo.git(&["worktree", "list"]).contains("dirtywt"));
        assert!(!repo.git(&["branch", "--list", "dirtywt"]).trim().is_empty());
    }

    #[test]
    fn json_emits_removed_flag_and_keeps_branch() {
        let repo = TestRepo::init();
        make_wt(&repo, "featurej");
        give_upstream(&repo, "featurej");
        let wt = wt_dir(&repo, "featurej");
        let (code, out, _) = run_in(&wt, &args(false), true).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["removed"], serde_json::json!(true));
        assert_eq!(v["branch"], serde_json::json!("featurej"));
        // Branch kept even in the JSON path.
        assert!(
            !repo
                .git(&["branch", "--list", "featurej"])
                .trim()
                .is_empty()
        );
    }
}
