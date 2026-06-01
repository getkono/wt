//! Reading commit metadata for display via `gix` (spec §4): short hash
//! (honoring `core.abbrev`), subject, author, and timestamp.

use gix::ObjectId;

use crate::error::{Error, Result};

/// Tip-commit metadata read from the object database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    /// Short commit hash (the full hex truncated to the abbreviation length).
    pub hash: String,
    /// Commit subject (first line of the message).
    pub subject: String,
    /// Author name.
    pub author: String,
    /// Author timestamp as Unix seconds.
    pub timestamp_unix: i64,
}

/// The short-hash abbreviation length, honoring `core.abbrev` when set to a
/// number, otherwise defaulting to 7 (spec §7 "Display conventions").
pub fn abbrev_len(repo: &gix::Repository) -> usize {
    repo.config_snapshot()
        .integer("core.abbrev")
        .and_then(|n| usize::try_from(n).ok())
        .filter(|n| (4..=64).contains(n))
        .unwrap_or(7)
}

/// Reads commit metadata for the object named by `oid_hex` (a full hex OID).
pub fn commit_info(repo: &gix::Repository, oid_hex: &str, abbrev: usize) -> Result<CommitInfo> {
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
}
