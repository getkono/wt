//! `wt issue` — create (or reuse) a worktree for a GitHub issue, with a branch
//! name and implementation brief proposed by the generation agent.
//!
//! `wt` deliberately stops here: it fetches the issue, proposes a branch and
//! brief, creates the worktree, and records the link. It does **not** launch a
//! coding agent — running the work is karet's job over the Agent Client
//! Protocol (issue #100). What this command hands back is a prepared worktree
//! and the metadata to find it again.
//!
//! Generation is best-effort throughout. A model that returns nonsense, an agent
//! that is not installed, and an agent that hangs all degrade to a deterministic
//! fallback branch built from the issue's own labels and title (issue #98), so
//! **worktree creation never depends on model behaviour**.
//!
//! The `TYPE/{number}-SLUG` contract itself lives in [`crate::naming`], which is
//! pure and shared: the prompt fragment the model is given and the validator its
//! answer is checked against come from the same place and cannot drift
//! (issue #96).

use crate::agent::{AgentClient, AgentKind, AgentModel, AgentOptions, Effort};
use crate::cli::IssueArgs;
use crate::commands::{Session, confirm, finish_worktree, open_session, report_created};
use crate::config::{Config, wtconfig};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::gh::IssueView;
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{all_branches, default_base_ref, local_branches, validate_branch_name};
use crate::hooks::{HookContext, HookRunner};
use crate::naming::{self, BranchKind};
use crate::progress;
use crate::worktree::{
    CreateOptions, MetaUpdate, apply_meta, create_in, lock_repo, preview_target,
};

use super::Nav;

/// How long the generation agent gets before it is killed and the fallback is
/// used. Generous enough for a short structured answer, short enough that a
/// hung agent does not hold up a worktree.
const GENERATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The branch name and implementation brief for an issue worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssuePlan {
    /// The branch to create or reuse.
    pub(crate) branch: String,
    /// The implementation brief; empty when generation produced none.
    pub(crate) brief: String,
}

/// Runs `wt issue`.
pub(crate) fn run(cx: &mut Cx, hooks: &dyn HookRunner, args: &IssueArgs) -> Result<u8> {
    // Gate before anything costly. `wt issue` reviews values with the user, so
    // without a terminal it needs `--yes` — and finding that out *after* a
    // network round trip and an LLM call would be rude and slow.
    if !cx.err.is_tty() && !cx.assume_yes {
        return Err(Error::usage(
            "`wt issue` requires an interactive terminal or `--yes`",
        ));
    }

    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());

    let gh = cx.gh.clone();
    let target = args.target.clone();
    let issue = progress::run(&mut cx.err, "Fetching issue", move || {
        gh.view_issue(&dir, &target)
    })?;

    // An issue already linked to a branch reuses it, which costs no LLM call at
    // all — the common case of returning to work in progress.
    let linked = linked_branch(session.repo.gix(), issue.number)?;
    let mut plan = match &linked {
        Some(branch) => {
            if let Some(requested) = &args.branch
                && requested != branch
            {
                return Err(Error::usage(format!(
                    "issue #{} is already linked to branch {branch:?}; \
                     drop --branch or pass that branch",
                    issue.number
                )));
            }
            cx.err.line(&format!(
                "issue #{} is already linked to branch {branch:?}",
                issue.number
            ))?;
            IssuePlan {
                branch: branch.clone(),
                brief: args.brief.clone().unwrap_or_default(),
            }
        }
        None => resolve_plan(cx, &session, &issue, args)?,
    };

    let mut base = args
        .from
        .clone()
        .or_else(|| default_base_ref(session.repo.gix()));

    // Review the proposal. Fields the user pinned with a flag are not re-asked.
    if !cx.assume_yes {
        let branches = all_branches(session.repo.gix()).unwrap_or_default();
        if args.branch.is_none() && linked.is_none() {
            plan.branch = prompt_value(cx, "branch", &plan.branch)?;
        }
        if args.from.is_none() {
            base = prompt_base(cx, base.as_deref(), &branches)?;
        }
        if args.brief.is_none() {
            plan.brief = prompt_value(cx, "brief", &plan.brief)?;
        }
    }

    // A human-chosen name only has to be a legal git branch. The naming contract
    // exists to constrain the *model*; the issue link lives in git config, never
    // in the branch name, so an off-contract name still resolves back to its
    // issue and breaks nothing.
    validate_branch_name(&plan.branch).map_err(Error::usage)?;
    if naming::parse_and_validate(&plan.branch, issue.number).is_err() {
        cx.err.line(&format!(
            "note: {:?} does not follow TYPE/{}-SLUG; the issue link is recorded \
             in git config, so this still works",
            plan.branch, issue.number
        ))?;
    }

    let env = cx.env.clone();
    let preview = preview_target(&session.parts(&env), &plan.branch)?;
    let prompt = format!(
        "Issue #{}: {}\nBranch:  {}\nBase:    {}\nWorktree: {}\nBrief:   {}\n\
         Create this issue worktree? [y/N] ",
        issue.number,
        issue.title,
        plan.branch,
        base.as_deref().unwrap_or("(repository default)"),
        preview.display(),
        if plan.brief.trim().is_empty() {
            "(none)"
        } else {
            plan.brief.trim()
        },
    );
    if !confirm(cx, &prompt)? {
        cx.err.line("aborted: issue worktree was not created")?;
        return Ok(0);
    }

    let options = CreateOptions {
        branch: plan.branch.clone(),
        base,
        track: None,
        copy_from: args.copy_from.clone(),
        // The service never prompts; the interactive submodule policy is applied
        // below, exactly as `wt new` does it.
        init_submodules: false,
        seed_submodules: session.config.submodules_seed.is_enabled(),
        reflink: session.config.create_reflink.is_enabled(),
        no_hooks: args.no_hooks,
    };
    let created = create_in(&session.parts(&env), git.as_ref(), hooks, &options)?;
    report_created(cx, &created)?;
    if !created.reused {
        super::maybe_init_submodules_interactive(
            cx,
            git.as_ref(),
            &created.path,
            session.config.submodules_init,
            args.submodule_override(),
            !cx.assume_yes,
            session.config.submodules_seed.is_enabled(),
        )?;
    }

    link_issue(
        git.as_ref(),
        &session.primary_root,
        &created.branch,
        &issue,
        &plan.brief,
    )?;

    let ctx = HookContext {
        worktree_path: created.path.clone(),
        branch: created.branch.clone(),
        repo_root: session.primary_root.clone(),
        base_ref: created.base_ref.clone(),
        pr_number: None,
    };
    finish_worktree(
        cx,
        hooks,
        &created.path,
        &ctx,
        Nav {
            json: false,
            no_switch: args.no_switch,
            note: if created.reused {
                "reusing issue worktree at"
            } else {
                "created issue worktree at"
            },
            start: None,
        },
    )
}

/// Proposes a branch and brief for `issue`, never failing for a model reason.
///
/// Only a bad `--model`/`--effort` value returns `Err`; every generation failure
/// mode degrades instead:
///
/// | outcome | branch | brief |
/// | --- | --- | --- |
/// | valid | as generated | as generated |
/// | malformed branch | [`naming::fallback`] | kept — still usable |
/// | unavailable / timeout / unparseable | [`naming::fallback`] | empty |
fn resolve_plan(
    cx: &mut Cx,
    session: &Session,
    issue: &IssueView,
    args: &IssueArgs,
) -> Result<IssuePlan> {
    let fallback = naming::fallback(fallback_kind(issue), issue.number, &issue.title);

    // Nothing left to generate: both values were supplied.
    if let (Some(branch), Some(brief)) = (&args.branch, &args.brief) {
        cx.err
            .line("generation: skipped (branch and brief supplied)")?;
        return Ok(IssuePlan {
            branch: branch.clone(),
            brief: brief.clone(),
        });
    }

    let opts = resolve_generation_options(args, &session.config)?;
    let kind = session.config.agent_generation.provider;
    let agent = cx.agent.clone();
    let root = session.primary_root.clone();
    let prompt = build_generation_prompt(issue);
    let generated = progress::run(&mut cx.err, "Generating issue setup", move || {
        generate(agent.as_ref(), kind, &prompt, &root, &opts)
    });

    let (branch, brief) = match generated {
        Ok(plan) => match naming::parse_and_validate(&plan.branch, issue.number) {
            Ok(name) => (name.to_string(), plan.brief),
            // Only the branch was wrong; the brief is independent and still useful.
            Err(e) => {
                cx.err.line(&format!(
                    "note: generated branch {:?} rejected ({e}); using {fallback} instead",
                    plan.branch
                ))?;
                (fallback.to_string(), plan.brief)
            }
        },
        Err(e) => {
            cx.err.line(&format!(
                "note: could not generate issue setup ({e}); using {fallback} with no brief"
            ))?;
            (fallback.to_string(), String::new())
        }
    };

    Ok(IssuePlan {
        branch: args.branch.clone().unwrap_or(branch),
        brief: args.brief.clone().unwrap_or(brief),
    })
}

/// The branch kind implied by the issue's labels, then its issue type, then
/// `feat` (issue #98). Deterministic, so the fallback branch for a given issue
/// is always the same name.
fn fallback_kind(issue: &IssueView) -> BranchKind {
    issue
        .labels
        .iter()
        .map(|label| label.name.as_str())
        .chain(issue.issue_type.iter().map(|ty| ty.name.as_str()))
        .find_map(BranchKind::from_label)
        .unwrap_or(BranchKind::Feat)
}

/// Resolves the generation model and effort: a `--model`/`--effort` flag
/// overrides `[agent.generation]`. An unknown flag value is a usage error — the
/// one failure in this flow that is the user's to fix, so it is *not* swallowed
/// by the fallback.
fn resolve_generation_options(args: &IssueArgs, config: &Config) -> Result<AgentOptions> {
    let model = match &args.model {
        Some(m) => AgentModel::parse(m).ok_or_else(|| {
            Error::usage(format!(
                "unknown --model {m:?}; expected one of: opus, sonnet, haiku"
            ))
        })?,
        None => config.agent_generation.model,
    };
    let effort = match &args.effort {
        Some(e) => Effort::parse(e).ok_or_else(|| {
            Error::usage(format!(
                "unknown --effort {e:?}; expected one of: low, medium, high"
            ))
        })?,
        None => config.agent_generation.effort,
    };
    Ok(AgentOptions {
        model,
        effort,
        timeout: Some(GENERATION_TIMEOUT),
    })
}

/// The generation prompt.
///
/// The branch contract is [`naming::branch_rule`] verbatim, so the rule the
/// model is asked to follow is the same one [`naming::parse_and_validate`]
/// enforces. Only the issue fields `wt` actually fetched are included; comments,
/// assignees and project data are deliberately absent.
fn build_generation_prompt(issue: &IssueView) -> String {
    let labels = issue
        .labels
        .iter()
        .map(|label| label.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let issue_type = issue.issue_type.as_ref().map_or("", |ty| ty.name.as_str());
    let milestone = issue.milestone.as_ref().map_or("", |m| m.title.as_str());
    format!(
        "Generate setup values for a coding agent working on a GitHub issue.\n\
         Return only one JSON object with string fields `branch` and `brief`.\n\
         {}\n\
         Keep `brief` under 120 words and describe the implementation outcome and \
         validation, without inventing requirements.\n\n\
         Issue number: {}\nURL: {}\nTitle: {}\nType: {}\nLabels: {}\nMilestone: {}\nBody:\n{}",
        naming::branch_rule(issue.number),
        issue.number,
        issue.url,
        issue.title,
        issue_type,
        labels,
        milestone,
        issue.body,
    )
}

/// Runs the generation agent and parses its answer. Every failure here is
/// recoverable by the caller's fallback.
fn generate(
    agent: &dyn AgentClient,
    kind: AgentKind,
    prompt: &str,
    dir: &std::path::Path,
    opts: &AgentOptions,
) -> Result<IssuePlan> {
    let run = agent.run(kind, prompt, dir, opts)?;
    if run.is_error {
        return Err(Error::operation(format!(
            "generation agent reported an error: {}",
            run.result.trim()
        )));
    }
    parse_issue_plan(&run.result)
}

/// Parses the generated JSON object, tolerating a markdown code fence around it
/// (models add one often enough that rejecting it would waste a good answer).
fn parse_issue_plan(output: &str) -> Result<IssuePlan> {
    let trimmed = output.trim();
    let body = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .and_then(|rest| rest.trim_end().strip_suffix("```"))
        .unwrap_or(trimmed)
        .trim();

    #[derive(serde::Deserialize)]
    struct Raw {
        branch: String,
        #[serde(default)]
        brief: String,
    }
    let raw: Raw = serde_json::from_str(body).map_err(|e| {
        Error::operation(format!("generated output was not the expected JSON: {e}"))
    })?;
    Ok(IssuePlan {
        branch: raw.branch.trim().to_string(),
        brief: raw.brief.trim().to_string(),
    })
}

/// The local branch already linked to `issue_number`, if any. More than one is
/// ambiguous and refused rather than guessed at.
fn linked_branch(repo: &gix::Repository, issue_number: u64) -> Result<Option<String>> {
    let mut found: Vec<String> = local_branches(repo)
        .unwrap_or_default()
        .into_iter()
        .filter(|branch| wtconfig::read_meta(repo, branch).issue_number == Some(issue_number))
        .collect();
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => {
            found.sort();
            Err(Error::operation(format!(
                "issue #{issue_number} is linked to more than one branch: {}",
                found.join(", ")
            )))
        }
    }
}

/// Records the issue link for `branch` as a locked read-check-write.
///
/// Two traps this avoids. First, `create_in` wrote metadata through the `git`
/// subprocess but `gix` snapshots config at open, so the session's handle is
/// stale — the check has to run against a freshly discovered repository.
/// Second, this cannot go through `Workspace::write_meta`, which takes the lock
/// itself and so cannot enclose the preceding read; the advisory lock is neither
/// reentrant nor owner-aware, so taking it twice would deadlock until it times
/// out.
fn link_issue(
    git: &dyn GitCli,
    root: &std::path::Path,
    branch: &str,
    issue: &IssueView,
    brief: &str,
) -> Result<()> {
    let repo = Repo::discover(root)?;
    wtconfig::ensure_schema_supported(repo.gix())?;
    let _lock = lock_repo(root)?;

    let meta = wtconfig::read_meta(repo.gix(), branch);
    if let Some(other) = meta.issue_number
        && other != issue.number
    {
        return Err(Error::operation(format!(
            "branch {branch:?} is already linked to issue #{other}"
        )));
    }

    apply_meta(
        git,
        root,
        branch,
        &MetaUpdate {
            issue_number: Some(issue.number),
            issue_title: Some(issue.title.clone()),
            issue_url: Some(issue.url.clone()),
            // An empty brief leaves the key alone rather than recording a blank.
            issue_brief: (!brief.trim().is_empty()).then(|| brief.trim().to_string()),
            ..MetaUpdate::default()
        },
    )
}

/// Prompts for one field on stderr, keeping `current` when the line is blank.
fn prompt_value(cx: &mut Cx, label: &str, current: &str) -> Result<String> {
    let shown = if current.trim().is_empty() {
        "(none)".to_string()
    } else {
        current.trim().to_string()
    };
    cx.err.text(&format!("{label} [{shown}]: "))?;
    cx.err.flush()?;
    let line = cx.input.read_line()?;
    let typed = line.trim();
    Ok(if typed.is_empty() {
        current.to_string()
    } else {
        typed.to_string()
    })
}

/// Prompts for the base ref as a numbered menu of the repository's branches,
/// with the repository default and a free-form entry.
fn prompt_base(cx: &mut Cx, current: Option<&str>, branches: &[String]) -> Result<Option<String>> {
    let shown = current.unwrap_or("(repository default)");
    cx.err.line(&format!("base [{shown}]:"))?;
    for (index, branch) in branches.iter().enumerate() {
        let marker = if Some(branch.as_str()) == current {
            " (current)"
        } else {
            ""
        };
        cx.err.line(&format!("  {}) {branch}{marker}", index + 1))?;
    }
    cx.err
        .line("  or type a ref; blank keeps the current value")?;
    cx.err.text("base: ")?;
    cx.err.flush()?;

    let line = cx.input.read_line()?;
    let typed = line.trim();
    if typed.is_empty() {
        return Ok(current.map(str::to_string));
    }
    // A bare number selects from the menu; anything else is a ref, so a branch
    // literally named "2" is still reachable by typing it at a non-menu prompt.
    if let Ok(choice) = typed.parse::<usize>() {
        return match branches.get(choice.wrapping_sub(1)) {
            Some(branch) => Ok(Some(branch.clone())),
            None => Err(Error::usage(format!(
                "{choice} is not one of the {} listed branches",
                branches.len()
            ))),
        };
    }
    Ok(Some(typed.to_string()))
}

/// The worktree path for `branch`, for tests that assert what was created.
#[cfg(test)]
fn target_of(session: &Session, env: &crate::cx::Env, branch: &str) -> std::path::PathBuf {
    preview_target(&session.parts(env), branch).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cx::Stream;
    use crate::gh::{IssueLabel, IssueType};
    use crate::git::cli::RealGit;
    use crate::hooks::RealHookRunner;
    use crate::testutil::{CannedInput, FakeAgent, FakeGh, SharedBuf, TestRepo, test_cx};
    use std::sync::Arc;

    fn issue() -> IssueView {
        IssueView {
            number: 7,
            title: "Add login page".into(),
            body: "Users need to sign in.".into(),
            state: "OPEN".into(),
            labels: vec![IssueLabel {
                name: "enhancement".into(),
            }],
            issue_type: None,
            milestone: None,
            created_at: String::new(),
            updated_at: String::new(),
            url: "https://github.com/o/r/issues/7".into(),
        }
    }

    fn args() -> IssueArgs {
        IssueArgs {
            target: "7".into(),
            branch: None,
            from: None,
            brief: None,
            model: None,
            effort: None,
            no_switch: true,
            no_hooks: true,
            copy_from: None,
            init_submodules: false,
            no_init_submodules: true,
        }
    }

    /// A repo plus a `--yes` context wired to the given agent output.
    fn run_with_agent(
        agent: FakeAgent,
        args: &IssueArgs,
    ) -> (TestRepo, crate::testutil::TestCx, u8) {
        let repo = TestRepo::init();
        let mut t = test_cx(&[], repo.root().to_str().unwrap_or_default());
        t.cx.assume_yes = true;
        t.cx.gh = Arc::new(FakeGh::with_issue(issue()));
        t.cx.agent = Arc::new(agent);
        let code = run(&mut t.cx, &RealHookRunner, args).unwrap();
        (repo, t, code)
    }

    /// The `wt.<branch>.*` metadata recorded for `branch`.
    fn meta(repo: &TestRepo, branch: &str) -> crate::config::wtconfig::WtMeta {
        let r = Repo::discover(repo.root()).unwrap();
        wtconfig::read_meta(r.gix(), branch)
    }

    fn branches(repo: &TestRepo) -> Vec<String> {
        let r = Repo::discover(repo.root()).unwrap();
        local_branches(r.gix()).unwrap_or_default()
    }

    #[test]
    fn generation_creates_the_branch_and_links_the_issue() {
        let (repo, t, code) = run_with_agent(
            FakeAgent::drafting(r#"{"branch":"feat/7-add-login","brief":"Wire up the form."}"#),
            &args(),
        );
        assert_eq!(code, 0);
        assert!(branches(&repo).iter().any(|b| b == "feat/7-add-login"));
        let recorded = meta(&repo, "feat/7-add-login");
        assert_eq!(recorded.issue_number, Some(7));
        assert_eq!(recorded.issue_title.as_deref(), Some("Add login page"));
        assert_eq!(
            recorded.issue_url.as_deref(),
            Some("https://github.com/o/r/issues/7")
        );
        assert_eq!(recorded.issue_brief.as_deref(), Some("Wire up the form."));
        assert!(t.err.contents().contains("created issue worktree at"));
    }

    #[test]
    fn a_malformed_branch_falls_back_but_keeps_the_brief() {
        // Only the branch violated the contract; the brief is independent.
        let (repo, t, _) = run_with_agent(
            FakeAgent::drafting(r#"{"branch":"NOT A BRANCH","brief":"Wire up the form."}"#),
            &args(),
        );
        assert!(branches(&repo).iter().any(|b| b == "feat/7-add-login-page"));
        assert_eq!(
            meta(&repo, "feat/7-add-login-page").issue_brief.as_deref(),
            Some("Wire up the form.")
        );
        assert!(
            t.err.contents().contains("rejected"),
            "{}",
            t.err.contents()
        );
    }

    #[test]
    fn garbage_output_still_produces_a_valid_branch_and_worktree() {
        // Issue #98's headline requirement: a flaky generation call must not be
        // able to block worktree creation.
        let (repo, t, code) =
            run_with_agent(FakeAgent::drafting("I'm afraid I can't do that"), &args());
        assert_eq!(code, 0);
        assert!(branches(&repo).iter().any(|b| b == "feat/7-add-login-page"));
        assert_eq!(meta(&repo, "feat/7-add-login-page").issue_number, Some(7));
        // A failed generation leaves no brief rather than a bogus one.
        assert_eq!(meta(&repo, "feat/7-add-login-page").issue_brief, None);
        assert!(t.err.contents().contains("could not generate"));
    }

    #[test]
    fn an_unavailable_agent_falls_back() {
        let (repo, _, code) = run_with_agent(FakeAgent::unavailable(), &args());
        assert_eq!(code, 0);
        assert!(branches(&repo).iter().any(|b| b == "feat/7-add-login-page"));
    }

    #[test]
    fn a_timed_out_agent_falls_back() {
        let (repo, t, code) = run_with_agent(FakeAgent::timing_out(), &args());
        assert_eq!(code, 0);
        assert!(branches(&repo).iter().any(|b| b == "feat/7-add-login-page"));
        assert!(t.err.contents().contains("did not respond"));
    }

    #[test]
    fn an_erroring_agent_falls_back() {
        let (repo, _, code) = run_with_agent(FakeAgent::erroring("rate limited"), &args());
        assert_eq!(code, 0);
        assert!(branches(&repo).iter().any(|b| b == "feat/7-add-login-page"));
    }

    #[test]
    fn explicit_branch_and_brief_skip_generation_entirely() {
        let mut a = args();
        a.branch = Some("fix/7-mine".into());
        a.brief = Some("Do the thing.".into());
        // An unavailable agent proves no generation call was attempted.
        let (repo, t, code) = run_with_agent(FakeAgent::unavailable(), &a);
        assert_eq!(code, 0);
        assert!(branches(&repo).iter().any(|b| b == "fix/7-mine"));
        assert_eq!(
            meta(&repo, "fix/7-mine").issue_brief.as_deref(),
            Some("Do the thing.")
        );
        assert!(t.err.contents().contains("generation: skipped"));
    }

    #[test]
    fn fallback_kind_prefers_labels_then_type_then_feat() {
        let mut i = issue();
        i.labels = vec![IssueLabel { name: "bug".into() }];
        assert_eq!(fallback_kind(&i), BranchKind::Fix);

        // Case-insensitive, and unknown labels fall through to the next source.
        i.labels = vec![IssueLabel {
            name: "Documentation".into(),
        }];
        assert_eq!(fallback_kind(&i), BranchKind::Docs);

        i.labels = vec![IssueLabel {
            name: "help wanted".into(),
        }];
        i.issue_type = Some(IssueType {
            name: "Feature".into(),
        });
        assert_eq!(fallback_kind(&i), BranchKind::Feat);

        i.labels = Vec::new();
        i.issue_type = None;
        assert_eq!(fallback_kind(&i), BranchKind::Feat);
    }

    #[test]
    fn an_already_linked_branch_is_reused_without_generation() {
        let repo = TestRepo::init();
        repo.git(&["branch", "feat/7-existing"]);
        wtconfig::write_issue_number(&RealGit, repo.root(), "feat/7-existing", 7).unwrap();

        let mut t = test_cx(&[], repo.root().to_str().unwrap_or_default());
        t.cx.assume_yes = true;
        t.cx.gh = Arc::new(FakeGh::with_issue(issue()));
        // Unavailable: reaching generation at all would fail this expectation.
        t.cx.agent = Arc::new(FakeAgent::unavailable());
        let code = run(&mut t.cx, &RealHookRunner, &args()).unwrap();

        assert_eq!(code, 0);
        assert!(t.err.contents().contains("already linked to branch"));
        assert!(!t.err.contents().contains("could not generate"));
    }

    #[test]
    fn a_branch_flag_conflicting_with_the_link_is_a_usage_error() {
        let repo = TestRepo::init();
        repo.git(&["branch", "feat/7-existing"]);
        wtconfig::write_issue_number(&RealGit, repo.root(), "feat/7-existing", 7).unwrap();

        let mut t = test_cx(&[], repo.root().to_str().unwrap_or_default());
        t.cx.assume_yes = true;
        t.cx.gh = Arc::new(FakeGh::with_issue(issue()));
        let mut a = args();
        a.branch = Some("feat/7-other".into());
        let err = run(&mut t.cx, &RealHookRunner, &a).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("already linked"));
    }

    #[test]
    fn a_branch_linked_to_another_issue_is_refused() {
        let repo = TestRepo::init();
        let mut t = test_cx(&[], repo.root().to_str().unwrap_or_default());
        t.cx.assume_yes = true;
        t.cx.gh = Arc::new(FakeGh::with_issue(issue()));
        t.cx.agent = Arc::new(FakeAgent::unavailable());

        let mut a = args();
        a.branch = Some("feat/shared".into());
        a.brief = Some("x".into());
        repo.git(&["branch", "feat/shared"]);
        wtconfig::write_issue_number(&RealGit, repo.root(), "feat/shared", 9).unwrap();

        let err = run(&mut t.cx, &RealHookRunner, &a).unwrap_err();
        assert!(
            err.to_string().contains("already linked to issue #9"),
            "{err}"
        );
    }

    #[test]
    fn without_a_terminal_or_yes_it_fails_before_the_fetch() {
        let repo = TestRepo::init();
        let mut t = test_cx(&[], repo.root().to_str().unwrap_or_default());
        // An unavailable `gh` would make a fetch fail loudly; a `Usage` error
        // instead proves the gate ran first.
        t.cx.gh = Arc::new(FakeGh::unavailable());
        let err = run(&mut t.cx, &RealHookRunner, &args()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("interactive terminal"));
    }

    /// A `--yes`-free context whose stderr is a TTY, so the review prompts run.
    fn interactive(repo: &TestRepo, answers: &[&str]) -> crate::testutil::TestCx {
        let mut t = test_cx(&[], repo.root().to_str().unwrap_or_default());
        let err = SharedBuf::new();
        t.cx.err = Stream::new(Box::new(err.clone()), true);
        t.err = err;
        t.cx.input = Box::new(CannedInput::new(answers));
        t.cx.gh = Arc::new(FakeGh::with_issue(issue()));
        t
    }

    #[test]
    fn declining_the_confirmation_creates_nothing() {
        let repo = TestRepo::init();
        // branch, base, brief, then decline.
        let mut t = interactive(&repo, &["", "", "", "n"]);
        t.cx.agent = Arc::new(FakeAgent::drafting(
            r#"{"branch":"feat/7-add-login","brief":"b"}"#,
        ));
        let code = run(&mut t.cx, &RealHookRunner, &args()).unwrap();

        assert_eq!(code, 0);
        assert!(t.err.contents().contains("aborted"));
        assert!(!branches(&repo).iter().any(|b| b == "feat/7-add-login"));
        assert_eq!(meta(&repo, "feat/7-add-login").issue_number, None);
    }

    #[test]
    fn interactive_edits_override_the_generated_values() {
        let repo = TestRepo::init();
        let mut t = interactive(&repo, &["fix/7-mine", "", "my brief", "y"]);
        t.cx.agent = Arc::new(FakeAgent::drafting(
            r#"{"branch":"feat/7-add-login","brief":"generated"}"#,
        ));
        let code = run(&mut t.cx, &RealHookRunner, &args()).unwrap();

        assert_eq!(code, 0);
        assert!(branches(&repo).iter().any(|b| b == "fix/7-mine"));
        assert_eq!(
            meta(&repo, "fix/7-mine").issue_brief.as_deref(),
            Some("my brief")
        );
    }

    #[test]
    fn an_edited_off_contract_branch_warns_but_proceeds() {
        let repo = TestRepo::init();
        let mut t = interactive(&repo, &["hotfix-thing", "", "", "y"]);
        t.cx.agent = Arc::new(FakeAgent::drafting(
            r#"{"branch":"feat/7-add-login","brief":"b"}"#,
        ));
        let code = run(&mut t.cx, &RealHookRunner, &args()).unwrap();

        assert_eq!(code, 0);
        assert!(branches(&repo).iter().any(|b| b == "hotfix-thing"));
        assert!(t.err.contents().contains("does not follow TYPE/7-SLUG"));
        // The link still resolves, which is why the warning is not an error.
        assert_eq!(meta(&repo, "hotfix-thing").issue_number, Some(7));
    }

    #[test]
    fn an_edited_illegal_branch_name_is_still_refused() {
        let repo = TestRepo::init();
        let mut t = interactive(&repo, &["not a branch", "", "", "y"]);
        t.cx.agent = Arc::new(FakeAgent::drafting(
            r#"{"branch":"feat/7-add-login","brief":"b"}"#,
        ));
        let err = run(&mut t.cx, &RealHookRunner, &args()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn the_prompt_carries_the_naming_contract_verbatim() {
        let prompt = build_generation_prompt(&issue());
        assert!(prompt.contains(&naming::branch_rule(7)));
        assert!(prompt.contains("Add login page"));
        assert!(prompt.contains("Users need to sign in."));
        assert!(prompt.contains("enhancement"));
        // Only the fetched fields; nothing invented.
        assert!(!prompt.contains("comments"));
    }

    #[test]
    fn plan_parsing_tolerates_code_fences() {
        let bare = parse_issue_plan(r#"{"branch":"feat/7-x","brief":"b"}"#).unwrap();
        assert_eq!(bare.branch, "feat/7-x");
        let fenced =
            parse_issue_plan("```json\n{\"branch\":\"feat/7-x\",\"brief\":\"b\"}\n```").unwrap();
        assert_eq!(fenced, bare);
        let plain =
            parse_issue_plan("```\n{\"branch\":\"feat/7-x\",\"brief\":\"b\"}\n```").unwrap();
        assert_eq!(plain, bare);
        // A missing brief is tolerated; a missing branch is not.
        assert_eq!(
            parse_issue_plan(r#"{"branch":"feat/7-x"}"#).unwrap().brief,
            ""
        );
        assert!(parse_issue_plan(r#"{"brief":"b"}"#).is_err());
        assert!(parse_issue_plan("not json at all").is_err());
    }

    #[test]
    fn generation_flags_override_the_config_profile() {
        let config = Config::default();
        let mut a = args();
        let opts = resolve_generation_options(&a, &config).unwrap();
        assert_eq!(opts.model, config.agent_generation.model);
        assert_eq!(opts.effort, config.agent_generation.effort);
        // Generation is always bounded, so a hung agent cannot block a worktree.
        assert_eq!(opts.timeout, Some(GENERATION_TIMEOUT));

        a.model = Some("opus".into());
        a.effort = Some("high".into());
        let opts = resolve_generation_options(&a, &config).unwrap();
        assert_eq!(opts.model, AgentModel::Opus);
        assert_eq!(opts.effort, Effort::High);

        a.model = Some("gpt".into());
        let err = resolve_generation_options(&a, &config).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn a_bad_model_flag_fails_rather_than_falling_back() {
        // The one failure in this flow that is the user's to fix.
        let repo = TestRepo::init();
        let mut t = test_cx(&[], repo.root().to_str().unwrap_or_default());
        t.cx.assume_yes = true;
        t.cx.gh = Arc::new(FakeGh::with_issue(issue()));
        let mut a = args();
        a.effort = Some("nonsense".into());
        let err = run(&mut t.cx, &RealHookRunner, &a).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn base_choice_accepts_a_number_a_ref_and_a_blank() {
        let repo = TestRepo::init();
        let all = vec!["main".to_string(), "topic".to_string()];

        let mut t = interactive(&repo, &["2"]);
        assert_eq!(
            prompt_base(&mut t.cx, Some("main"), &all).unwrap(),
            Some("topic".to_string())
        );

        let mut t = interactive(&repo, &["origin/release"]);
        assert_eq!(
            prompt_base(&mut t.cx, Some("main"), &all).unwrap(),
            Some("origin/release".to_string())
        );

        let mut t = interactive(&repo, &[""]);
        assert_eq!(
            prompt_base(&mut t.cx, Some("main"), &all).unwrap(),
            Some("main".to_string())
        );

        let mut t = interactive(&repo, &["9"]);
        let err = prompt_base(&mut t.cx, Some("main"), &all).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn a_field_prompt_keeps_the_current_value_on_a_blank_line() {
        let repo = TestRepo::init();
        let mut t = interactive(&repo, &["", "typed"]);
        assert_eq!(prompt_value(&mut t.cx, "branch", "kept").unwrap(), "kept");
        assert_eq!(prompt_value(&mut t.cx, "branch", "kept").unwrap(), "typed");
        assert!(t.err.contents().contains("branch [kept]: "));
    }

    #[test]
    fn the_worktree_lands_where_the_confirmation_previewed() {
        let (repo, t, _) = run_with_agent(
            FakeAgent::drafting(r#"{"branch":"feat/7-add-login","brief":"b"}"#),
            &args(),
        );
        let mut probe = test_cx(&[], repo.root().to_str().unwrap_or_default());
        probe.cx.assume_yes = true;
        let session = open_session(&probe.cx, &RealGit).unwrap();
        let expected = target_of(&session, &probe.cx.env.clone(), "feat/7-add-login");
        assert!(
            expected.is_dir(),
            "{} is not a directory",
            expected.display()
        );
        assert!(t.err.contents().contains(&expected.display().to_string()));
    }
}
