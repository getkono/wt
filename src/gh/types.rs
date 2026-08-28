//! `gh` JSON shapes and their mapping to the domain model (spec §4).

use serde::{Deserialize, Serialize};

use crate::model::PrState;

/// A GitHub issue label.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct IssueLabel {
    /// Label name.
    #[serde(default)]
    pub name: String,
}

/// A GitHub issue type (the organization-defined "type" field, distinct from
/// labels).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct IssueType {
    /// Issue type name.
    #[serde(default)]
    pub name: String,
}

/// A GitHub issue milestone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct IssueMilestone {
    /// Milestone title.
    #[serde(default)]
    pub title: String,
}

/// An open issue as returned by `gh issue list --json ...`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IssueSummary {
    /// Issue number.
    pub number: u64,
    /// Issue title.
    pub title: String,
    /// Issue state (`OPEN`/`CLOSED`).
    pub state: String,
    /// Labels attached to the issue.
    #[serde(default)]
    pub labels: Vec<IssueLabel>,
    /// Optional organization-defined issue type.
    #[serde(rename = "issueType", default)]
    pub issue_type: Option<IssueType>,
    /// Optional milestone.
    #[serde(default)]
    pub milestone: Option<IssueMilestone>,
    /// ISO-8601 creation time.
    #[serde(rename = "createdAt", default)]
    pub created_at: String,
    /// Issue web URL.
    #[serde(default)]
    pub url: String,
}

/// A full issue as returned by `gh issue view <target> --json ...`.
///
/// This carries only the token-efficient issue context the generation step
/// needs; comments, assignees, reactions and project bookkeeping are
/// deliberately not requested.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct IssueView {
    /// Issue number.
    pub number: u64,
    /// Issue title.
    pub title: String,
    /// Issue body.
    #[serde(default)]
    pub body: String,
    /// Issue state (`OPEN`/`CLOSED`).
    pub state: String,
    /// Labels attached to the issue.
    #[serde(default)]
    pub labels: Vec<IssueLabel>,
    /// Optional organization-defined issue type.
    #[serde(rename = "issueType", default)]
    pub issue_type: Option<IssueType>,
    /// Optional milestone.
    #[serde(default)]
    pub milestone: Option<IssueMilestone>,
    /// ISO-8601 creation time.
    #[serde(rename = "createdAt", default)]
    pub created_at: String,
    /// ISO-8601 update time.
    #[serde(rename = "updatedAt", default)]
    pub updated_at: String,
    /// Issue web URL.
    #[serde(default)]
    pub url: String,
}

/// A PR author (`{ "login": ... }`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Author {
    /// The author's login.
    #[serde(default)]
    pub login: String,
}

/// A PR as returned by `gh pr list --json ...`.
#[derive(Debug, Clone, Deserialize)]
pub struct PrSummary {
    /// PR number.
    pub number: u64,
    /// PR title.
    pub title: String,
    /// PR author.
    #[serde(default)]
    pub author: Author,
    /// PR state (`OPEN`/`CLOSED`/`MERGED`).
    pub state: String,
    /// Whether the PR is a draft.
    #[serde(rename = "isDraft", default)]
    pub is_draft: bool,
    /// The PR's head branch name.
    #[serde(rename = "headRefName", default)]
    pub head_ref_name: String,
    /// ISO-8601 creation time.
    #[serde(rename = "createdAt", default)]
    pub created_at: String,
}

impl PrSummary {
    /// The mapped [`PrState`].
    pub fn pr_state(&self) -> PrState {
        pr_state(&self.state, self.is_draft)
    }
}

/// A PR as returned by `gh pr view <target> --json ...`.
#[derive(Debug, Clone, Deserialize)]
pub struct PrView {
    /// PR number.
    pub number: u64,
    /// PR title.
    pub title: String,
    /// PR state (`OPEN`/`CLOSED`/`MERGED`).
    pub state: String,
    /// Whether the PR is a draft.
    #[serde(rename = "isDraft", default)]
    pub is_draft: bool,
    /// The PR's head branch name (the local branch the worktree checks out).
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    /// The PR's base branch name (recorded as the worktree's base ref).
    #[serde(rename = "baseRefName")]
    pub base_ref_name: String,
    /// The PR's web URL (shown in the TUI detail pane).
    #[serde(default)]
    pub url: String,
}

impl PrView {
    /// The mapped [`PrState`].
    pub fn pr_state(&self) -> PrState {
        pr_state(&self.state, self.is_draft)
    }
}

/// An open PR found for a branch, as returned by
/// `gh pr list --head <branch> --json number,url,state,isDraft`.
///
/// This is `wt`'s local mirror of `sendit::ExistingPr`; it is converted to the
/// `sendit` type when assembling a `PrContext` for the compose/submit flow.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpenPr {
    /// PR number.
    pub number: u64,
    /// PR web URL.
    #[serde(default)]
    pub url: String,
    /// PR state (`OPEN`/`CLOSED`/`MERGED`).
    pub state: String,
    /// Whether the PR is a draft.
    #[serde(rename = "isDraft", default)]
    pub is_draft: bool,
}

/// Extract the default branch name from `gh repo view --json defaultBranchRef`
/// output, or `None` if it is absent or unparseable (kept non-fatal so trunk
/// detection can fall back to local git state).
pub(crate) fn parse_default_branch(json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Ref {
        name: String,
    }
    #[derive(Deserialize)]
    struct View {
        #[serde(rename = "defaultBranchRef")]
        default_branch_ref: Option<Ref>,
    }
    let view: View = serde_json::from_str(json).ok()?;
    view.default_branch_ref.map(|r| r.name)
}

/// Maps a `gh` state string + draft flag to a [`PrState`].
pub fn pr_state(state: &str, is_draft: bool) -> PrState {
    if is_draft && state.eq_ignore_ascii_case("open") {
        return PrState::Draft;
    }
    match state.to_ascii_lowercase().as_str() {
        "closed" => PrState::Closed,
        "merged" => PrState::Merged,
        _ => PrState::Open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_issue_list_json() {
        let json = r#"[
            {"number": 12, "title": "Broken login", "state": "OPEN",
             "labels": [{"name": "bug"}], "issueType": {"name": "Bug"},
             "milestone": {"title": "v2"}, "createdAt": "2024-01-15T10:30:00Z",
             "url": "https://github.com/o/r/issues/12"},
            {"number": 13, "title": "Bare", "state": "OPEN"}
        ]"#;
        let issues: Vec<IssueSummary> = serde_json::from_str(json).unwrap();
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 12);
        assert_eq!(issues[0].labels[0].name, "bug");
        assert_eq!(issues[0].issue_type.as_ref().unwrap().name, "Bug");
        assert_eq!(issues[0].milestone.as_ref().unwrap().title, "v2");
        // Every optional field defaults, so a sparse `gh` response still parses.
        assert!(issues[1].labels.is_empty());
        assert_eq!(issues[1].issue_type, None);
        assert_eq!(issues[1].milestone, None);
        assert_eq!(issues[1].url, "");
    }

    #[test]
    fn parses_issue_view_json() {
        let json = r#"{"number": 7, "title": "Add login", "body": "Please add it.",
            "state": "OPEN", "labels": [{"name": "enhancement"}],
            "issueType": null, "milestone": null,
            "createdAt": "2024-01-15T10:30:00Z", "updatedAt": "2024-02-01T09:00:00Z",
            "url": "https://github.com/o/r/issues/7"}"#;
        let issue: IssueView = serde_json::from_str(json).unwrap();
        assert_eq!(issue.number, 7);
        assert_eq!(issue.body, "Please add it.");
        assert_eq!(issue.labels[0].name, "enhancement");
        assert_eq!(issue.issue_type, None);
        assert_eq!(issue.url, "https://github.com/o/r/issues/7");
    }

    #[test]
    fn issue_view_tolerates_a_body_only_response() {
        // `gh` omits fields the repository does not use; only number/title/state
        // are required, so the rest must not make parsing fail.
        let issue: IssueView =
            serde_json::from_str(r#"{"number": 1, "title": "T", "state": "OPEN"}"#).unwrap();
        assert_eq!(issue.body, "");
        assert_eq!(issue.updated_at, "");
    }

    #[test]
    fn parses_pr_list_json() {
        let json = r#"[
            {"number": 42, "title": "Add login", "author": {"login": "alice"},
             "state": "OPEN", "isDraft": false, "headRefName": "feature/login",
             "createdAt": "2024-01-15T10:30:00Z"},
            {"number": 7, "title": "WIP", "author": {"login": "bob"},
             "state": "OPEN", "isDraft": true, "headRefName": "wip"}
        ]"#;
        let prs: Vec<PrSummary> = serde_json::from_str(json).unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!(prs[0].number, 42);
        assert_eq!(prs[0].author.login, "alice");
        assert_eq!(prs[0].pr_state(), PrState::Open);
        assert_eq!(prs[1].pr_state(), PrState::Draft); // open + draft
    }

    #[test]
    fn parses_pr_view_json() {
        let json = r#"{"number": 5, "title": "Fix", "state": "MERGED", "isDraft": false,
            "headRefName": "fork-branch", "baseRefName": "main"}"#;
        let view: PrView = serde_json::from_str(json).unwrap();
        assert_eq!(view.number, 5);
        assert_eq!(view.head_ref_name, "fork-branch");
        assert_eq!(view.base_ref_name, "main");
        assert_eq!(view.pr_state(), PrState::Merged);
    }

    #[test]
    fn state_mapping() {
        assert_eq!(pr_state("OPEN", false), PrState::Open);
        assert_eq!(pr_state("OPEN", true), PrState::Draft);
        assert_eq!(pr_state("CLOSED", false), PrState::Closed);
        assert_eq!(pr_state("MERGED", false), PrState::Merged);
        assert_eq!(pr_state("CLOSED", true), PrState::Closed); // draft only matters for open
    }

    #[test]
    fn parses_open_pr_list() {
        let json = r#"[{"number": 77, "url": "https://github.com/o/r/pull/77",
            "state": "OPEN", "isDraft": true}]"#;
        let prs: Vec<OpenPr> = serde_json::from_str(json).unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 77);
        assert_eq!(prs[0].url, "https://github.com/o/r/pull/77");
        assert!(prs[0].is_draft);
    }

    #[test]
    fn parses_default_branch() {
        assert_eq!(
            parse_default_branch(r#"{"defaultBranchRef": {"name": "main"}}"#),
            Some("main".to_string())
        );
        // Null ref (e.g. empty repo) and garbage both yield None.
        assert_eq!(parse_default_branch(r#"{"defaultBranchRef": null}"#), None);
        assert_eq!(parse_default_branch("not json"), None);
    }
}
