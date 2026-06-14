//! Reading commit metadata for display via `gix` (spec §4): short hash
//! (honoring `core.abbrev`), subject, author, and timestamp.

use gix::ObjectId;

use crate::error::{Error, Result};

/// Tip-commit metadata read from the object database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommitInfo {
    /// Short commit hash (the full hex truncated to the abbreviation length).
    pub(crate) hash: String,
    /// Commit subject (first line of the message).
    pub(crate) subject: String,
    /// Author name.
    pub(crate) author: String,
    /// Author timestamp as Unix seconds.
    pub(crate) timestamp_unix: i64,
}

/// The short-hash abbreviation length, honoring `core.abbrev` when set to a
/// number, otherwise defaulting to 7 (spec §7 "Display conventions").
pub(crate) fn abbrev_len(repo: &gix::Repository) -> usize {
    repo.config_snapshot()
        .integer("core.abbrev")
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| (4..=64).contains(n))
        .unwrap_or(7)
}

/// Reads commit metadata for the object named by `oid_hex` (a full hex OID).
pub(crate) fn commit_info(
    repo: &gix::Repository,
    oid_hex: &str,
    abbrev: usize,
) -> Result<CommitInfo> {
    let id = ObjectId::from_hex(oid_hex.as_bytes())
        .map_err(|e| Error::operation(format!("invalid object id {oid_hex:?}: {e}")))?;
    let commit = repo
        .find_object(id)
        .map_err(|e| Error::operation(format!("cannot read object {oid_hex}: {e}")))?
        .try_into_commit()
        .map_err(|e| Error::operation(format!("object {oid_hex} is not a commit: {e}")))?;

    let message = commit
        .message()
        .map_err(|e| Error::operation(format!("cannot decode commit message: {e}")))?;
    let subject = message.summary().to_string();

    let author = commit
        .author()
        .map_err(|e| Error::operation(format!("cannot decode commit author: {e}")))?;
    let name = author.name.to_string();
    let timestamp_unix = author.seconds();

    let len = abbrev.clamp(4, oid_hex.len());
    Ok(CommitInfo {
        hash: oid_hex[..len].to_string(),
        subject,
        author: name,
        timestamp_unix,
    })
}

/// Reads up to `max` recent commits starting at `start_hex` (newest first) by
/// walking ancestry via `gix` (spec §4 reads). Best-effort: an invalid start or
/// an unreadable commit simply truncates the result.
pub(crate) fn recent_commits(
    repo: &gix::Repository,
    start_hex: &str,
    abbrev: usize,
    max: usize,
) -> Vec<CommitInfo> {
    let Ok(id) = ObjectId::from_hex(start_hex.as_bytes()) else {
        return Vec::new();
    };
    let Ok(walk) = repo.rev_walk([id]).all() else {
        return Vec::new();
    };
    walk.take(max)
        .filter_map(std::result::Result::ok)
        .filter_map(|info| commit_info(repo, &info.id.to_string(), abbrev).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::discover::Repo;
    use crate::testutil::TestRepo;

    fn head_oid(repo: &TestRepo) -> String {
        repo.git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    #[test]
    fn reads_subject_author_and_short_hash() {
        let repo = TestRepo::init();
        let oid = head_oid(&repo);
        let r = Repo::discover(repo.root()).unwrap();
        let info = commit_info(r.gix(), &oid, 7).unwrap();
        assert_eq!(info.hash.len(), 7);
        assert!(oid.starts_with(&info.hash));
        assert_eq!(info.subject, "init");
        assert_eq!(info.author, "wt Test");
        assert!(info.timestamp_unix > 1_600_000_000);
    }

    #[test]
    fn subject_is_first_line_only() {
        let repo = TestRepo::init();
        repo.write("f.txt", "x\n");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "summary line\n\nbody text"]);
        let oid = head_oid(&repo);
        let r = Repo::discover(repo.root()).unwrap();
        let info = commit_info(r.gix(), &oid, 10).unwrap();
        assert_eq!(info.subject, "summary line");
        assert_eq!(info.hash.len(), 10);
    }

    #[test]
    fn abbrev_len_defaults_to_seven() {
        let repo = TestRepo::init();
        // "auto" (git's default) is not an integer, so it falls through to 7
        // regardless of any host-global core.abbrev value.
        repo.git(&["config", "core.abbrev", "auto"]);
        let r = Repo::discover(repo.root()).unwrap();
        assert_eq!(abbrev_len(r.gix()), 7);
        repo.git(&["config", "core.abbrev", "12"]);
        let r2 = Repo::discover(repo.root()).unwrap();
        assert_eq!(abbrev_len(r2.gix()), 12);
    }

    #[test]
    fn invalid_oid_errors() {
        let repo = TestRepo::init();
        let r = Repo::discover(repo.root()).unwrap();
        assert!(commit_info(r.gix(), "not-hex", 7).is_err());
    }

    #[test]
    fn recent_commits_walks_newest_first_and_caps() {
        let repo = TestRepo::init(); // one commit: "init"
        repo.write("a.txt", "1\n");
        repo.commit_all("second");
        repo.write("b.txt", "2\n");
        repo.commit_all("third");
        let oid = head_oid(&repo);
        let r = Repo::discover(repo.root()).unwrap();
        let commits = recent_commits(r.gix(), &oid, 7, 5);
        assert_eq!(commits.len(), 3);
        assert_eq!(commits[0].subject, "third"); // newest first
        assert_eq!(commits[2].subject, "init");
        // The cap is honored.
        assert_eq!(recent_commits(r.gix(), &oid, 7, 2).len(), 2);
        // An invalid start yields nothing.
        assert!(recent_commits(r.gix(), "not-hex", 7, 5).is_empty());
    }
}
