//! `wt switch [<query>]` — navigate to a worktree (spec §5/§7). With a query,
//! resolve and print its path; without one, open the TUI picker. Either way the
//! path goes to stdout (for the shell wrapper to `cd` into); UI goes to stderr.

use crate::cli::SwitchArgs;
use crate::commands::{Resolution, open_session, resolve_query};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::worktree_service::build_worktrees;

/// Resolves the query (or opens the picker) and prints the chosen path.
pub fn run(cx: &mut Cx, args: &SwitchArgs) -> Result<u8> {
    if let Some(query) = &args.query {
        let git = cx.git.clone();
        let session = open_session(cx, git.as_ref())?;
        let worktrees = build_worktrees(&session.repo, git.as_ref())?;
        return match resolve_query(cx, &worktrees, query) {
            Resolution::Found(index) => {
                cx.out.line(&worktrees[index].path.to_string_lossy())?;
                Ok(0)
            }
            Resolution::Ambiguous => Ok(3),
            Resolution::NotFound => Err(Error::NotFound {
                query: query.clone(),
            }),
        };
    }
    launch_picker(cx)
}

/// Launches the TUI picker; on a switch, prints the chosen path (so the wrapper
/// `cd`s). A cancelled picker prints nothing and exits `0` (no `cd`).
pub fn launch_picker(cx: &mut Cx) -> Result<u8> {
    match crate::tui::run_tui(cx, None)? {
        Some(path) => {
            cx.out.line(&path.to_string_lossy())?;
            Ok(0)
        }
        None => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::SwitchArgs;
    use crate::testutil::TestRepo;

    fn args(query: Option<&str>) -> SwitchArgs {
        SwitchArgs {
            query: query.map(str::to_string),
            print_path: false,
        }
    }

    #[test]
    fn switch_with_query_prints_path() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(Some("feature/x"))).unwrap();
        assert_eq!(code, 0);
        assert!(t.out.contents().trim().ends_with("wt-x"));
    }

    #[test]
    fn switch_unknown_query_errors() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(&mut t.cx, &args(Some("nope"))).unwrap_err();
        assert!(matches!(err, crate::error::Error::NotFound { .. }));
    }
}
