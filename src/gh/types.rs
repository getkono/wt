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

/// A pull request's head repository (`{ "name": ... }`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct HeadRepository {
    /// The repository's name, without its owner.
    #[serde(default)]
    pub name: String,
}

/// A merged PR found for a head branch, as returned by
/// `gh pr list --head <branch> --state merged --json ...`.
///
/// `wt prune` reads it only to find a candidate head commit; every claim it
/// makes is then proven against local git objects, so nothing here is trusted
/// on its own.
#[derive(Debug, Clone, Deserialize)]
pub struct MergedPr {
    /// PR number.
    pub number: u64,
    /// PR state (`MERGED` for every PR this query returns).
    #[serde(default)]
    pub state: String,
    /// The PR's head branch name.
    #[serde(rename = "headRefName", default)]
    pub head_ref_name: String,
    /// The commit the PR's head pointed at when it merged.
    #[serde(rename = "headRefOid", default)]
    pub head_ref_oid: String,
    /// The PR's base branch name.
    #[serde(rename = "baseRefName", default)]
    pub base_ref_name: String,
    /// The head repository, or `None` once it was deleted.
    #[serde(rename = "headRepository", default)]
    pub head_repository: Option<HeadRepository>,
    /// The head repository's owner, or `None` once it was deleted.
    #[serde(rename = "headRepositoryOwner", default)]
    pub head_repository_owner: Option<Author>,
    /// Whether the head repository differs from the base repository (a fork).
    #[serde(rename = "isCrossRepository", default)]
    pub is_cross_repository: bool,
}

impl MergedPr {
    /// The head repository as `(owner, name)`, when both are known.
    pub fn head_slug(&self) -> Option<(&str, &str)> {
        let owner = self.head_repository_owner.as_ref()?.login.as_str();
        let name = self.head_repository.as_ref()?.name.as_str();
        (!owner.is_empty() && !name.is_empty()).then_some((owner, name))
    }
}

/// The `(owner, name)` a hosted remote URL names, or `None` for anything else
/// (a local path, a `file://` URL, or a URL with no `owner/name` path).
///
/// Accepts `scheme://[user@]host[:port]/owner/name[.git][/]` for the network
/// schemes and the scp-like `[user@]host:owner/name[.git]`. The host is not
/// checked, so a GitHub Enterprise remote parses as well as `github.com`.
pub fn remote_slug(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let path = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        let (host, path) = rest.split_once('/')?;
        if host.is_empty() {
            return None;
        }
        path
    } else {
        // scp-like syntax: a `:` before any `/`, and a non-empty host.
        let colon = url.find(':')?;
        if url.find('/').is_some_and(|slash| slash < colon) {
            return None;
        }
        let (host, path) = (&url[..colon], &url[colon + 1..]);
        let host = host.rsplit('@').next().unwrap_or(host);
        // A one-letter "host" is a Windows drive (`C:/repo`), which git reads
        // as a local path too.
        if host.len() < 2 {
            return None;
        }
        path
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    match path.split('/').collect::<Vec<_>>()[..] {
        [owner, name] if !owner.is_empty() && !name.is_empty() => {
            Some((owner.to_string(), name.to_string()))
        }
        _ => None,
    }
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
    fn parses_merged_pr_list() {
        let json = r#"[{"baseRefName":"master","headRefName":"fix/x",
            "headRefOid":"b3dc19c6e8076554ff24e99ee7ebce40a86ed985",
            "headRepository":{"id":"R_1","name":"wt","nameWithOwner":"getkono/wt"},
            "headRepositoryOwner":{"id":"O_1","login":"getkono"},
            "isCrossRepository":false,"number":131,"state":"MERGED"},
            {"number": 9, "headRepository": null, "headRepositoryOwner": null}]"#;
        let prs: Vec<MergedPr> = serde_json::from_str(json).unwrap();
        assert_eq!(prs[0].number, 131);
        assert_eq!(prs[0].head_ref_name, "fix/x");
        assert_eq!(prs[0].base_ref_name, "master");
        assert_eq!(prs[0].head_slug(), Some(("getkono", "wt")));
        assert!(!prs[0].is_cross_repository);
        // A deleted head repository leaves nothing to match against.
        assert_eq!(prs[1].head_slug(), None);
    }

    #[test]
    fn remote_slug_reads_hosted_urls_only() {
        let slug = |url| remote_slug(url);
        let ow = Some(("o".to_string(), "r".to_string()));
        assert_eq!(slug("https://github.com/o/r.git"), ow);
        assert_eq!(slug("https://github.com/o/r"), ow);
        assert_eq!(slug("https://user@ghe.example.com:8443/o/r/"), ow);
        assert_eq!(slug("ssh://git@github.com/o/r.git"), ow);
        assert_eq!(slug("git@github.com:o/r.git"), ow);
        assert_eq!(slug("github.com:o/r"), ow);
        // Local paths and anything without exactly `owner/name` are not hosted.
        assert_eq!(slug("/tmp/remote.git"), None);
        assert_eq!(slug("./o/r"), None);
        assert_eq!(slug("file:///tmp/o/r.git"), None);
        assert_eq!(slug("C:/o/r"), None);
        assert_eq!(slug("https://github.com/o"), None);
        assert_eq!(slug("https://github.com/o/r/extra"), None);
        assert_eq!(slug("https:///o/r"), None);
        assert_eq!(slug("git@github.com:"), None);
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
