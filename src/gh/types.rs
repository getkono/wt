//! `gh` JSON shapes and their mapping to the domain model (spec §4).

use serde::Deserialize;

use crate::model::PrState;

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
}
