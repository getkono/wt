//! `wt pr open` — compose and open (create or update) a GitHub PR for the
//! current branch (issue #9). The PR mechanics are *backed by `sendit`*: this
//! module reuses `sendit`'s pure helpers (trunk resolution, git-output parsing,
//! `gh` arg building, URL parsing, the create/update decision, and the summary
//! formatter) but runs every `git`/`gh` call through `wt`'s injected
//! [`GitCli`]/[`GhClient`] boundaries so the flow honors `-C`, stays faked in
//! tests, and respects the stdout/stderr contract.
//!
//! The title/body come either from the user (via the TUI compose form or
//! `--title`/`--body` flags) or from a code agent (`--ai`, see [`draft_with_ai`]).

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

use crate::agent::{AgentClient, AgentKind, AgentModel, AgentOptions, Effort};
use crate::cli::PrOpenArgs;
use crate::commands::open_session;
use crate::config::Config;
use crate::config::wtconfig;
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::gh::{GhClient, OpenPr};
use crate::git::cli::GitCli;
use crate::git::refs::current_branch;
use crate::git::{resolve_hex, upstream_of};
use crate::tui::ComposeSeed;

/// Dispatches `wt pr open`: gather context, then either open the TUI compose form
/// (interactive) or submit directly (non-interactive / `--yes`), and emit the
/// result. Spec §5: the human summary goes to stderr, the bare URL (or JSON) to
/// stdout.
pub(crate) fn run(cx: &mut Cx, args: &PrOpenArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let gh = cx.gh.clone();
    let session = open_session(cx, git.as_ref())?;
    let root = session.primary_root.clone();
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| root.clone());

    let ctx = gather_pr_context(
        git.as_ref(),
        gh.as_ref(),
        session.repo.gix(),
        &root,
        &dir,
        args.base.as_deref(),
    )?;
    let flag_body = read_flag_body(args)?;
    // The model/effort for AI fill: CLI flags override the config defaults. The
    // compose form always carries these (Ctrl-A can draft at any time), so they
    // are resolved even when `--ai` was not passed.
    let opts = resolve_agent_options(args, &session.config)?;

    // Gate on the stderr TTY (where the TUI draws), matching the rest of `wt`.
    let interactive = cx.err.is_tty() && !args.yes;
    if interactive {
        let seed = ComposeSeed {
            title: args.title.clone().unwrap_or_default(),
            body: flag_body.unwrap_or_default(),
            draft: args.draft,
            model: opts.model,
            effort: opts.effort,
        };
        let action = action_for_form(ctx.existing_pr.as_ref(), args);
        match crate::tui::run_pr_compose(cx, &session, ctx.clone(), action, seed, args.ai)? {
            Some((outcome, spec)) => emit_outcome(cx, &ctx, &spec, &outcome, json),
            // Cancelled: nothing pushed or created.
            None => Ok(0),
        }
    } else {
        let (title, body) = if args.ai {
            draft_with_ai(cx.agent.as_ref(), &ctx, &dir, &opts)?
        } else {
            (
                args.title.clone().unwrap_or_default(),
                flag_body.unwrap_or_default(),
            )
        };
        if title.trim().is_empty() {
            return Err(Error::usage(
                "a PR title is required: pass --title (or run interactively)",
            ));
        }
        let action = resolve_direct_action(ctx.existing_pr.as_ref(), args)?;
        let spec = sendit::PrSpec {
            title,
            body,
            draft: args.draft,
        };
        let outcome = submit_pr(
            git.as_ref(),
            gh.as_ref(),
            &root,
            &dir,
            &session.config.pr_default_remote,
            &ctx,
            &spec,
            action,
        )?;
        record_pr_metadata(
            git.as_ref(),
            &root,
            &ctx.branch,
            &ctx.trunk,
            &outcome,
            &spec.title,
        )?;
        emit_outcome(cx, &ctx, &spec, &outcome, json)
    }
}

/// Reads the PR body from `--body`, `--body-file <path>`, or `--body-file -`
/// (stdin); `None` when no body flag was given.
fn read_flag_body(args: &PrOpenArgs) -> Result<Option<String>> {
    if let Some(body) = &args.body {
        return Ok(Some(body.clone()));
    }
    let Some(path) = &args.body_file else {
        return Ok(None);
    };
    if path == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        return Ok(Some(buf));
    }
    Ok(Some(std::fs::read_to_string(path)?))
}

/// The action shown in (and submitted from) the compose form: an unforced
/// conflict defaults to updating the existing PR — the friendly GUI default.
fn action_for_form(existing: Option<&sendit::ExistingPr>, args: &PrOpenArgs) -> sendit::PrAction {
    match sendit::resolve_action(existing, args.update, args.new, args.yes) {
        sendit::ActionChoice::Create => sendit::PrAction::Create,
        sendit::ActionChoice::Update(number) => sendit::PrAction::Update { number },
        sendit::ActionChoice::Conflict(pr) => sendit::PrAction::Update { number: pr.number },
    }
}

/// The action for a non-interactive submit: an unforced conflict is an error
/// (the caller must pass `--update` or `--new`), since there is no prompt.
fn resolve_direct_action(
    existing: Option<&sendit::ExistingPr>,
    args: &PrOpenArgs,
) -> Result<sendit::PrAction> {
    match sendit::resolve_action(existing, args.update, args.new, args.yes) {
        sendit::ActionChoice::Create => Ok(sendit::PrAction::Create),
        sendit::ActionChoice::Update(number) => Ok(sendit::PrAction::Update { number }),
        sendit::ActionChoice::Conflict(pr) => Err(Error::usage(format!(
            "an open PR (#{}) already exists for this branch; pass --update or --new",
            pr.number
        ))),
    }
}

/// Emits the result: a JSON object (`--json`) or `sendit`'s human summary to
/// stderr plus the bare PR URL to stdout (so it is scriptable).
fn emit_outcome(
    cx: &mut Cx,
    ctx: &sendit::PrContext,
    spec: &sendit::PrSpec,
    outcome: &sendit::PrOutcome,
    json: bool,
) -> Result<u8> {
    if json {
        let action = match outcome.action {
            sendit::PrAction::Create => "create",
            sendit::PrAction::Update { .. } => "update",
        };
        let row = serde_json::json!({
            "url": outcome.url,
            "number": outcome.number,
            "action": action,
            "draft": outcome.draft,
        });
        cx.out.line(&serde_json::to_string(&row)?)?;
    } else {
        cx.err.line(&sendit::format_summary(outcome, ctx, spec))?;
        cx.out.line(&outcome.url)?;
    }
    Ok(0)
}

/// Maps a `sendit` error into a `wt` error at the boundary. Only
/// [`sendit::resolve_trunk`]'s `NoTrunk` realistically reaches this, since `wt`
/// never calls `sendit`'s process-touching functions.
fn map_sendit_err(e: sendit::SenditError) -> Error {
    Error::operation(e.to_string())
}

/// Converts a `wt` [`OpenPr`] into the `sendit` [`ExistingPr`] used to populate
/// a [`PrContext`](sendit::PrContext).
fn open_pr_to_existing(pr: OpenPr) -> sendit::ExistingPr {
    sendit::ExistingPr {
        number: pr.number,
        url: pr.url,
        state: pr.state,
        is_draft: pr.is_draft,
    }
}

/// The branch behind `refs/remotes/origin/HEAD`, read through the injected
/// `git` boundary (so trunk detection respects `-C` and is fakeable), or `None`.
fn origin_head(git: &dyn GitCli, root: &Path) -> Option<String> {
    let out = git
        .run_raw(
            root,
            &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
        )
        .ok()?;
    if !out.success {
        return None;
    }
    out.stdout
        .trim()
        .strip_prefix("origin/")
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Gathers everything needed to compose a PR for the current branch, using the
/// injected `git`/`gh` boundaries plus `sendit`'s pure parsers. Refuses (with a
/// typed error) when HEAD is detached, the branch *is* the trunk, or there is
/// nothing ahead of the trunk to send.
pub(crate) fn gather_pr_context(
    git: &dyn GitCli,
    gh: &dyn GhClient,
    repo: &gix::Repository,
    root: &Path,
    dir: &Path,
    base_override: Option<&str>,
) -> Result<sendit::PrContext> {
    let branch = current_branch(repo).ok_or_else(|| {
        Error::operation("not on a branch (detached HEAD); check out a feature branch first")
    })?;

    let gh_default = gh.default_branch(dir)?;
    let origin = origin_head(git, root);
    let trunk = sendit::resolve_trunk(
        base_override,
        gh_default.as_deref(),
        origin.as_deref(),
        resolve_hex(repo, "refs/heads/main").is_some(),
        resolve_hex(repo, "refs/heads/master").is_some(),
    )
    .map_err(map_sendit_err)?;

    if branch == trunk {
        return Err(Error::operation(format!(
            "refusing to open a PR from the base branch `{trunk}`; check out a feature branch first"
        )));
    }

    let merge_base = git
        .run(root, &["merge-base", "HEAD", &trunk])?
        .trim()
        .to_string();
    let range = format!("{merge_base}..HEAD");
    let commits_ahead = git
        .run(root, &["rev-list", "--count", &range])?
        .trim()
        .parse::<u32>()
        .map_err(|e| Error::operation(format!("unexpected rev-list output: {e}")))?;
    if commits_ahead == 0 {
        return Err(Error::operation(format!(
            "no commits ahead of `{trunk}`; nothing to send"
        )));
    }

    let commit_log = sendit::parse_commit_log(&git.run(root, &["log", "--format=%h %s", &range])?);
    let raw_stat = git.run(root, &["diff", "--stat", &range])?;
    let (files, insertions, deletions) =
        sendit::parse_shortstat(&git.run(root, &["diff", "--shortstat", &range])?);
    let diffstat = sendit::DiffStat {
        files,
        insertions,
        deletions,
        raw: raw_stat.trim_end().to_string(),
    };

    let existing_pr = gh
        .find_pr_for_branch(dir, &branch)?
        .map(open_pr_to_existing);
    let has_upstream = upstream_of(repo, &branch).is_some();

    Ok(sendit::PrContext {
        branch,
        trunk,
        merge_base,
        has_upstream,
        commits_ahead,
        commit_log,
        diffstat,
        existing_pr,
    })
}

/// Builds the prompt that asks a code agent to draft a PR title and body from
/// the branch's commits and diff stat. The agent is asked to put the title on
/// the first line and the body after a blank line so the result parses with
/// [`sendit::parse_editor_output`].
pub(crate) fn build_ai_prompt(ctx: &sendit::PrContext) -> String {
    let mut s = String::new();
    s.push_str(
        "Write a GitHub pull request title and description for the changes on this branch.\n\
         Put the title on the first line, then a blank line, then a Markdown description.\n\
         Do not wrap the output in code fences and do not add any preamble.\n\n",
    );
    let _ = writeln!(s, "Branch: {} -> {}", ctx.branch, ctx.trunk);
    let _ = writeln!(s, "Commits ({}):", ctx.commits_ahead);
    for c in &ctx.commit_log {
        let _ = writeln!(s, "  {} {}", c.hash, c.subject);
    }
    if !ctx.diffstat.raw.is_empty() {
        s.push_str("\nDiff stat:\n");
        for line in ctx.diffstat.raw.lines() {
            let _ = writeln!(s, "  {line}");
        }
    }
    s
}

/// Resolves the agent model + effort for an AI draft: an explicit `--model` /
/// `--effort` flag overrides the resolved config default. Unknown flag values
/// are a usage error (exit 2).
pub(crate) fn resolve_agent_options(args: &PrOpenArgs, config: &Config) -> Result<AgentOptions> {
    let model = match &args.model {
        Some(m) => AgentModel::parse(m).ok_or_else(|| {
            Error::usage(format!(
                "unknown --model {m:?}; expected one of: opus, sonnet, haiku"
            ))
        })?,
        None => config.agent_model,
    };
    let effort = match &args.effort {
        Some(e) => Effort::parse(e).ok_or_else(|| {
            Error::usage(format!(
                "unknown --effort {e:?}; expected one of: low, medium, high"
            ))
        })?,
        None => config.agent_effort,
    };
    Ok(AgentOptions { model, effort })
}

/// Runs the code agent (`claude`) to draft a PR `(title, body)` from `ctx` with
/// the selected model and effort (`opts`), parsing its output with
/// [`sendit::parse_editor_output`]. Surfaces an erroring or empty draft as a
/// typed error; a missing agent propagates [`Error::AgentUnavailable`].
pub(crate) fn draft_with_ai(
    agent: &dyn AgentClient,
    ctx: &sendit::PrContext,
    dir: &Path,
    opts: &AgentOptions,
) -> Result<(String, String)> {
    let run = agent.run(AgentKind::Claude, &build_ai_prompt(ctx), dir, opts)?;
    if run.is_error {
        return Err(Error::operation(format!(
            "code agent reported an error: {}",
            run.result.trim()
        )));
    }
    let parsed = sendit::parse_editor_output(&run.result)
        .map_err(|_| Error::operation("code agent returned an empty PR title"))?;
    Ok((parsed.title, parsed.body))
}

/// Pushes the branch and creates or updates the PR through the injected
/// boundaries, reusing `sendit`'s arg builders and URL parsers. Never
/// force-pushes; sets the upstream when the branch has none.
#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_pr(
    git: &dyn GitCli,
    gh: &dyn GhClient,
    root: &Path,
    dir: &Path,
    remote: &str,
    ctx: &sendit::PrContext,
    spec: &sendit::PrSpec,
    action: sendit::PrAction,
) -> Result<sendit::PrOutcome> {
    // Push first (mirrors `sendit`): plain push when an upstream exists, else set
    // it. Never force-push, and only ever the current branch.
    if ctx.has_upstream {
        git.run(root, &["push"])?;
    } else {
        git.run(root, &["push", "-u", remote, &ctx.branch])?;
    }

    match action {
        sendit::PrAction::Create => {
            let stdout = gh.create_pr(dir, &sendit::build_create_args(ctx, spec))?;
            let url = sendit::parse_pr_url(&stdout);
            let number = sendit::parse_pr_number(&url);
            Ok(sendit::PrOutcome {
                url,
                number,
                draft: spec.draft,
                action,
            })
        }
        sendit::PrAction::Update { number } => {
            let stdout = gh.edit_pr(dir, &sendit::build_edit_args(number, spec))?;
            let parsed = sendit::parse_pr_url(&stdout);
            // `gh pr edit` prints the URL; fall back to the known one if absent.
            let url = if parsed.is_empty() {
                ctx.existing_pr
                    .as_ref()
                    .map(|p| p.url.clone())
                    .unwrap_or_default()
            } else {
                parsed
            };
            // Draft is create-only (`gh pr edit` can't toggle it), so reflect the
            // existing PR's draft state on update.
            let draft = ctx
                .existing_pr
                .as_ref()
                .map(|p| p.is_draft)
                .unwrap_or(spec.draft);
            Ok(sendit::PrOutcome {
                url,
                number: Some(number),
                draft,
                action,
            })
        }
    }
}

/// Records the new PR's metadata for `branch` (so `wt list`/TUI show it offline,
/// like a PR checkout does). Skips when no PR number could be parsed; never
/// marks the branch "created by wt" (it is the user's branch).
pub(crate) fn record_pr_metadata(
    git: &dyn GitCli,
    root: &Path,
    branch: &str,
    trunk: &str,
    outcome: &sendit::PrOutcome,
    title: &str,
) -> Result<()> {
    let Some(number) = outcome.number else {
        return Ok(());
    };
    let state = if outcome.draft { "draft" } else { "open" };
    wtconfig::write_pr(git, root, branch, number, state, title)?;
    if !outcome.url.is_empty() {
        wtconfig::write_pr_url(git, root, branch, &outcome.url)?;
    }
    wtconfig::write_base_ref(git, root, branch, trunk)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::git::discover::Repo;
    use crate::testutil::{FakeAgent, FakeGh, TestRepo};

    /// A `TestRepo` on a `feat` branch one commit ahead of `main`, returning the
    /// repo. (`main` already exists from `TestRepo::init`.)
    fn repo_with_feature() -> TestRepo {
        let repo = TestRepo::init();
        repo.git(&["checkout", "-q", "-b", "feat"]);
        repo.write("feature.txt", "hello\nworld\n");
        repo.commit_all("add feature");
        repo
    }

    fn ctx_for(branch: &str, trunk: &str, has_upstream: bool) -> sendit::PrContext {
        sendit::PrContext {
            branch: branch.to_string(),
            trunk: trunk.to_string(),
            merge_base: "abc".to_string(),
            has_upstream,
            commits_ahead: 1,
            commit_log: vec![sendit::CommitEntry {
                hash: "a1b2c3d".to_string(),
                subject: "add feature".to_string(),
            }],
            diffstat: sendit::DiffStat {
                files: 1,
                insertions: 2,
                deletions: 0,
                raw: " feature.txt | 2 ++".to_string(),
            },
            existing_pr: None,
        }
    }

    #[test]
    fn build_ai_prompt_includes_context() {
        let ctx = ctx_for("feat", "main", false);
        let p = build_ai_prompt(&ctx);
        assert!(p.contains("Branch: feat -> main"));
        assert!(p.contains("a1b2c3d add feature"));
        assert!(p.contains("feature.txt | 2 ++"));
        assert!(p.contains("Commits (1):"));
    }

    #[test]
    fn gather_context_for_feature_branch() {
        let repo = repo_with_feature();
        let r = Repo::discover(repo.root()).unwrap();
        let gh = FakeGh::sender("").with_default_branch("main");
        let ctx =
            gather_pr_context(&RealGit, &gh, r.gix(), repo.root(), repo.root(), None).unwrap();
        assert_eq!(ctx.branch, "feat");
        assert_eq!(ctx.trunk, "main");
        assert_eq!(ctx.commits_ahead, 1);
        assert_eq!(ctx.commit_log.len(), 1);
        assert_eq!(ctx.commit_log[0].subject, "add feature");
        assert_eq!(ctx.diffstat.files, 1);
        assert!(ctx.existing_pr.is_none());
        assert!(!ctx.has_upstream);
    }

    #[test]
    fn gather_context_picks_up_existing_pr() {
        let repo = repo_with_feature();
        let r = Repo::discover(repo.root()).unwrap();
        let gh = FakeGh::sender("")
            .with_default_branch("main")
            .with_existing_pr(crate::gh::OpenPr {
                number: 7,
                url: "https://github.com/o/r/pull/7".into(),
                state: "OPEN".into(),
                is_draft: true,
            });
        let ctx =
            gather_pr_context(&RealGit, &gh, r.gix(), repo.root(), repo.root(), None).unwrap();
        let existing = ctx.existing_pr.expect("existing PR");
        assert_eq!(existing.number, 7);
        assert!(existing.is_draft);
    }

    #[test]
    fn gather_context_refuses_on_trunk() {
        // On `main` (the trunk) there is nothing to send and it is the base.
        let repo = TestRepo::init();
        let r = Repo::discover(repo.root()).unwrap();
        let gh = FakeGh::sender("").with_default_branch("main");
        let err =
            gather_pr_context(&RealGit, &gh, r.gix(), repo.root(), repo.root(), None).unwrap_err();
        assert!(err.to_string().contains("base branch"));
    }

    #[test]
    fn gather_context_base_override_wins() {
        let repo = repo_with_feature();
        // Add a `master` ref (at `main`, behind `feat`) so the override is meaningful.
        repo.git(&["branch", "master", "main"]);
        let r = Repo::discover(repo.root()).unwrap();
        let gh = FakeGh::sender("").with_default_branch("main");
        let ctx = gather_pr_context(
            &RealGit,
            &gh,
            r.gix(),
            repo.root(),
            repo.root(),
            Some("master"),
        )
        .unwrap();
        assert_eq!(ctx.trunk, "master");
    }

    #[test]
    fn submit_create_pushes_and_records_args() {
        let bare = TestRepo::init_bare();
        let repo = repo_with_feature();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        let gh = FakeGh::sender("https://github.com/o/r/pull/77\n");
        let spec = sendit::PrSpec {
            title: "Add feature".into(),
            body: "Body".into(),
            draft: false,
        };
        let ctx = ctx_for("feat", "main", false);
        let outcome = submit_pr(
            &RealGit,
            &gh,
            repo.root(),
            repo.root(),
            "origin",
            &ctx,
            &spec,
            sendit::PrAction::Create,
        )
        .unwrap();

        // The PR was created with exactly sendit's create args.
        assert_eq!(
            gh.created_args(),
            vec![sendit::build_create_args(&ctx, &spec)]
        );
        assert_eq!(outcome.url, "https://github.com/o/r/pull/77");
        assert_eq!(outcome.number, Some(77));
        // The branch was pushed to the bare remote and upstream was set.
        assert!(
            !bare
                .git(&["rev-parse", "refs/heads/feat"])
                .trim()
                .is_empty()
        );
        assert_eq!(
            repo.git(&["rev-parse", "--abbrev-ref", "feat@{u}"]).trim(),
            "origin/feat"
        );
    }

    #[test]
    fn submit_update_uses_edit_args_and_keeps_url() {
        let bare = TestRepo::init_bare();
        let repo = repo_with_feature();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        // Push once so the branch has an upstream (update path uses plain push).
        repo.git(&["push", "-u", "origin", "feat"]);
        // Empty stdout from `gh pr edit` => fall back to the existing PR URL.
        let gh = FakeGh::sender("");
        let spec = sendit::PrSpec {
            title: "Updated".into(),
            body: "New body".into(),
            draft: false,
        };
        let mut ctx = ctx_for("feat", "main", true);
        ctx.existing_pr = Some(sendit::ExistingPr {
            number: 9,
            url: "https://github.com/o/r/pull/9".into(),
            state: "OPEN".into(),
            is_draft: false,
        });
        let outcome = submit_pr(
            &RealGit,
            &gh,
            repo.root(),
            repo.root(),
            "origin",
            &ctx,
            &spec,
            sendit::PrAction::Update { number: 9 },
        )
        .unwrap();
        assert_eq!(gh.edited_args(), vec![sendit::build_edit_args(9, &spec)]);
        assert_eq!(outcome.url, "https://github.com/o/r/pull/9");
        assert_eq!(outcome.number, Some(9));
    }

    #[test]
    fn record_metadata_writes_config() {
        let repo = repo_with_feature();
        let outcome = sendit::PrOutcome {
            url: "https://github.com/o/r/pull/77".into(),
            number: Some(77),
            draft: false,
            action: sendit::PrAction::Create,
        };
        record_pr_metadata(
            &RealGit,
            repo.root(),
            "feat",
            "main",
            &outcome,
            "Add feature",
        )
        .unwrap();
        assert_eq!(
            repo.git(&["config", "--get", "wt.feat.prNumber"]).trim(),
            "77"
        );
        assert_eq!(
            repo.git(&["config", "--get", "wt.feat.prState"]).trim(),
            "open"
        );
        assert_eq!(
            repo.git(&["config", "--get", "wt.feat.baseRef"]).trim(),
            "main"
        );
        assert!(
            repo.git(&["config", "--get", "wt.feat.prUrl"])
                .contains("pull/77")
        );
        // The user's branch is not marked created-by-wt.
        assert!(
            !repo
                .git(&["config", "--list"])
                .contains("wt.feat.createdbywt")
        );
    }

    #[test]
    fn record_metadata_skips_without_number() {
        let repo = repo_with_feature();
        let outcome = sendit::PrOutcome {
            url: String::new(),
            number: None,
            draft: false,
            action: sendit::PrAction::Create,
        };
        record_pr_metadata(&RealGit, repo.root(), "feat", "main", &outcome, "T").unwrap();
        assert!(!repo.git(&["config", "--list"]).contains("wt.feat."));
    }

    #[test]
    fn draft_with_ai_parses_title_and_body() {
        let agent = FakeAgent::drafting("Add login\n\n## Summary\n- did it\n");
        let dir = tempfile::tempdir().unwrap();
        let (title, body) = draft_with_ai(
            &agent,
            &ctx_for("feat", "main", false),
            dir.path(),
            &AgentOptions::default(),
        )
        .unwrap();
        assert_eq!(title, "Add login");
        assert_eq!(body, "## Summary\n- did it");
    }

    #[test]
    fn draft_with_ai_threads_model_and_effort() {
        let agent = FakeAgent::drafting("T\n\nB");
        let dir = tempfile::tempdir().unwrap();
        let opts = AgentOptions {
            model: AgentModel::Opus,
            effort: Effort::High,
        };
        draft_with_ai(&agent, &ctx_for("feat", "main", false), dir.path(), &opts).unwrap();
        assert_eq!(agent.last_opts(), Some(opts));
    }

    #[test]
    fn draft_with_ai_maps_error_result() {
        let agent = FakeAgent::erroring("rate limited");
        let dir = tempfile::tempdir().unwrap();
        let err = draft_with_ai(
            &agent,
            &ctx_for("feat", "main", false),
            dir.path(),
            &AgentOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("code agent reported an error"));
    }

    #[test]
    fn draft_with_ai_unavailable_propagates() {
        let agent = FakeAgent::unavailable();
        let dir = tempfile::tempdir().unwrap();
        let err = draft_with_ai(
            &agent,
            &ctx_for("feat", "main", false),
            dir.path(),
            &AgentOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, Error::AgentUnavailable(_)));
    }

    #[test]
    fn resolve_agent_options_flags_override_config() {
        let config = Config {
            agent_model: AgentModel::Haiku,
            agent_effort: Effort::Low,
            ..Config::default()
        };
        // No flags: config defaults win.
        let opts = resolve_agent_options(&open_args(None), &config).unwrap();
        assert_eq!(opts.model, AgentModel::Haiku);
        assert_eq!(opts.effort, Effort::Low);
        // Flags override.
        let mut args = open_args(None);
        args.model = Some("opus".into());
        args.effort = Some("high".into());
        let opts = resolve_agent_options(&args, &config).unwrap();
        assert_eq!(opts.model, AgentModel::Opus);
        assert_eq!(opts.effort, Effort::High);
    }

    #[test]
    fn resolve_agent_options_rejects_unknown_values() {
        let mut args = open_args(None);
        args.model = Some("gpt".into());
        assert!(matches!(
            resolve_agent_options(&args, &Config::default()),
            Err(Error::Usage(_))
        ));
        let mut args = open_args(None);
        args.effort = Some("max".into());
        assert!(matches!(
            resolve_agent_options(&args, &Config::default()),
            Err(Error::Usage(_))
        ));
    }

    fn open_args(title: Option<&str>) -> PrOpenArgs {
        PrOpenArgs {
            title: title.map(str::to_string),
            body: None,
            body_file: None,
            draft: false,
            ai: false,
            model: None,
            effort: None,
            yes: false,
            base: None,
            update: false,
            new: false,
        }
    }

    /// A feature repo with a bare `origin` remote, for end-to-end `run` tests.
    fn repo_with_remote() -> (TestRepo, TestRepo) {
        let bare = TestRepo::init_bare();
        let repo = repo_with_feature();
        repo.git(&["remote", "add", "origin", bare.root().to_str().unwrap()]);
        (repo, bare)
    }

    #[test]
    fn run_direct_non_tty_creates_pr() {
        let (repo, _bare) = repo_with_remote();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = std::sync::Arc::new(
            FakeGh::sender("https://github.com/o/r/pull/77\n").with_default_branch("main"),
        );
        let code = run(&mut t.cx, &open_args(Some("My PR")), false).unwrap();
        assert_eq!(code, 0);
        // Bare URL on stdout; human summary on stderr (spec §5).
        assert_eq!(t.out.contents().trim(), "https://github.com/o/r/pull/77");
        assert!(t.err.contents().contains("Created PR"));
        assert!(t.err.contents().contains("My PR"));
        assert_eq!(
            repo.git(&["config", "--get", "wt.feat.prNumber"]).trim(),
            "77"
        );
    }

    #[test]
    fn run_direct_json_emits_object() {
        let (repo, _bare) = repo_with_remote();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = std::sync::Arc::new(
            FakeGh::sender("https://github.com/o/r/pull/77\n").with_default_branch("main"),
        );
        let code = run(&mut t.cx, &open_args(Some("My PR")), true).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(t.out.contents().trim()).unwrap();
        assert_eq!(v["number"], serde_json::json!(77));
        assert_eq!(v["action"], serde_json::json!("create"));
    }

    #[test]
    fn run_direct_requires_title() {
        let (repo, _bare) = repo_with_remote();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = std::sync::Arc::new(FakeGh::sender("").with_default_branch("main"));
        let err = run(&mut t.cx, &open_args(None), false).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }

    #[test]
    fn run_direct_conflict_requires_flag() {
        let (repo, _bare) = repo_with_remote();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = std::sync::Arc::new(
            FakeGh::sender("")
                .with_default_branch("main")
                .with_existing_pr(OpenPr {
                    number: 5,
                    url: "https://github.com/o/r/pull/5".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                }),
        );
        let err = run(&mut t.cx, &open_args(Some("T")), false).unwrap_err();
        assert!(matches!(err, Error::Usage(_)));
    }

    #[test]
    fn run_direct_update_existing_pr() {
        let (repo, _bare) = repo_with_remote();
        // Set an upstream so the update path uses a plain `push`.
        repo.git(&["push", "-u", "origin", "feat"]);
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = std::sync::Arc::new(
            FakeGh::sender("https://github.com/o/r/pull/5\n")
                .with_default_branch("main")
                .with_existing_pr(OpenPr {
                    number: 5,
                    url: "https://github.com/o/r/pull/5".into(),
                    state: "OPEN".into(),
                    is_draft: false,
                }),
        );
        let mut args = open_args(Some("Updated"));
        args.update = true;
        let code = run(&mut t.cx, &args, true).unwrap();
        assert_eq!(code, 0);
        let v: serde_json::Value = serde_json::from_str(t.out.contents().trim()).unwrap();
        assert_eq!(v["number"], serde_json::json!(5));
        assert_eq!(v["action"], serde_json::json!("update"));
    }
}
