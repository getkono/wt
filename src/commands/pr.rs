//! `wt pr` — check out a GitHub PR into its own worktree, or list open PRs
//! (spec §7). PR operations go through the `gh` boundary (§4).

use std::path::{Path, PathBuf};

use crate::cli::{PrArgs, PrSub};
use crate::commands::{
    Session, emit_worktree, maybe_init_submodules, open_session, resolve_target, rollback_worktree,
};
use crate::config::wtconfig;
use crate::copy::copy_ignored_files;
use crate::cx::Cx;
use crate::error::Result;
use crate::gh::GhClient;
use crate::git::cli::GitCli;
use crate::git::{branch_ref, ops, resolve_hex};
use crate::hooks::{HookContext, HookRunner, run_post_create};
use crate::slug::slugify_with_fallback;
use crate::time::{now_unix, parse_iso8601, relative};
use crate::worktree_service::enumerate_worktrees;

/// Dispatches `pr list`, `pr <target>` (checkout), or `pr` (picker, TUI).
pub fn run(cx: &mut Cx, hooks: &dyn HookRunner, args: &PrArgs, json: bool) -> Result<u8> {
    // `wt pr` with no target and no sub: open the interactive PR picker (§7).
    // The picker opens its own session, so return before opening one here.
    if args.sub.is_none() && args.target.is_none() {
        return launch_pr_picker(cx);
    }

    // `wt pr open`: compose and open a PR for the current branch (issue #9). It
    // opens its own session, so dispatch before setting one up here.
    if let Some(PrSub::Open(open_args)) = &args.sub {
        return crate::commands::pr_open::run(cx, open_args, json);
    }

    let git = cx.git.clone();
    let gh = cx.gh.clone();
    let session = open_session(cx, git.as_ref())?;
    let dir = session
        .repo
        .current_workdir()
        .unwrap_or_else(|| session.primary_root.clone());

    if matches!(args.sub, Some(PrSub::List)) {
        return pr_list(cx, gh.as_ref(), &dir, json);
    }
    let target = args.target.clone().unwrap_or_default();
    pr_checkout(
        cx,
        git.as_ref(),
        gh.as_ref(),
        hooks,
        &session,
        &dir,
        &target,
        args,
        json,
    )
}

/// Launches the TUI PR picker; on a checkout, prints the chosen worktree path
/// (so the wrapper `cd`s). A cancelled picker prints nothing and exits `0`.
fn launch_pr_picker(cx: &mut Cx) -> Result<u8> {
    match crate::tui::run_pr_picker(cx)? {
        Some(path) => {
            cx.out.line(&path.to_string_lossy())?;
            Ok(0)
        }
        None => Ok(0),
    }
}

/// `pr list` — print open PRs as a table or newline-delimited JSON.
fn pr_list(cx: &mut Cx, gh: &dyn GhClient, dir: &Path, json: bool) -> Result<u8> {
    let prs = gh.list_open_prs(dir)?;
    if json {
        for pr in &prs {
            let row = serde_json::json!({
                "number": pr.number,
                "title": pr.title,
                "author": pr.author.login,
                "state": pr.pr_state().as_str(),
                "head_ref": pr.head_ref_name,
                "created_at": pr.created_at,
            });
            cx.out.line(&serde_json::to_string(&row)?)?;
        }
        return Ok(0);
    }
    if prs.is_empty() {
        cx.err.line("no open pull requests")?;
        return Ok(0);
    }
    let now = now_unix();
    for pr in &prs {
        let age = parse_iso8601(&pr.created_at).map_or_else(String::new, |u| relative(now, u));
        cx.out.line(&format!(
            "#{}  {}  ({})  {}  {age}",
            pr.number,
            pr.title,
            pr.author.login,
            pr.pr_state().as_str()
        ))?;
    }
    Ok(0)
}

/// `pr <target>` — fetch the PR head, create a worktree for it, then emit the
/// navigation result (path/JSON/note).
#[allow(clippy::too_many_arguments)]
fn pr_checkout(
    cx: &mut Cx,
    git: &dyn GitCli,
    gh: &dyn GhClient,
    hooks: &dyn HookRunner,
    session: &Session,
    dir: &Path,
    target: &str,
    args: &PrArgs,
    json: bool,
) -> Result<u8> {
    let (path, existed) =
        checkout_pr_worktree(cx, git, gh, hooks, session, dir, target, args.no_hooks)?;
    let note = if existed {
        "worktree already exists at"
    } else {
        "checked out PR worktree at"
    };
    emit_worktree(cx, &path, json, args.no_switch, note)
}

/// Checks out `target` (a PR number, URL, or head branch) into a worktree,
/// recording its PR metadata (§7). Returns the worktree path and whether the
/// worktree already existed (vs. was created here). Does not emit output — the
/// caller decides how to surface the path (CLI navigation result, or TUI
/// switch).
#[allow(clippy::too_many_arguments)]
pub(crate) fn checkout_pr_worktree(
    cx: &mut Cx,
    git: &dyn GitCli,
    gh: &dyn GhClient,
    hooks: &dyn HookRunner,
    session: &Session,
    dir: &Path,
    target: &str,
    no_hooks: bool,
) -> Result<(PathBuf, bool)> {
    let view = gh.view_pr(dir, target)?;
    let root = session.primary_root.clone();
    let branch = view.head_ref_name.clone();
    let base = view.base_ref_name.clone();
    let number = view.number;
    let state = view.pr_state();

    // If the PR's head branch is already a worktree, record/refresh its PR
    // metadata (§7) and switch to it. The worktree was not necessarily created
    // by `wt`, so this does not mark it "created by wt".
    let worktrees = enumerate_worktrees(&session.repo, git)?;
    if let Some(existing) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch.as_str()))
    {
        let path = existing.path.clone();
        wtconfig::write_pr(git, &root, &branch, number, state.as_str(), &view.title)?;
        wtconfig::write_pr_url(git, &root, &branch, &view.url)?;
        wtconfig::write_base_ref(git, &root, &branch, &base)?;
        return Ok((path, true));
    }

    // Fetch the PR head (works for fork PRs too via the pull/<n>/head ref).
    ops::fetch_refspec(
        git,
        &root,
        &session.config.pr_default_remote,
        &format!("pull/{number}/head"),
    )?;
    let head_oid = git
        .run(&root, &["rev-parse", "FETCH_HEAD"])?
        .trim()
        .to_string();
    let short_hash = head_oid.get(..7).unwrap_or(&head_oid).to_string();
    let slug = slugify_with_fallback(&branch, &short_hash);
    // If a local branch of this name already exists (but has no worktree), check
    // it out as-is rather than failing on `-b` (mirrors `wt new`).
    let branch_exists = resolve_hex(session.repo.gix(), &branch_ref(&branch)).is_some();

    let worktree_path = resolve_target(
        &session.config,
        &root,
        &branch,
        &slug,
        &short_hash,
        &cx.env,
        session.repo.is_bare(),
    )?;
    if let Some(parent) = worktree_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let target_str = worktree_path.to_string_lossy().into_owned();
    if branch_exists {
        ops::worktree_add(git, &root, &target_str, &branch)?;
    } else {
        ops::worktree_add_branch(git, &root, &branch, &target_str, "FETCH_HEAD", false)?;
    }

    // Record metadata + copy, rolling back on failure (§13).
    let copy_outcome = match (|| -> Result<crate::copy::CopyOutcome> {
        wtconfig::write_pr(git, &root, &branch, number, state.as_str(), &view.title)?;
        wtconfig::write_pr_url(git, &root, &branch, &view.url)?;
        wtconfig::write_base_ref(git, &root, &branch, &base)?;
        // Only mark "created by wt" when we actually created the branch here; a
        // pre-existing branch belongs to the user and must not be branch-deleted
        // by a later `remove` (spec §10).
        if !branch_exists {
            wtconfig::mark_created_by_wt(git, &root, &branch)?;
        }
        let source = session
            .repo
            .current_workdir()
            .unwrap_or_else(|| root.clone());
        copy_ignored_files(git, &source, &worktree_path, &session.config.copy)
    })() {
        Ok(outcome) => outcome,
        Err(e) => {
            // Delete the branch only if we created it here, but always clear the
            // metadata this command wrote (§13).
            rollback_worktree(git, &root, &worktree_path, &branch, !branch_exists, true);
            return Err(e);
        }
    };
    crate::commands::log_copy_outcome(cx, &copy_outcome);

    let ctx = HookContext {
        worktree_path: worktree_path.clone(),
        branch: branch.clone(),
        repo_root: root.clone(),
        base_ref: Some(base),
        pr_number: Some(number),
    };
    run_post_create(
        hooks,
        cx,
        session.config.hooks_post_create.as_deref(),
        &ctx,
        no_hooks,
    )?;

    // Honor `[submodules] init` for PR worktrees too (issue #50); no
    // per-invocation flag here. Non-fatal.
    maybe_init_submodules(
        cx,
        git,
        &worktree_path,
        session.config.submodules_init,
        None,
    )?;

    Ok((worktree_path, false))
}

#[cfg(test)]
mod tests {
    use crate::cli::{PrArgs, PrSub};
    use crate::gh::PrView;
    use crate::hooks::RealHookRunner;
    use crate::testutil::{FakeGh, TestRepo};
    use std::sync::Arc;

    fn pr_args(target: Option<&str>, sub: Option<PrSub>) -> PrArgs {
        PrArgs {
            target: target.map(str::to_string),
            no_switch: false,
            no_hooks: true,
            sub,
        }
    }

    fn view(number: u64, head: &str, base: &str) -> PrView {
        PrView {
            number,
            title: "Add login".into(),
            state: "OPEN".into(),
            is_draft: false,
            head_ref_name: head.into(),
            base_ref_name: base.into(),
            url: format!("https://github.com/o/r/pull/{number}"),
        }
    }

    /// Sets up a fetchable `pull/<n>/head` ref served by an `origin` remote
    /// pointing at the repo itself, returning the repo.
    fn repo_with_pr(number: u64) -> TestRepo {
        let repo = TestRepo::init();
        // A commit to serve as the PR head.
        repo.write("pr.txt", "from pr\n");
        repo.commit_all("pr commit");
        let pr_oid = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", &format!("refs/pull/{number}/head"), &pr_oid]);
        // Reset main back so the PR commit is "ahead".
        repo.git(&["reset", "-q", "--hard", "HEAD~1"]);
        repo.git(&["remote", "add", "origin", repo.root().to_str().unwrap()]);
        repo
    }

    #[test]
    fn checks_out_pr_into_worktree() {
        let repo = repo_with_pr(123);
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = Arc::new(FakeGh::with_view(view(123, "pr-feature", "main")));
        let code = super::run(
            &mut t.cx,
            &RealHookRunner,
            &pr_args(Some("123"), None),
            false,
        )
        .unwrap();
        assert_eq!(code, 0);
        let path = t.out.contents().trim().to_string();
        assert!(std::path::Path::new(&path).is_dir());
        assert!(path.ends_with("pr-feature"));
        // The PR file is present (correct head fetched).
        assert!(std::path::Path::new(&path).join("pr.txt").exists());
        // Metadata recorded.
        assert_eq!(
            repo.git(&["config", "--get", "wt.pr-feature.prNumber"])
                .trim(),
            "123"
        );
        assert_eq!(
            repo.git(&["config", "--get", "wt.pr-feature.baseRef"])
                .trim(),
            "main"
        );
        assert_eq!(
            repo.git(&["config", "--get", "wt.pr-feature.prState"])
                .trim(),
            "open"
        );
        assert!(
            repo.git(&["config", "--get", "wt.pr-feature.prUrl"])
                .contains("pull/123")
        );
    }

    #[test]
    fn pr_on_existing_worktree_records_metadata_without_marking_created() {
        // Re-running `wt pr <n>` on a branch that already has a worktree records
        // the PR metadata (§7) without marking the worktree as created by wt.
        let repo = repo_with_pr(55);
        repo.add_worktree("pr-feature", "../pf");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = Arc::new(FakeGh::with_view(view(55, "pr-feature", "main")));
        let code = super::run(
            &mut t.cx,
            &RealHookRunner,
            &pr_args(Some("55"), None),
            false,
        )
        .unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            repo.git(&["config", "--get", "wt.pr-feature.prNumber"])
                .trim(),
            "55"
        );
        assert_eq!(
            repo.git(&["config", "--get", "wt.pr-feature.baseRef"])
                .trim(),
            "main"
        );
        assert!(
            repo.git(&["config", "--get", "wt.pr-feature.prUrl"])
                .contains("pull/55")
        );
        // Not marked created-by-wt (the worktree predates this command). Keys are
        // lowercased in `config --list`.
        assert!(
            !repo
                .git(&["config", "--list"])
                .contains("wt.pr-feature.createdbywt")
        );
    }

    #[test]
    fn pr_checks_out_existing_local_branch_without_a_worktree() {
        // A local branch of the PR head name exists but has no worktree: check it
        // out (no `-b`) rather than failing.
        let repo = repo_with_pr(77);
        repo.git(&["branch", "pr-feature"]);
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = Arc::new(FakeGh::with_view(view(77, "pr-feature", "main")));
        let code = super::run(
            &mut t.cx,
            &RealHookRunner,
            &pr_args(Some("77"), None),
            false,
        )
        .unwrap();
        assert_eq!(code, 0);
        let path = t.out.contents().trim().to_string();
        assert!(std::path::Path::new(&path).is_dir());
        assert!(path.ends_with("pr-feature"));
        // The user's pre-existing branch is NOT marked created-by-wt (so a later
        // `remove` will not delete it).
        assert!(
            !repo
                .git(&["config", "--list"])
                .contains("wt.pr-feature.createdbywt")
        );
    }

    #[test]
    fn pr_rollback_on_existing_branch_keeps_branch_and_clears_metadata() {
        use crate::git::cli::{GitCli, GitOutput, RealGit};
        use std::path::Path as StdPath;
        // A git that fails the copy step's `ls-files` after the metadata writes
        // succeed, forcing a rollback on the pre-existing-branch path.
        struct FailLs(RealGit);
        impl GitCli for FailLs {
            fn run_raw(&self, repo: &StdPath, args: &[&str]) -> crate::error::Result<GitOutput> {
                if args.first() == Some(&"ls-files") {
                    return Ok(GitOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "boom".into(),
                    });
                }
                self.0.run_raw(repo, args)
            }
        }

        let repo = repo_with_pr(88);
        repo.git(&["branch", "pr-feature"]);
        // A copy pattern so the (failing) copy step runs.
        std::fs::write(repo.root().join(".wt.toml"), "copy = [\".env\"]\n").unwrap();
        repo.write(".env", "X=1\n");
        let mut t = crate::testutil::test_cx_with_git(
            &[],
            repo.root().to_str().unwrap(),
            Arc::new(FailLs(RealGit)),
        );
        t.cx.gh = Arc::new(FakeGh::with_view(view(88, "pr-feature", "main")));
        let err = super::run(
            &mut t.cx,
            &RealHookRunner,
            &pr_args(Some("88"), None),
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("boom"));
        // The user's branch survives the rollback...
        assert!(
            !repo
                .git(&["branch", "--list", "pr-feature"])
                .trim()
                .is_empty(),
            "pre-existing branch must not be deleted on rollback"
        );
        // ...and the metadata written before the failure is cleared (no orphans).
        assert!(
            !repo.git(&["config", "--list"]).contains("wt.pr-feature."),
            "rollback must clear the wt.* metadata it wrote"
        );
    }

    #[test]
    fn pr_list_prints_open_prs() {
        use crate::gh::{Author, PrSummary};
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = Arc::new(FakeGh::with_list(vec![PrSummary {
            number: 7,
            title: "Fix bug".into(),
            author: Author {
                login: "alice".into(),
            },
            state: "OPEN".into(),
            is_draft: false,
            head_ref_name: "fix".into(),
            created_at: String::new(),
        }]));
        super::run(
            &mut t.cx,
            &RealHookRunner,
            &pr_args(None, Some(PrSub::List)),
            false,
        )
        .unwrap();
        let out = t.out.contents();
        assert!(out.contains("#7"));
        assert!(out.contains("Fix bug"));
        assert!(out.contains("alice"));
    }

    #[test]
    fn pr_list_json() {
        use crate::gh::{Author, PrSummary};
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = Arc::new(FakeGh::with_list(vec![PrSummary {
            number: 9,
            title: "T".into(),
            author: Author {
                login: "bob".into(),
            },
            state: "OPEN".into(),
            is_draft: true,
            head_ref_name: "wip".into(),
            created_at: String::new(),
        }]));
        super::run(
            &mut t.cx,
            &RealHookRunner,
            &pr_args(None, Some(PrSub::List)),
            true,
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(t.out.contents().trim()).unwrap();
        assert_eq!(v["number"], serde_json::json!(9));
        assert_eq!(v["state"], serde_json::json!("draft"));
    }

    #[test]
    fn gh_unavailable_is_actionable() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.gh = Arc::new(FakeGh::unavailable());
        let err = super::run(
            &mut t.cx,
            &RealHookRunner,
            &pr_args(None, Some(PrSub::List)),
            false,
        )
        .unwrap_err();
        assert!(matches!(err, crate::error::Error::GhUnavailable(_)));
    }
}
