//! Ahead/behind computation via `git rev-list --left-right --count`. Ahead/behind
//! is async-loaded data (spec §10), so this subprocess is outside the synchronous
//! listing fast-path; `git rev-list` is used for its exact correctness. The same
//! `rev-list` reachability backs [`is_recoverable`], the "deleting this branch
//! loses no commit" check `wt prune` keys on.

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

/// Whether every commit on `branch_ref` is also reachable from a remote-tracking
/// ref or from one of `keep` (e.g. the local default branch), run in `dir` — i.e.
/// deleting the branch loses no commit. True exactly when
/// `git rev-list -n1 <branch_ref> --not --remotes <keep>...` prints nothing. Only
/// as current as the remote-tracking refs, so callers fetch first.
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn is_recoverable(
    git: &dyn GitCli,
    dir: &Path,
    branch_ref: &str,
    keep: &[&str],
) -> Result<bool> {
    let mut args = vec!["rev-list", "-n1", branch_ref, "--not", "--remotes"];
    args.extend_from_slice(keep);
    let output = git.run(dir, &args)?;
    Ok(output.trim().is_empty())
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

    #[test]
    fn recoverable_when_a_remote_ref_holds_every_commit() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("t.txt", "1\n");
        repo.commit_all("t1");
        repo.git(&["checkout", "-q", "main"]);
        let recoverable = |keep: &[&str]| {
            is_recoverable(&RealGit, repo.root(), "refs/heads/topic", keep).unwrap()
        };
        // Its commit exists nowhere else yet.
        assert!(!recoverable(&[]));
        assert!(!recoverable(&["refs/heads/main"]));
        // Once a remote-tracking ref holds the tip, nothing would be lost.
        repo.git(&[
            "update-ref",
            "refs/remotes/origin/topic",
            "refs/heads/topic",
        ]);
        assert!(recoverable(&[]));
        // A new local commit on top is unique again.
        repo.git(&["checkout", "-q", "topic"]);
        repo.write("t.txt", "2\n");
        repo.commit_all("t2");
        repo.git(&["checkout", "-q", "main"]);
        assert!(!recoverable(&[]));
    }

    #[test]
    fn recoverable_when_a_kept_ref_holds_every_commit() {
        let repo = TestRepo::init();
        repo.git(&["branch", "old"]); // at main's tip, no remote refs at all
        assert!(!is_recoverable(&RealGit, repo.root(), "refs/heads/old", &[]).unwrap());
        assert!(
            is_recoverable(
                &RealGit,
                repo.root(),
                "refs/heads/old",
                &["refs/heads/main"]
            )
            .unwrap()
        );
    }

    #[test]
    fn recoverable_errors_on_an_unknown_ref() {
        let repo = TestRepo::init();
        assert!(is_recoverable(&RealGit, repo.root(), "refs/heads/nope", &[]).is_err());
    }
}
