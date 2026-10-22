//! Ahead/behind computation via `git rev-list --left-right --count`. Ahead/behind
//! is async-loaded data (spec §10), so this subprocess is outside the synchronous
//! listing fast-path; `git rev-list` is used for its exact correctness. The same
//! `rev-list` reachability backs [`recoverable_tips`], the "deleting this branch
//! loses no commit" check `wt prune` keys on.

use std::collections::HashSet;
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

/// Commit ids per `rev-list` call in [`recoverable_tips`], so the argv stays far
/// below any platform's limit however many branches a repository has.
const TIPS_PER_CALL: usize = 512;

/// The subset of `tips` (full commit ids) whose every commit is also reachable
/// from a remote-tracking ref or from one of `keep` (e.g. the local default
/// branch), run in `dir` — i.e. the tips whose branch can be deleted without
/// losing a commit. Only as current as the remote-tracking refs, so callers
/// fetch first.
///
/// One `git rev-list <tips>... --not --remotes <keep>...` lists every commit
/// reachable from some tip and from nothing kept; a tip is recoverable exactly
/// when it is not among them (a tip is printed itself whenever any of its
/// commits is). That is the per-tip `rev-list -n1 <tip> --not ...` check,
/// answered for many tips in one process.
#[cfg_attr(not(feature = "cli"), allow(dead_code))]
pub(crate) fn recoverable_tips(
    git: &dyn GitCli,
    dir: &Path,
    tips: &[&str],
    keep: &[&str],
) -> Result<HashSet<String>> {
    let mut unique: HashSet<String> = HashSet::new();
    for chunk in tips.chunks(TIPS_PER_CALL) {
        let mut args = vec!["rev-list"];
        args.extend_from_slice(chunk);
        args.extend(["--not", "--remotes"]);
        args.extend_from_slice(keep);
        let output = git.run(dir, &args)?;
        unique.extend(output.lines().map(|line| line.trim().to_string()));
    }
    Ok(tips
        .iter()
        .filter(|tip| !unique.contains(**tip))
        .map(|tip| tip.to_string())
        .collect())
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

    fn commit_of(repo: &TestRepo, rev: &str) -> String {
        repo.git(&["rev-parse", rev]).trim().to_string()
    }

    /// Whether the single tip `rev` is recoverable given `keep`.
    fn recoverable(repo: &TestRepo, rev: &str, keep: &[&str]) -> bool {
        let tip = commit_of(repo, rev);
        recoverable_tips(&RealGit, repo.root(), &[&tip], keep)
            .unwrap()
            .contains(&tip)
    }

    #[test]
    fn recoverable_when_a_remote_ref_holds_every_commit() {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "topic"]);
        repo.write("t.txt", "1\n");
        repo.commit_all("t1");
        repo.git(&["checkout", "-q", "main"]);
        // Its commit exists nowhere else yet.
        assert!(!recoverable(&repo, "topic", &[]));
        assert!(!recoverable(&repo, "topic", &["refs/heads/main"]));
        // Once a remote-tracking ref holds the tip, nothing would be lost.
        repo.git(&[
            "update-ref",
            "refs/remotes/origin/topic",
            "refs/heads/topic",
        ]);
        assert!(recoverable(&repo, "topic", &[]));
        // A new local commit on top is unique again.
        repo.git(&["checkout", "-q", "topic"]);
        repo.write("t.txt", "2\n");
        repo.commit_all("t2");
        repo.git(&["checkout", "-q", "main"]);
        assert!(!recoverable(&repo, "topic", &[]));
    }

    #[test]
    fn recoverable_when_a_kept_ref_holds_every_commit() {
        let repo = TestRepo::init();
        repo.git(&["branch", "old"]); // at main's tip, no remote refs at all
        assert!(!recoverable(&repo, "old", &[]));
        assert!(recoverable(&repo, "old", &["refs/heads/main"]));
    }

    #[test]
    fn recoverable_tips_sorts_a_mixed_batch_in_one_call() {
        // A pushed tip, an unpushed commit on top of it, and `main` beneath
        // it: the unpushed tip alone is held back, though it shares history
        // with the other two.
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "local"]);
        repo.write("p.txt", "1\n");
        repo.commit_all("pushed");
        repo.git(&["update-ref", "refs/remotes/origin/pushed", "HEAD"]);
        repo.write("l.txt", "1\n");
        repo.commit_all("local on top");
        repo.git(&["checkout", "-q", "main"]);
        let pushed = commit_of(&repo, "refs/remotes/origin/pushed");
        let local = commit_of(&repo, "local");
        let main = commit_of(&repo, "main");
        let got = recoverable_tips(&RealGit, repo.root(), &[&pushed, &local, &main], &[]).unwrap();
        assert!(got.contains(&pushed));
        assert!(!got.contains(&local));
        assert!(got.contains(&main));
    }

    #[test]
    fn recoverable_tips_spans_chunks() {
        // More tips than one call takes: the answer does not depend on which
        // chunk a tip lands in.
        let repo = TestRepo::init();
        let main = commit_of(&repo, "main");
        repo.git(&["update-ref", "refs/remotes/origin/main", &main]);
        repo.git(&["checkout", "-q", "-b", "wip"]);
        repo.write("w.txt", "1\n");
        repo.commit_all("wip");
        repo.git(&["checkout", "-q", "main"]);
        let wip = commit_of(&repo, "wip");
        let mut tips: Vec<&str> = vec![main.as_str(); TIPS_PER_CALL];
        tips.push(&wip);
        tips.push(&main);
        let got = recoverable_tips(&RealGit, repo.root(), &tips, &[]).unwrap();
        assert!(got.contains(&main));
        assert!(!got.contains(&wip));
    }

    #[test]
    fn recoverable_tips_errors_on_an_unknown_tip() {
        let repo = TestRepo::init();
        let unknown = "0123456789abcdef0123456789abcdef01234567";
        assert!(recoverable_tips(&RealGit, repo.root(), &[unknown], &[]).is_err());
        assert!(
            recoverable_tips(&RealGit, repo.root(), &[], &[])
                .unwrap()
                .is_empty()
        );
    }
}
