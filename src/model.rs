//! Domain model: the worktree row and its JSON schema (spec §7), plus the
//! sort and column enums used by `list`/`status`.
//!
//! [`Worktree`] serializes to exactly the stable schema documented in §7. The
//! `Option` fields encode the spec's null semantics: `ahead`/`behind` are
//! `None` (→ JSON `null`) when there is no upstream; the working-tree fields and
//! `commit` are `None` for a missing worktree; `branch`/`slug` are `None` for a
//! detached HEAD. `None` serializes as `null` (the fields are never omitted).

use std::path::PathBuf;

use serde::Serialize;

use crate::error::{Error, Result};

/// The current `--json` schema version (spec §7/§13). Bumped only on a breaking
/// change so consumers can detect incompatibility.
pub const SCHEMA_VERSION: u32 = 1;

/// One worktree row — the stable §7 JSON schema shared by `list`, `status`, and
/// the `new`/`pr`/`remove` result objects.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Worktree {
    /// Schema version (always [`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// Absolute path of the worktree.
    pub path: PathBuf,
    /// Full branch name, or `None` for a detached HEAD.
    pub branch: Option<String>,
    /// Filesystem-safe slug of the branch, or `None` when detached.
    pub slug: Option<String>,
    /// Whether this is the current worktree.
    pub is_current: bool,
    /// Whether this is the primary worktree.
    pub is_main: bool,
    /// Whether the worktree's directory has been deleted externally.
    pub is_missing: bool,
    /// Whether the worktree has a detached HEAD.
    pub is_detached: bool,
    /// Whether tracked files are modified/staged; `None` when missing.
    pub dirty: Option<bool>,
    /// Whether untracked files are present; `None` when missing.
    pub has_untracked: Option<bool>,
    /// Commits ahead of upstream; `None` when no upstream or missing.
    pub ahead: Option<u32>,
    /// Commits behind upstream; `None` when no upstream or missing.
    pub behind: Option<u32>,
    /// Upstream tracking branch (e.g. `origin/feature/login`); `None` if unset.
    pub upstream: Option<String>,
    /// Base ref recorded at creation; `None` if unset.
    pub base_ref: Option<String>,
    /// Tip commit metadata; `None` when missing.
    pub commit: Option<Commit>,
    /// Recorded pull request; `None` when none.
    pub pr: Option<Pr>,
    /// Whether a checked-out worktree exists for this row. `false` marks a
    /// "branch row": a local branch with no worktree, listed beneath the real
    /// worktrees with its ahead/behind relative to its base (issue #47). Not part
    /// of the §7 JSON schema (where every row is a real worktree), so it is skipped
    /// during serialization. Branch rows are normally TUI-only, but `wt sync
    /// <branch>` of a worktree-less branch emits one in `--json`; such a row's
    /// `path` is the `branch://<branch>` sentinel rather than a filesystem path,
    /// since no checkout exists.
    #[serde(skip)]
    pub has_worktree: bool,
    /// Up to the last five commits, for the TUI detail pane only. Not part of
    /// the §7 JSON schema (which carries only the tip `commit`), so it is skipped
    /// during serialization.
    #[serde(skip)]
    pub recent_commits: Vec<Commit>,
    /// The recorded PR URL, for the TUI detail pane only. Not part of the §7
    /// `pr` object, so it is skipped during serialization.
    #[serde(skip)]
    pub pr_url: Option<String>,
    /// Offline merge/tracking state, for delete-safety messaging in the TUI
    /// only. `None` until enrichment runs (and for a missing worktree, where it
    /// cannot be computed). Not part of the §7 JSON schema, so it is skipped
    /// during serialization.
    #[serde(skip)]
    pub merge_state: Option<MergeState>,
}

impl Worktree {
    /// Builds a worktree row with the given absolute path and all other fields
    /// at their defaults (no branch, all flags false, all optionals `None`, and
    /// `has_worktree` true — a real checkout). Callers populate the remaining
    /// fields.
    pub fn new(path: PathBuf) -> Self {
        Worktree {
            schema_version: SCHEMA_VERSION,
            path,
            branch: None,
            slug: None,
            is_current: false,
            is_main: false,
            is_missing: false,
            is_detached: false,
            dirty: None,
            has_untracked: None,
            ahead: None,
            behind: None,
            upstream: None,
            base_ref: None,
            commit: None,
            pr: None,
            has_worktree: true,
            recent_commits: Vec::new(),
            pr_url: None,
            merge_state: None,
        }
    }

    /// Serializes this row to a single-line JSON string (no trailing newline),
    /// for the newline-delimited `--json` framing of `list`/`status`.
    pub fn to_json_line(&self) -> Result<String> {
        Ok(serde_json::to_string(self)?)
    }
}

/// How a branch's commits relate to the rest of the repo, for delete-safety
/// messaging in the TUI. Computed offline (no fetch): from ancestry against the
/// base/default branch, a recorded merged PR, and whether the configured
/// upstream's tracking ref is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeState {
    /// Fully merged, so deletion is safe. `into` names the ref it merged into
    /// (e.g. `main`); `None` means only a merged PR proves it (a squash/rebase
    /// merge, whose commit hash differs so ancestry cannot confirm it).
    Merged {
        /// The ref the branch merged into, or `None` when only a merged PR
        /// proves the merge.
        into: Option<String>,
    },
    /// An upstream was configured but its remote-tracking ref is gone and the
    /// merge could not be confirmed — most likely merged with the remote branch
    /// auto-deleted afterwards.
    UpstreamGone,
    /// No upstream was ever configured and the branch is not merged: genuinely
    /// local-only work that would be lost on deletion.
    NoUpstreamLocal,
    /// A live upstream exists; the ahead/behind counts carry the detail.
    Tracked,
}

/// Tip-commit metadata for display (spec §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Commit {
    /// Short commit hash (honoring `core.abbrev`).
    pub hash: String,
    /// Commit subject (first line of the message).
    pub subject: String,
    /// Author name.
    pub author: String,
    /// Author timestamp as an ISO-8601 UTC string (e.g. `2024-01-15T10:30:00Z`).
    pub timestamp: String,
}

/// A recorded pull request (spec §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pr {
    /// PR number.
    pub number: u64,
    /// PR state.
    pub state: PrState,
    /// PR title.
    pub title: String,
}

/// Pull-request state, mirroring `gh` (spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    /// An open PR.
    Open,
    /// A closed (unmerged) PR.
    Closed,
    /// A merged PR.
    Merged,
    /// A draft PR.
    Draft,
}

impl PrState {
    /// The lowercase string form (matches the JSON serialization).
    pub fn as_str(self) -> &'static str {
        match self {
            PrState::Open => "open",
            PrState::Closed => "closed",
            PrState::Merged => "merged",
            PrState::Draft => "draft",
        }
    }

    /// Parses a lowercase state string, or `None` if unknown.
    pub fn parse(s: &str) -> Option<PrState> {
        Some(match s {
            "open" => PrState::Open,
            "closed" => PrState::Closed,
            "merged" => PrState::Merged,
            "draft" => PrState::Draft,
            _ => return None,
        })
    }
}

/// The `remove` result object: the worktree row plus a `removed` flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemovedResult {
    /// The removed worktree's row, flattened into this object.
    #[serde(flatten)]
    pub worktree: Worktree,
    /// Always `true` (the worktree was removed).
    pub removed: bool,
}

/// A field to sort `wt list` by (spec §7 `--sort`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    /// Sort by branch name (the default).
    Branch,
    /// Modified/staged first, then untracked-only, then clean.
    Dirty,
    /// Sort by ahead count.
    Ahead,
    /// Sort by behind count.
    Behind,
    /// Most-recent commit first.
    Activity,
    /// Sort by path.
    Path,
}

impl SortKey {
    /// Parses a sort field name, or `None` if unknown.
    pub fn parse(name: &str) -> Option<SortKey> {
        Some(match name {
            "branch" => SortKey::Branch,
            "dirty" => SortKey::Dirty,
            "ahead" => SortKey::Ahead,
            "behind" => SortKey::Behind,
            "activity" => SortKey::Activity,
            "path" => SortKey::Path,
            _ => return None,
        })
    }
}

/// A sort field plus direction (spec §7; a `-` prefix means descending).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    /// The field to sort by.
    pub key: SortKey,
    /// Whether to sort in descending order.
    pub descending: bool,
}

impl Default for SortSpec {
    fn default() -> Self {
        SortSpec {
            key: SortKey::Branch,
            descending: false,
        }
    }
}

impl SortSpec {
    /// Parses a `--sort` argument such as `branch`, `ahead`, or `-ahead`.
    pub fn parse(value: &str) -> Result<SortSpec> {
        let (descending, name) = match value.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, value),
        };
        let key = SortKey::parse(name)
            .ok_or_else(|| Error::usage(format!("unknown sort field: {name:?}")))?;
        Ok(SortSpec { key, descending })
    }
}

/// A `wt list` display column (spec §11 `list.columns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    /// Status marker (`*`/`!`/`~`/space).
    Status,
    /// Dirty marker (`M`/`?`).
    Dirty,
    /// Branch name.
    Branch,
    /// Path relative to the repo root.
    Path,
    /// Ahead/behind counts.
    AheadBehind,
    /// Commit summary.
    Commit,
    /// PR number and state.
    Pr,
}

impl Column {
    /// The full, ordered set of columns (the default `list.columns`).
    pub const ALL: [Column; 7] = [
        Column::Status,
        Column::Dirty,
        Column::Branch,
        Column::Path,
        Column::AheadBehind,
        Column::Commit,
        Column::Pr,
    ];

    /// Parses a column identifier, or `None` if unknown.
    pub fn parse(identifier: &str) -> Option<Column> {
        Some(match identifier {
            "status" => Column::Status,
            "dirty" => Column::Dirty,
            "branch" => Column::Branch,
            "path" => Column::Path,
            "ahead-behind" => Column::AheadBehind,
            "commit" => Column::Commit,
            "pr" => Column::Pr,
            _ => return None,
        })
    }

    /// The identifier string for this column.
    pub fn identifier(self) -> &'static str {
        match self {
            Column::Status => "status",
            Column::Dirty => "dirty",
            Column::Branch => "branch",
            Column::Path => "path",
            Column::AheadBehind => "ahead-behind",
            Column::Commit => "commit",
            Column::Pr => "pr",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact §7 schema example.
    const SPEC_EXAMPLE: &str = r#"{
        "schema_version": 1,
        "path": "/absolute/path",
        "branch": "feature/login",
        "slug": "feature-login",
        "is_current": true,
        "is_main": false,
        "is_missing": false,
        "is_detached": false,
        "dirty": true,
        "has_untracked": false,
        "ahead": 2,
        "behind": 0,
        "upstream": "origin/feature/login",
        "base_ref": "main",
        "commit": {
            "hash": "abc1234",
            "subject": "Add login page",
            "author": "Alice",
            "timestamp": "2024-01-15T10:30:00Z"
        },
        "pr": { "number": 42, "state": "open", "title": "Add login page" }
    }"#;

    fn spec_example_worktree() -> Worktree {
        Worktree {
            schema_version: 1,
            path: PathBuf::from("/absolute/path"),
            branch: Some("feature/login".into()),
            slug: Some("feature-login".into()),
            is_current: true,
            is_main: false,
            is_missing: false,
            is_detached: false,
            dirty: Some(true),
            has_untracked: Some(false),
            ahead: Some(2),
            behind: Some(0),
            upstream: Some("origin/feature/login".into()),
            base_ref: Some("main".into()),
            commit: Some(Commit {
                hash: "abc1234".into(),
                subject: "Add login page".into(),
                author: "Alice".into(),
                timestamp: "2024-01-15T10:30:00Z".into(),
            }),
            pr: Some(Pr {
                number: 42,
                state: PrState::Open,
                title: "Add login page".into(),
            }),
            has_worktree: true,
            recent_commits: Vec::new(),
            pr_url: None,
            merge_state: None,
        }
    }

    #[test]
    fn serializes_to_spec_schema() {
        let got: serde_json::Value = serde_json::to_value(spec_example_worktree()).unwrap();
        let want: serde_json::Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn behind_zero_is_not_null() {
        let v = serde_json::to_value(spec_example_worktree()).unwrap();
        assert_eq!(v["behind"], serde_json::json!(0));
        assert!(!v["behind"].is_null());
    }

    #[test]
    fn missing_worktree_nulls_working_tree_fields() {
        let mut wt = Worktree::new(PathBuf::from("/gone"));
        wt.branch = Some("feature/x".into());
        wt.slug = Some("feature-x".into());
        wt.is_missing = true;
        wt.base_ref = Some("main".into());
        let v = serde_json::to_value(&wt).unwrap();
        assert!(v["dirty"].is_null());
        assert!(v["has_untracked"].is_null());
        assert!(v["ahead"].is_null());
        assert!(v["behind"].is_null());
        assert!(v["commit"].is_null());
        // Admin-derived fields remain populated.
        assert_eq!(v["branch"], serde_json::json!("feature/x"));
        assert_eq!(v["base_ref"], serde_json::json!("main"));
        assert_eq!(v["is_missing"], serde_json::json!(true));
    }

    #[test]
    fn has_worktree_defaults_true_and_is_not_serialized() {
        // A fresh row is a real worktree, and the TUI-only flag never leaks into
        // the stable §7 JSON schema (issue #47).
        let wt = Worktree::new(PathBuf::from("/r"));
        assert!(wt.has_worktree);
        let v = serde_json::to_value(&wt).unwrap();
        assert!(v.get("has_worktree").is_none());
    }

    #[test]
    fn detached_head_has_null_branch() {
        let mut wt = Worktree::new(PathBuf::from("/d"));
        wt.is_detached = true;
        let v = serde_json::to_value(&wt).unwrap();
        assert!(v["branch"].is_null());
        assert!(v["slug"].is_null());
        assert_eq!(v["is_detached"], serde_json::json!(true));
    }

    #[test]
    fn no_upstream_nulls_ahead_behind() {
        let mut wt = Worktree::new(PathBuf::from("/n"));
        wt.branch = Some("topic".into());
        let v = serde_json::to_value(&wt).unwrap();
        assert!(v["ahead"].is_null());
        assert!(v["behind"].is_null());
        assert!(v["upstream"].is_null());
        assert!(v["pr"].is_null());
    }

    #[test]
    fn pr_states_serialize_lowercase() {
        for (state, text) in [
            (PrState::Open, "open"),
            (PrState::Closed, "closed"),
            (PrState::Merged, "merged"),
            (PrState::Draft, "draft"),
        ] {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(text)
            );
            assert_eq!(state.as_str(), text);
            assert_eq!(PrState::parse(text), Some(state));
        }
        assert_eq!(PrState::parse("bogus"), None);
    }

    #[test]
    fn json_line_is_single_line() {
        let line = spec_example_worktree().to_json_line().unwrap();
        assert!(!line.contains('\n'));
        assert!(line.starts_with('{') && line.ends_with('}'));
    }

    #[test]
    fn removed_result_flattens_worktree_plus_flag() {
        let result = RemovedResult {
            worktree: Worktree::new(PathBuf::from("/x")),
            removed: true,
        };
        let v = serde_json::to_value(&result).unwrap();
        assert_eq!(v["removed"], serde_json::json!(true));
        assert_eq!(v["path"], serde_json::json!("/x"));
        assert_eq!(v["schema_version"], serde_json::json!(1));
    }

    #[test]
    fn sort_spec_parsing() {
        assert_eq!(SortSpec::default().key, SortKey::Branch);
        assert!(!SortSpec::default().descending);
        assert_eq!(
            SortSpec::parse("ahead").unwrap(),
            SortSpec {
                key: SortKey::Ahead,
                descending: false
            }
        );
        let desc = SortSpec::parse("-activity").unwrap();
        assert_eq!(desc.key, SortKey::Activity);
        assert!(desc.descending);
        for f in ["branch", "dirty", "ahead", "behind", "activity", "path"] {
            assert!(SortSpec::parse(f).is_ok());
        }
        let err = SortSpec::parse("bogus").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn column_parse_roundtrip() {
        for col in Column::ALL {
            assert_eq!(Column::parse(col.identifier()), Some(col));
        }
        assert_eq!(Column::parse("bogus"), None);
        assert_eq!(Column::ALL.len(), 7);
    }
}
