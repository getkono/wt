//! The hidden `wt __complete <kind> [partial]` helper (spec §9). Prints
//! candidates one per line; degrades silently (exit `0`, no output) outside a
//! repository or when a backend is unavailable, so the shell never errors.

use crate::cli::CompleteArgs;
use crate::cx::Cx;
use crate::error::Result;
use crate::git::discover::Repo;
use crate::git::local_branches;
use crate::worktree_service::enumerate_worktrees;

/// Prints completion candidates for the requested kind, filtered by the partial
/// token. Any failure results in no output (silent degradation).
pub fn run(cx: &mut Cx, args: &CompleteArgs) -> Result<u8> {
    let partial = args.partial.as_deref().unwrap_or("");
    let candidates = candidates(cx, &args.kind).unwrap_or_default();
    for candidate in candidates {
        if partial.is_empty() || candidate.starts_with(partial) {
            cx.out.line(&candidate)?;
        }
    }
    Ok(0)
}

/// Collects candidates for `kind`, or an error (mapped to no output by the
/// caller) when outside a repo or otherwise unavailable.
fn candidates(cx: &Cx, kind: &str) -> Result<Vec<String>> {
    let git = cx.git.clone();
    let repo = Repo::discover(&cx.cwd)?;
    match kind {
        "worktrees" => {
            let worktrees = enumerate_worktrees(&repo, git.as_ref())?;
            let mut names = Vec::new();
            for worktree in &worktrees {
                if let Some(branch) = &worktree.branch {
                    names.push(branch.clone());
                }
                if let Some(slug) = &worktree.slug
                    && Some(slug) != worktree.branch.as_ref()
                {
                    names.push(slug.clone());
                }
                if let Some(dir) = worktree.path.file_name() {
                    names.push(dir.to_string_lossy().into_owned());
                }
            }
            names.sort();
            names.dedup();
            Ok(names)
        }
        "branches" => local_branches(repo.gix()),
        "pr-numbers" => {
            // Best-effort: silent when gh is missing/unauthenticated (§9).
            let dir = repo.current_workdir().unwrap_or_else(|| cx.cwd.clone());
            Ok(cx
                .gh
                .open_pr_numbers(&dir)
                .unwrap_or_default()
                .into_iter()
                .map(|n| n.to_string())
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::CompleteArgs;
    use crate::testutil::TestRepo;

    fn args(kind: &str, partial: Option<&str>) -> CompleteArgs {
        CompleteArgs {
            kind: kind.to_string(),
            partial: partial.map(str::to_string),
        }
    }

    #[test]
    fn completes_worktrees() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args("worktrees", None)).unwrap();
        let out = t.out.contents();
        assert!(out.contains("main"));
        assert!(out.contains("feature/x"));
        assert!(out.contains("feature-x")); // slug
    }

    #[test]
    fn completes_branches() {
        let repo = TestRepo::init();
        repo.git(&["branch", "topic"]);
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args("branches", None)).unwrap();
        let out = t.out.contents();
        assert!(out.contains("main"));
        assert!(out.contains("topic"));
    }

    #[test]
    fn partial_filters_candidates() {
        let repo = TestRepo::init();
        repo.git(&["branch", "feature-a"]);
        repo.git(&["branch", "hotfix-b"]);
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args("branches", Some("feat"))).unwrap();
        let out = t.out.contents();
        assert!(out.contains("feature-a"));
        assert!(!out.contains("hotfix-b"));
        assert!(!out.contains("main"));
    }

    #[test]
    fn outside_repo_is_silent() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = crate::testutil::test_cx(&[], dir.path().to_str().unwrap());
        let code = super::run(&mut t.cx, &args("worktrees", None)).unwrap();
        assert_eq!(code, 0);
        assert!(t.out.contents().is_empty());
        assert!(t.err.contents().is_empty());
    }

    #[test]
    fn pr_numbers_and_unknown_kind_are_empty() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args("pr-numbers", None)).unwrap();
        super::run(&mut t.cx, &args("bogus", None)).unwrap();
        assert!(t.out.contents().is_empty());
    }
}
