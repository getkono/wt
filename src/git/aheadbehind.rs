//! Ahead/behind computation via `git rev-list --left-right --count`. Ahead/behind
//! is async-loaded data (spec §10), so this subprocess is outside the synchronous
//! listing fast-path; `git rev-list` is used for its exact correctness.

use std::path::Path;

use crate::error::{Error, Result};
use crate::git::cli::GitCli;

/// Counts how far `branch_ref` is ahead of and behind `upstream_ref`, run in
/// `dir`. Returns `(ahead, behind)`.
pub(crate) fn ahead_behind(
    git: &dyn GitCli,
    dir: &Path,
    upstream_ref: &str,
    branch_ref: &str,
) -> Result<(u32, u32)> {
    let range = format!("{upstream_ref}...{branch_ref}");
    let output = git.run(dir, &["rev-list", "--left-right", "--count", &range])?;
    parse_left_right(&output)
}

/// Parses the `<behind>\t<ahead>` output of `rev-list --left-right --count`
/// (left = commits only in the upstream = behind; right = only in branch = ahead).
fn parse_left_right(text: &str) -> Result<(u32, u32)> {
    let mut parts = text.split_whitespace();
    let behind = parts.next().and_then(|s| s.parse::<u32>().ok());
    let ahead = parts.next().and_then(|s| s.parse::<u32>().ok());
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => Ok((ahead, behind)),
        _ => Err(Error::operation(format!(
            "unexpected rev-list output: {text:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    #[test]
    fn parse_left_right_orders_ahead_then_behind() {
        // "behind\tahead"
        assert_eq!(parse_left_right("0\t2\n").unwrap(), (2, 0));
        assert_eq!(parse_left_right("3\t1").unwrap(), (1, 3));
        assert!(parse_left_right("garbage").is_err());
    }

    #[test]
    fn ahead_of_upstream() {
        let repo = TestRepo::init();
        // Simulate an upstream by recording origin/main at the initial commit.
        let base = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &base]);
        // Two new commits on main make it 2 ahead, 0 behind.
        repo.write("a.txt", "1\n");
        repo.commit_all("c1");
        repo.write("b.txt", "2\n");
        repo.commit_all("c2");
        let (ahead, behind) = ahead_behind(
            &RealGit,
            repo.root(),
            "refs/remotes/origin/main",
            "refs/heads/main",
        )
        .unwrap();
        assert_eq!((ahead, behind), (2, 0));
    }

    #[test]
    fn behind_upstream() {
        let repo = TestRepo::init();
        let c1 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.write("a.txt", "1\n");
        repo.commit_all("c2");
        // origin/main is ahead of main by one commit -> main is 1 behind.
        let c2 = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/main", &c2]);
        repo.git(&["reset", "-q", "--hard", &c1]);
        let (ahead, behind) = ahead_behind(
            &RealGit,
            repo.root(),
            "refs/remotes/origin/main",
            "refs/heads/main",
        )
        .unwrap();
        assert_eq!((ahead, behind), (0, 1));
    }
}
