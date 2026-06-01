//! `wt root` — print the repository root (spec §7).

use crate::commands::open_session;
use crate::cx::Cx;
use crate::error::Result;

/// Prints the primary worktree (or bare repo) root to stdout.
pub fn run(cx: &mut Cx) -> Result<u8> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    cx.out.line(&session.primary_root.to_string_lossy())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::testutil::TestRepo;
    use std::path::Path;

    #[test]
    fn prints_repo_root() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx).unwrap();
        assert_eq!(code, 0);
        let printed = t.out.contents();
        let printed = printed.trim();
        assert_eq!(
            std::fs::canonicalize(printed).unwrap(),
            std::fs::canonicalize(repo.root()).unwrap()
        );
        // Nothing on stderr.
        assert!(t.err.contents().is_empty());
        assert!(Path::new(printed).is_absolute());
    }
}
