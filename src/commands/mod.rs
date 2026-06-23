//! Command handlers (spec §7). Each module implements one subcommand; this
//! module provides the shared repository session setup and query resolution.

pub mod checkout;
pub mod complete;
pub mod completions;
pub mod config_cmd;
pub mod init;
pub mod list;
pub mod new;
pub mod path;
pub mod pr;
pub mod pr_open;
pub mod prune;
pub mod remove;
pub mod root;
pub mod shell_init;
pub mod staleness;
pub mod status_cmd;
pub mod switch;
pub mod sync;

use std::path::{Path, PathBuf};

use crate::config::{self, Config, SubmoduleInit};
use crate::cx::{Cx, Env};
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::model::Worktree;
use crate::query::{self, Resolved};
use crate::template::{self, TemplateVars};
use crate::worktree_service::build_worktrees;

/// A discovered repository plus its resolved configuration, set up once per
/// repo-scoped command.
pub(crate) struct Session {
    /// The discovered repository (gix handle).
    pub(crate) repo: Repo,
    /// The primary worktree root (or bare repo path).
    pub(crate) primary_root: PathBuf,
    /// The merged configuration.
    pub(crate) config: Config,
}

/// Discovers the repository from the context's working directory, resolves the
/// primary root via `git rev-parse`, and loads the merged configuration.
pub(crate) fn open_session(cx: &Cx, git: &dyn GitCli) -> Result<Session> {
    let repo = Repo::discover(&cx.cwd)?;
    let dir = repo.current_workdir().unwrap_or_else(|| repo.git_dir());
    let common = git.run(
        &dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let common = PathBuf::from(common.trim());
    let primary_root = if repo.is_bare() {
        common
    } else {
        common.parent().map(Path::to_path_buf).unwrap_or(common)
    };
    let config = config::load(Some(&primary_root), &cx.env)?;
    Ok(Session {
        repo,
        primary_root,
        config,
    })
}

/// The outcome of resolving a query in a command handler.
pub(crate) enum Resolution {
    /// A unique worktree (its index).
    Found(usize),
    /// Ambiguous; candidates were listed to stderr (exit code `3`).
    Ambiguous,
    /// No match (the caller maps this to [`Error::NotFound`]).
    NotFound,
}

/// Resolves a query. On ambiguity at an interactive terminal (stderr is a TTY)
/// the TUI picker opens, pre-filtered to the query, and the chosen worktree is
/// returned; otherwise the candidate list is printed to stderr (exit code `3`).
/// Spec §7.
pub(crate) fn resolve_query(cx: &mut Cx, worktrees: &[Worktree], query: &str) -> Resolution {
    resolve_query_with(cx, worktrees, query, |cx, q| {
        crate::tui::run_tui(cx, Some(q))
    })
}

/// [`resolve_query`] with an injectable picker, so the TTY fallback is testable
/// without a real terminal. `picker` returns the chosen worktree path, `None`
/// on cancel, or an error.
fn resolve_query_with(
    cx: &mut Cx,
    worktrees: &[Worktree],
    query: &str,
    picker: impl FnOnce(&mut Cx, &str) -> Result<Option<PathBuf>>,
) -> Resolution {
    match query::resolve(worktrees, query) {
        Resolved::One(index) => Resolution::Found(index),
        Resolved::Ambiguous(indices) => {
            if cx.err.is_tty() {
                match picker(cx, query) {
                    // Map the chosen path back to an index in the caller's slice.
                    // A miss (path not in this set) or a cancel emits nothing and
                    // falls through to `Ambiguous` (exit 3, no `cd`).
                    Ok(Some(path)) => worktrees
                        .iter()
                        .position(|w| same_path(&w.path, &path))
                        .map_or(Resolution::Ambiguous, Resolution::Found),
                    Ok(None) => Resolution::Ambiguous,
                    Err(_) => list_candidates(cx, worktrees, query, &indices),
                }
            } else {
                list_candidates(cx, worktrees, query, &indices)
            }
        }
        Resolved::NotFound => Resolution::NotFound,
    }
}

/// Prints the ambiguous-query candidate list to stderr and returns
/// [`Resolution::Ambiguous`].
fn list_candidates(
    cx: &mut Cx,
    worktrees: &[Worktree],
    query: &str,
    indices: &[usize],
) -> Resolution {
    let _ = cx
        .err
        .line(&format!("query {query:?} is ambiguous; candidates:"));
    for &index in indices {
        let _ = cx
            .err
            .line(&format!("  {}", candidate_label(&worktrees[index])));
    }
    Resolution::Ambiguous
}

/// A human label for a worktree in candidate/diagnostic lists.
pub(crate) fn candidate_label(worktree: &Worktree) -> String {
    match &worktree.branch {
        Some(branch) => branch.clone(),
        None => worktree.path.display().to_string(),
    }
}

/// Whether two paths refer to the same location, comparing canonicalized forms
/// when possible (handles `/private` symlinks on macOS).
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Prompts on stderr and reads a yes/no confirmation (default no).
pub(crate) fn confirm(cx: &mut Cx, prompt: &str) -> Result<bool> {
    cx.err.text(prompt)?;
    cx.err.flush()?;
    let line = cx.input.read_line()?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Prompts on stderr and reads a yes/no confirmation (default *yes*): an empty
/// line or EOF counts as yes, so only an explicit `n`/`no` declines. Callers must
/// gate this on an interactive terminal — a non-interactive EOF would otherwise
/// silently accept.
pub(crate) fn confirm_default_yes(cx: &mut Cx, prompt: &str) -> Result<bool> {
    cx.err.text(prompt)?;
    cx.err.flush()?;
    let line = cx.input.read_line()?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer != "n" && answer != "no")
}

/// A three-way answer to the stale-base prompt (issue #56): update the base,
/// proceed off it as-is, or cancel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Choice {
    /// Update the base (fast-forward it to its upstream) before proceeding.
    Update,
    /// Proceed off the base as-is.
    Proceed,
    /// Abort the operation.
    Cancel,
}

/// Prompts on stderr for an update/proceed/cancel choice (issue #56). Anything
/// other than an explicit update/proceed — including an empty line or EOF — is
/// `Cancel`, so a non-interactive run defaults to the safe choice.
pub(crate) fn choose(cx: &mut Cx, prompt: &str) -> Result<Choice> {
    cx.err.text(prompt)?;
    cx.err.flush()?;
    let line = cx.input.read_line()?;
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "u" | "update" => Choice::Update,
        "p" | "proceed" => Choice::Proceed,
        _ => Choice::Cancel,
    })
}

/// Initializes git submodules in `dir` when enabled, after a worktree is created
/// or a branch is checked out (issue #50). `flag_override` is the resolved
/// `--init-submodules`/`--no-init-submodules` choice and wins over `policy`;
/// without a flag, `policy` decides — only `[submodules] init = "always"` runs
/// here (`prompt`/`never` are skips, so this is the non-interactive fallback for
/// the [`Prompt`](SubmoduleInit::Prompt) default). Disabled is the common case
/// and costs nothing — no `git` runs. A failure is non-fatal: the worktree
/// already exists, so it is surfaced as a warning rather than propagated.
pub(crate) fn maybe_init_submodules(
    cx: &mut Cx,
    git: &dyn GitCli,
    dir: &Path,
    policy: SubmoduleInit,
    flag_override: Option<bool>,
) -> Result<()> {
    let enabled = flag_override.unwrap_or(matches!(policy, SubmoduleInit::Always));
    if !enabled {
        return Ok(());
    }
    let pending = crate::git::submodule::uninitialized(git, dir)?;
    if pending.is_empty() {
        return Ok(());
    }
    init_submodules(cx, git, dir, pending.len());
    Ok(())
}

/// Initializes git submodules in `dir`, prompting first at an interactive
/// terminal when the policy is left at its [`Prompt`](SubmoduleInit::Prompt)
/// default (no `--init-submodules`/`--no-init-submodules` flag). An explicit flag
/// or an `always`/`never` policy decides without a prompt, deferring to
/// [`maybe_init_submodules`]. `prompt` is the caller's interactivity gate (the
/// CLI passes `true`; the TUI passes `false` and handles its own modal), so a
/// prompt only fires for `prompt && stderr.is_tty()`. Non-fatal throughout.
pub(crate) fn maybe_init_submodules_interactive(
    cx: &mut Cx,
    git: &dyn GitCli,
    dir: &Path,
    policy: SubmoduleInit,
    flag_override: Option<bool>,
    prompt: bool,
) -> Result<()> {
    // An explicit flag or an always/never policy decides without asking.
    if flag_override.is_some() || !matches!(policy, SubmoduleInit::Prompt) {
        return maybe_init_submodules(cx, git, dir, policy, flag_override);
    }
    // The Prompt default: only ask interactively; non-interactively leave them be.
    if !(prompt && cx.err.is_tty()) {
        return Ok(());
    }
    let pending = crate::git::submodule::uninitialized(git, dir)?;
    if pending.is_empty() {
        return Ok(());
    }
    let ask = format!(
        "repository has {} uninitialized submodule(s); initialize recursively (`git submodule update --init --recursive`)? [Y/n] ",
        pending.len()
    );
    if confirm_default_yes(cx, &ask)? {
        init_submodules(cx, git, dir, pending.len());
    }
    Ok(())
}

/// Runs `git submodule update --init --recursive` in `dir`, logging a heads-up on
/// stderr first (never stdout — the `cd` path must stay clean) since a submodule
/// clone can block for a while. A failure is surfaced as a warning, not an error.
fn init_submodules(cx: &mut Cx, git: &dyn GitCli, dir: &Path, count: usize) {
    let _ = cx.err.line(&format!("initializing {count} submodule(s)…"));
    if let Err(e) = crate::git::submodule::update_init(git, dir) {
        let _ = cx
            .err
            .line(&format!("warning: failed to initialize submodules: {e}"));
    }
}

/// The git directory used for the `.git`-containment check (spec §6).
pub(crate) fn git_dir_of(root: &Path, is_bare: bool) -> PathBuf {
    if is_bare {
        root.to_path_buf()
    } else {
        root.join(".git")
    }
}

/// Renders the worktree store path for a branch with the given slug (spec §6).
pub(crate) fn render_target(
    config: &Config,
    root: &Path,
    branch: &str,
    slug: &str,
    env: &Env,
) -> Result<PathBuf> {
    let vars = TemplateVars {
        repo_parent: root
            .parent()
            .map_or_else(|| root.to_path_buf(), Path::to_path_buf),
        repo: root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        repo_root: root.to_path_buf(),
        branch: branch.to_string(),
        branch_slug: slug.to_string(),
        home: env
            .get("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("~")),
    };
    template::render(&config.path_template, &vars)
}

/// Resolves the final target path: renders it, rejects the `.git` directory, and
/// on collision with an unrelated path appends `-<short_hash>` (erroring if both
/// are occupied). Spec §6.
pub(crate) fn resolve_target(
    config: &Config,
    root: &Path,
    branch: &str,
    slug: &str,
    short_hash: &str,
    env: &Env,
    is_bare: bool,
) -> Result<PathBuf> {
    let target = render_target(config, root, branch, slug, env)?;
    template::ensure_outside_git(&target, &git_dir_of(root, is_bare))?;
    if !target.exists() {
        return Ok(target);
    }
    let alt = render_target(config, root, branch, &format!("{slug}-{short_hash}"), env)?;
    if alt.exists() {
        return Err(Error::operation(format!(
            "target path already exists: {}",
            target.display()
        )));
    }
    Ok(alt)
}

/// Runs a best-effort cleanup git command: on failure it logs a breadcrumb and
/// continues rather than aborting the caller. Used by the rollback and prune
/// cleanup paths, where a failed step must not stop the wider operation. `step`
/// is a short label identifying the command in the log.
pub(crate) fn run_best_effort(git: &dyn GitCli, root: &Path, args: &[&str], step: &str) {
    match git.run_raw(root, args) {
        Ok(out) if out.success => {}
        Ok(out) => {
            tracing::debug!(step, stderr = %out.stderr.trim(), "best-effort cleanup step failed");
        }
        Err(error) => {
            tracing::debug!(step, %error, "best-effort cleanup step could not run");
        }
    }
}

/// Rolls back a partially-created worktree (spec §13): removes the worktree and
/// prunes, optionally deletes the branch (only when it was created here), and
/// optionally clears the `wt.*` metadata written during the operation, so
/// nothing half-created is left behind. The two flags are independent: `wt pr`
/// on a *pre-existing* branch keeps the branch but still clears the metadata it
/// wrote. Best-effort.
pub(crate) fn rollback_worktree(
    git: &dyn GitCli,
    root: &Path,
    target: &Path,
    branch: &str,
    delete_branch: bool,
    clear_metadata: bool,
) {
    let target_str = target.to_string_lossy();
    run_best_effort(
        git,
        root,
        &["worktree", "remove", "--force", &target_str],
        "rollback: worktree remove",
    );
    run_best_effort(
        git,
        root,
        &["worktree", "prune"],
        "rollback: worktree prune",
    );
    if delete_branch {
        run_best_effort(
            git,
            root,
            &["branch", "-D", branch],
            "rollback: branch delete",
        );
    }
    if clear_metadata {
        // Remove the metadata written before the failure (else a later worktree
        // on this branch name would show stale PR/base info, or a wrongly-set
        // `createdByWt` could cause its branch to be deleted on remove).
        let _ = crate::config::wtconfig::clear_meta(git, root, branch);
    }
}

/// Builds the [`Worktree`] row for `target` (for `--json` results).
pub(crate) fn build_target_row(cx: &Cx, target: &Path) -> Result<Worktree> {
    let git = cx.git.clone();
    let repo = Repo::discover(&cx.cwd)?;
    let worktrees = build_worktrees(&repo, git.as_ref())?;
    worktrees
        .into_iter()
        .find(|w| same_path(&w.path, target))
        .ok_or_else(|| Error::operation("created worktree not found"))
}

/// Logs the copy step's outcome to stderr at `-v` (spec §8: copied files and
/// files skipped because the target already exists are silent by default and
/// logged at verbose).
pub(crate) fn log_copy_outcome(cx: &mut Cx, outcome: &crate::copy::CopyOutcome) {
    if cx.verbose == 0 {
        return;
    }
    for path in &outcome.copied {
        let _ = cx.err.line(&format!("copied {}", path.display()));
    }
    for path in &outcome.skipped_existing {
        let _ = cx
            .err
            .line(&format!("skipped (target exists) {}", path.display()));
    }
}

/// Emits a navigation result: JSON object, the bare path (for `cd`), or a stderr
/// note when `--no-switch` (spec §5/§7).
pub(crate) fn emit_worktree(
    cx: &mut Cx,
    target: &Path,
    json: bool,
    no_switch: bool,
    note: &str,
) -> Result<u8> {
    if json {
        let row = build_target_row(cx, target)?;
        cx.out.line(&row.to_json_line()?)?;
    } else if no_switch {
        cx.err.line(&format!("{note} {}", target.display()))?;
    } else {
        cx.out.line(&target.to_string_lossy())?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{Resolution, resolve_query_with};
    use crate::cx::Stream;
    use crate::model::Worktree;
    use crate::testutil::{SharedBuf, test_cx};
    use std::path::PathBuf;

    /// A worktree row carrying a branch (and its slug), as the resolver matches on.
    fn wt(branch: &str) -> Worktree {
        let slug = branch.replace('/', "-");
        let mut w = Worktree::new(PathBuf::from(format!("/r/{slug}")));
        w.branch = Some(branch.to_string());
        w.slug = Some(slug);
        w
    }

    fn worktrees() -> Vec<Worktree> {
        vec![wt("main"), wt("feature/login"), wt("feature/logout")]
    }

    /// Wires a [`crate::testutil::TestCx`] whose stderr reports the given TTY
    /// status, returning the cx and a handle to read what was written to stderr.
    fn cx_with_err_tty(is_tty: bool) -> (crate::testutil::TestCx, SharedBuf) {
        let mut t = test_cx(&[], "/work");
        let err = SharedBuf::new();
        t.cx.err = Stream::new(Box::new(err.clone()), is_tty);
        (t, err)
    }

    #[test]
    fn unique_match_is_found() {
        let (mut t, _err) = cx_with_err_tty(false);
        let r = resolve_query_with(&mut t.cx, &worktrees(), "main", |_, _| {
            panic!("picker must not run for a unique match")
        });
        assert!(matches!(r, Resolution::Found(0)));
    }

    #[test]
    fn no_match_is_not_found() {
        let (mut t, _err) = cx_with_err_tty(true);
        let r = resolve_query_with(&mut t.cx, &worktrees(), "zzz", |_, _| {
            panic!("picker must not run when nothing matches")
        });
        assert!(matches!(r, Resolution::NotFound));
    }

    #[test]
    fn non_tty_ambiguous_lists_candidates() {
        let (mut t, err) = cx_with_err_tty(false);
        let r = resolve_query_with(&mut t.cx, &worktrees(), "feature/log", |_, _| {
            panic!("picker must not run when stderr is not a TTY")
        });
        assert!(matches!(r, Resolution::Ambiguous));
        let out = err.contents();
        assert!(out.contains("ambiguous"));
        assert!(out.contains("feature/login"));
        assert!(out.contains("feature/logout"));
    }

    #[test]
    fn tty_picker_selection_maps_to_index() {
        let (mut t, err) = cx_with_err_tty(true);
        let wts = worktrees();
        let chosen = wts[2].path.clone();
        let r = resolve_query_with(&mut t.cx, &wts, "feature/log", move |_, q| {
            assert_eq!(q, "feature/log");
            Ok(Some(chosen))
        });
        assert!(matches!(r, Resolution::Found(2)));
        // The picker showed the choices; no candidate list is printed.
        assert!(err.contents().is_empty());
    }

    #[test]
    fn tty_picker_cancel_is_ambiguous_without_output() {
        let (mut t, err) = cx_with_err_tty(true);
        let r = resolve_query_with(&mut t.cx, &worktrees(), "feature/log", |_, _| Ok(None));
        assert!(matches!(r, Resolution::Ambiguous));
        assert!(err.contents().is_empty());
    }

    #[test]
    fn tty_picker_error_falls_back_to_listing() {
        let (mut t, err) = cx_with_err_tty(true);
        let r = resolve_query_with(&mut t.cx, &worktrees(), "feature/log", |_, _| {
            Err(crate::error::Error::operation("boom"))
        });
        assert!(matches!(r, Resolution::Ambiguous));
        assert!(err.contents().contains("ambiguous"));
        assert!(err.contents().contains("feature/login"));
    }

    #[test]
    fn tty_picker_unknown_path_is_ambiguous() {
        let (mut t, err) = cx_with_err_tty(true);
        let r = resolve_query_with(&mut t.cx, &worktrees(), "feature/log", |_, _| {
            Ok(Some(PathBuf::from("/somewhere/else")))
        });
        assert!(matches!(r, Resolution::Ambiguous));
        assert!(err.contents().is_empty());
    }

    #[test]
    fn confirm_default_yes_treats_empty_as_yes() {
        use crate::commands::confirm_default_yes;
        use crate::testutil::CannedInput;
        let cases = [
            ("", true),
            ("y", true),
            ("yes", true),
            ("anything", true),
            ("n", false),
            ("N", false),
            ("no", false),
        ];
        for (answer, expected) in cases {
            let mut t = test_cx(&[], "/work");
            t.cx.input = Box::new(CannedInput::new(&[answer]));
            assert_eq!(
                confirm_default_yes(&mut t.cx, "? ").unwrap(),
                expected,
                "answer {answer:?}"
            );
        }
    }

    #[test]
    fn choose_maps_answers_and_defaults_to_cancel() {
        use crate::commands::{Choice, choose};
        use crate::testutil::CannedInput;
        let cases = [
            ("update", Choice::Update),
            ("u", Choice::Update),
            ("proceed", Choice::Proceed),
            ("p", Choice::Proceed),
            ("", Choice::Cancel),
            ("nonsense", Choice::Cancel),
        ];
        for (answer, expected) in cases {
            let mut t = test_cx(&[], "/work");
            t.cx.input = Box::new(CannedInput::new(&[answer]));
            assert_eq!(choose(&mut t.cx, "? ").unwrap(), expected);
        }
    }

    mod submodules {
        use super::cx_with_err_tty;
        use crate::commands::{maybe_init_submodules, maybe_init_submodules_interactive};
        use crate::config::SubmoduleInit;
        use crate::git::cli::{GitCli, GitOutput, RealGit};
        use crate::git::submodule::uninitialized;
        use crate::testutil::{CannedInput, TestRepo};
        use std::path::Path;

        /// A repo with one submodule deinitialized, so it reports as uninitialized
        /// but `update --init` can reuse `.git/modules` (no file-protocol clone).
        fn repo_with_uninitialized_submodule() -> TestRepo {
            let repo = TestRepo::init();
            repo.add_submodule("libs/sub");
            repo.deinit_submodule("libs/sub");
            repo
        }

        fn is_initialized(repo: &TestRepo) -> bool {
            repo.root().join("libs/sub/sub.txt").exists()
        }

        #[test]
        fn disabled_policy_is_a_noop() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(true);
            maybe_init_submodules(&mut t.cx, &RealGit, repo.root(), SubmoduleInit::Never, None)
                .unwrap();
            assert!(!is_initialized(&repo));
            assert!(err.contents().is_empty());
        }

        #[test]
        fn always_policy_initializes() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(true);
            maybe_init_submodules(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Always,
                None,
            )
            .unwrap();
            assert!(is_initialized(&repo));
            assert!(uninitialized(&RealGit, repo.root()).unwrap().is_empty());
            assert!(err.contents().contains("initializing 1 submodule"));
        }

        #[test]
        fn flag_override_forces_init_over_never() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, _err) = cx_with_err_tty(true);
            maybe_init_submodules(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Never,
                Some(true),
            )
            .unwrap();
            assert!(is_initialized(&repo));
        }

        #[test]
        fn flag_override_forces_skip_over_always() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, _err) = cx_with_err_tty(true);
            maybe_init_submodules(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Always,
                Some(false),
            )
            .unwrap();
            assert!(!is_initialized(&repo));
        }

        #[test]
        fn enabled_with_no_submodules_is_a_noop() {
            let repo = TestRepo::init();
            let (mut t, err) = cx_with_err_tty(true);
            maybe_init_submodules(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Always,
                None,
            )
            .unwrap();
            assert!(err.contents().is_empty());
        }

        /// A `git` whose `submodule status` reports one uninitialized submodule but
        /// whose `submodule update` (routed through `run`) fails.
        struct StatusOkUpdateFails;
        impl GitCli for StatusOkUpdateFails {
            fn run_raw(&self, _repo: &Path, _args: &[&str]) -> crate::error::Result<GitOutput> {
                Ok(GitOutput {
                    success: true,
                    stdout: "-deadbeef libs/sub\n".into(),
                    stderr: String::new(),
                })
            }
            fn run(&self, _repo: &Path, _args: &[&str]) -> crate::error::Result<String> {
                Err(crate::error::Error::operation("boom"))
            }
        }

        #[test]
        fn prompt_default_yes_empty_initializes() {
            // The Prompt default at a TTY asks, and an empty answer (the default)
            // initializes (issue #50).
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(true);
            t.cx.input = Box::new(CannedInput::new(&[""]));
            maybe_init_submodules_interactive(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Prompt,
                None,
                true,
            )
            .unwrap();
            assert!(is_initialized(&repo));
            assert!(err.contents().contains("uninitialized submodule"));
        }

        #[test]
        fn prompt_no_leaves_uninitialized() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(true);
            t.cx.input = Box::new(CannedInput::new(&["n"]));
            maybe_init_submodules_interactive(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Prompt,
                None,
                true,
            )
            .unwrap();
            assert!(!is_initialized(&repo));
            // The prompt was shown, but nothing was initialized.
            assert!(err.contents().contains("uninitialized submodule"));
            assert!(!err.contents().contains("initializing"));
        }

        #[test]
        fn prompt_with_no_submodules_does_not_ask() {
            let repo = TestRepo::init();
            let (mut t, err) = cx_with_err_tty(true);
            maybe_init_submodules_interactive(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Prompt,
                None,
                true,
            )
            .unwrap();
            assert!(err.contents().is_empty());
        }

        #[test]
        fn prompt_non_tty_is_a_silent_skip() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(false);
            maybe_init_submodules_interactive(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Prompt,
                None,
                true,
            )
            .unwrap();
            assert!(!is_initialized(&repo));
            assert!(err.contents().is_empty());
        }

        #[test]
        fn prompt_false_gate_skips_even_at_a_tty() {
            // The TUI passes `prompt = false` and drives its own modal; the inline
            // helper must not prompt or init.
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(true);
            maybe_init_submodules_interactive(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Prompt,
                None,
                false,
            )
            .unwrap();
            assert!(!is_initialized(&repo));
            assert!(err.contents().is_empty());
        }

        #[test]
        fn flag_skip_overrides_prompt_without_asking() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(true);
            maybe_init_submodules_interactive(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Prompt,
                Some(false),
                true,
            )
            .unwrap();
            assert!(!is_initialized(&repo));
            assert!(err.contents().is_empty());
        }

        #[test]
        fn flag_init_overrides_prompt_without_asking() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(true);
            maybe_init_submodules_interactive(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Prompt,
                Some(true),
                true,
            )
            .unwrap();
            assert!(is_initialized(&repo));
            // Initialized directly, with no `[Y/n]` prompt.
            assert!(err.contents().contains("initializing"));
            assert!(!err.contents().contains("uninitialized submodule"));
        }

        #[test]
        fn never_policy_does_not_ask_at_a_tty() {
            let repo = repo_with_uninitialized_submodule();
            let (mut t, err) = cx_with_err_tty(true);
            maybe_init_submodules_interactive(
                &mut t.cx,
                &RealGit,
                repo.root(),
                SubmoduleInit::Never,
                None,
                true,
            )
            .unwrap();
            assert!(!is_initialized(&repo));
            assert!(err.contents().is_empty());
        }

        #[test]
        fn update_failure_is_non_fatal_and_warns() {
            let (mut t, err) = cx_with_err_tty(true);
            // Returns Ok despite the failed update; the error is surfaced as a warning.
            maybe_init_submodules(
                &mut t.cx,
                &StatusOkUpdateFails,
                Path::new("/work"),
                SubmoduleInit::Always,
                None,
            )
            .unwrap();
            let out = err.contents();
            assert!(out.contains("initializing 1 submodule"));
            assert!(out.contains("warning: failed to initialize submodules"));
            assert!(out.contains("boom"));
        }
    }
}
