//! `wt path <query>` — print a matching worktree's absolute path (spec §7).

use crate::cli::PathArgs;
use crate::commands::{Resolution, open_session, resolve_query};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::worktree_service::build_worktrees;

/// Resolves the query and prints the worktree's absolute path to stdout. An
/// ambiguous query lists candidates on stderr and exits `3`; no match exits `1`.
pub fn run(cx: &mut Cx, args: &PathArgs) -> Result<u8> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let worktrees = build_worktrees(&session.repo, git.as_ref())?;
    match resolve_query(cx, &worktrees, &args.query) {
        Resolution::Found(index) => {
            cx.out.line(&worktrees[index].path.to_string_lossy())?;
            Ok(0)
        }
        Resolution::Ambiguous => Ok(3),
        Resolution::NotFound => Err(Error::NotFound {
            query: args.query.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::PathArgs;
    use crate::testutil::TestRepo;

    fn args(query: &str) -> PathArgs {
        PathArgs {
            query: query.to_string(),
        }
    }

    #[test]
    fn prints_matching_worktree_path() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args("feature/x")).unwrap();
        assert_eq!(code, 0);
        assert!(t.out.contents().trim().ends_with("wt-x"));
    }

    #[test]
    fn no_match_is_error() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(&mut t.cx, &args("nope")).unwrap_err();
        assert!(matches!(err, crate::error::Error::NotFound { .. }));
    }

    #[test]
    fn ambiguous_lists_candidates_and_exits_three() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/login", "../wt-login");
        repo.add_worktree("feature/logout", "../wt-logout");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args("feature/log")).unwrap();
        assert_eq!(code, 3);
        assert!(t.err.contents().contains("ambiguous"));
        assert!(t.err.contents().contains("feature/login"));
        // Nothing on stdout (no path to cd into).
        assert!(t.out.contents().is_empty());
    }
}
