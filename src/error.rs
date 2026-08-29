//! Typed error type for the `wt` library.
//!
//! Library APIs return [`Error`]; the binary maps it to a process exit code via
//! [`Error::exit_code`]. Exit codes follow the spec (§12): `0` success, `1`
//! user/operation error, `2` usage/argument error, `3` ambiguous query or
//! nothing selected.

/// A convenient `Result` alias for `wt` library operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by `wt` library operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The current directory is not inside a Git repository.
    #[error("not in a git repository")]
    NotInRepo,

    /// There is no current worktree (e.g. a bare repository) but the command
    /// requires one.
    #[error("no current worktree (bare repository); pass a query")]
    NoCurrentWorktree,

    /// A query resolved to more than one worktree (spec exit code `3`).
    #[error("query {query:?} is ambiguous ({} candidates)", candidates.len())]
    Ambiguous {
        /// The query string that was ambiguous.
        query: String,
        /// Human-readable identifiers of the matching worktrees.
        candidates: Vec<String>,
    },

    /// A query matched no worktree (spec exit code `1`).
    #[error("no worktree matches {query:?}")]
    NotFound {
        /// The query string that matched nothing.
        query: String,
    },

    /// Nothing was selected, e.g. a cancelled picker (spec exit code `3`).
    #[error("nothing selected")]
    NothingSelected,

    /// A usage or argument error (spec exit code `2`).
    #[error("{0}")]
    Usage(String),

    /// A configuration error, naming the file, key, and reason.
    #[error("{file}: {key}: {reason}")]
    Config {
        /// Path (or label) of the offending config file.
        file: String,
        /// The offending key.
        key: String,
        /// Why it was rejected.
        reason: String,
    },

    /// A subprocess (`git`, `gh`, or a code agent) failed; `stderr` is surfaced
    /// verbatim.
    #[error("{program} failed: {stderr}")]
    Subprocess {
        /// The program that failed (e.g. `git`, `gh`, `claude`).
        program: String,
        /// Captured standard error, verbatim.
        stderr: String,
    },

    /// The `gh` CLI is missing or unauthenticated.
    #[error("{0}")]
    GhUnavailable(String),

    /// No code-agent CLI is available (missing binary or failed to launch).
    #[error("{0}")]
    AgentUnavailable(String),

    /// A code-agent run exceeded its deadline and was killed. Generation is
    /// always best-effort, so callers are expected to fall back rather than
    /// surface this to the user as a failure.
    #[error("{binary} did not respond within {seconds}s")]
    AgentTimeout {
        /// The agent binary that was killed.
        binary: String,
        /// The deadline it exceeded, in seconds.
        seconds: u64,
    },

    /// The repository's `wt.schema` was written by a newer `wt` (or embedder)
    /// than this one; reading it could silently misinterpret metadata, so the
    /// operation is refused (issue #99).
    #[error(
        "repository metadata schema (wt.schema = {found}) is newer than this wt supports \
         (up to {supported}); upgrade wt to work with this repository"
    )]
    SchemaTooNew {
        /// The `wt.schema` value recorded in the repository.
        found: u64,
        /// The highest schema this build understands.
        supported: u64,
    },

    /// The repository's advisory mutation lock could not be acquired (issue
    /// #99) — usually another `wt` (or embedder) operation is in flight.
    #[error("could not acquire the wt repository lock at {path}: {reason}")]
    LockUnavailable {
        /// The lock file path.
        path: String,
        /// Why acquisition failed (e.g. a timeout while another holder ran).
        reason: String,
    },

    /// Worktree removal was blocked by the dirty/unpushed safety guards
    /// (spec §10/§12); force-removal overrides them.
    #[error("worktree {}; use --force to remove anyway", guard_reasons(*dirty, *unpushed))]
    RemoveGuarded {
        /// Modified/staged tracked files (or untracked files when the config
        /// counts them).
        dirty: bool,
        /// Local commits ahead of upstream, or no upstream configured.
        unpushed: bool,
    },

    /// An operation failed for the reason described by the message.
    #[error("{0}")]
    Operation(String),

    /// An underlying I/O error.
    #[error("{0}")]
    Io(#[from] std::io::Error),

    /// A JSON serialization or deserialization error.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Renders the blocking guard reasons for [`Error::RemoveGuarded`], matching
/// the CLI's historical wording.
fn guard_reasons(dirty: bool, unpushed: bool) -> String {
    let mut reasons = Vec::new();
    if dirty {
        reasons.push("has uncommitted changes");
    }
    if unpushed {
        reasons.push("has unpushed work");
    }
    reasons.join(" and ")
}

impl Error {
    /// The process exit code this error maps to (spec §12): `2` for usage
    /// errors, `3` for ambiguous queries or nothing selected, `1` otherwise.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Usage(_) => 2,
            Error::Ambiguous { .. } | Error::NothingSelected => 3,
            _ => 1,
        }
    }

    /// Builds an [`Error::Operation`] from anything string-like.
    pub fn operation(message: impl Into<String>) -> Self {
        Error::Operation(message.into())
    }

    /// Builds an [`Error::Usage`] from anything string-like.
    pub fn usage(message: impl Into<String>) -> Self {
        Error::Usage(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_spec() {
        assert_eq!(Error::usage("x").exit_code(), 2);
        assert_eq!(
            Error::Ambiguous {
                query: "f".into(),
                candidates: vec!["a".into(), "b".into()],
            }
            .exit_code(),
            3
        );
        assert_eq!(Error::NothingSelected.exit_code(), 3);
        assert_eq!(Error::NotFound { query: "f".into() }.exit_code(), 1);
        assert_eq!(Error::NotInRepo.exit_code(), 1);
        assert_eq!(Error::NoCurrentWorktree.exit_code(), 1);
        assert_eq!(
            Error::Config {
                file: "c".into(),
                key: "k".into(),
                reason: "r".into(),
            }
            .exit_code(),
            1
        );
        assert_eq!(
            Error::Subprocess {
                program: "git".into(),
                stderr: "boom".into(),
            }
            .exit_code(),
            1
        );
        assert_eq!(
            Error::RemoveGuarded {
                dirty: true,
                unpushed: false,
            }
            .exit_code(),
            1
        );
        assert_eq!(Error::GhUnavailable("gh".into()).exit_code(), 1);
        assert_eq!(Error::AgentUnavailable("a".into()).exit_code(), 1);
        assert_eq!(Error::operation("op").exit_code(), 1);
        assert_eq!(Error::from(std::io::Error::other("io")).exit_code(), 1);
        let json_err = serde_json::from_str::<i32>("nope").unwrap_err();
        assert_eq!(Error::from(json_err).exit_code(), 1);
    }

    #[test]
    fn display_messages_are_descriptive() {
        assert!(Error::NotInRepo.to_string().contains("git repository"));
        assert!(
            Error::NoCurrentWorktree
                .to_string()
                .contains("no current worktree")
        );
        assert!(
            Error::Ambiguous {
                query: "feat".into(),
                candidates: vec!["a".into(), "b".into()],
            }
            .to_string()
            .contains("ambiguous")
        );
        assert!(
            Error::NotFound { query: "x".into() }
                .to_string()
                .contains("no worktree")
        );
        assert_eq!(Error::NothingSelected.to_string(), "nothing selected");
        assert_eq!(Error::usage("oops").to_string(), "oops");
        assert_eq!(
            Error::Config {
                file: "f".into(),
                key: "k".into(),
                reason: "r".into(),
            }
            .to_string(),
            "f: k: r"
        );
        assert_eq!(
            Error::Subprocess {
                program: "gh".into(),
                stderr: "no auth".into(),
            }
            .to_string(),
            "gh failed: no auth"
        );
        assert_eq!(
            Error::RemoveGuarded {
                dirty: true,
                unpushed: true,
            }
            .to_string(),
            "worktree has uncommitted changes and has unpushed work; use --force to remove anyway"
        );
        assert_eq!(
            Error::RemoveGuarded {
                dirty: false,
                unpushed: true,
            }
            .to_string(),
            "worktree has unpushed work; use --force to remove anyway"
        );
        assert_eq!(Error::GhUnavailable("nope".into()).to_string(), "nope");
        assert_eq!(Error::AgentUnavailable("nope".into()).to_string(), "nope");
        assert_eq!(Error::operation("op").to_string(), "op");
        assert_eq!(Error::from(std::io::Error::other("io")).to_string(), "io");
    }
}
