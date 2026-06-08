//! Command handlers (spec §7). Each module implements one subcommand; this
//! module provides the shared repository session setup and query resolution.

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
pub mod status_cmd;
pub mod switch;

use std::path::{Path, PathBuf};

use crate::config::{self, Config};
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
pub struct Session {
    /// The discovered repository (gix handle).
    pub repo: Repo,
    /// The primary worktree root (or bare repo path).
    pub primary_root: PathBuf,
    /// The merged configuration.
    pub config: Config,
}

/// Discovers the repository from the context's working directory, resolves the
/// primary root via `git rev-parse`, and loads the merged configuration.
pub fn open_session(cx: &Cx, git: &dyn GitCli) -> Result<Session> {
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
pub enum Resolution {
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
pub fn resolve_query(cx: &mut Cx, worktrees: &[Worktree], query: &str) -> Resolution {
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
pub fn candidate_label(worktree: &Worktree) -> String {
    match &worktree.branch {
        Some(branch) => branch.clone(),
        None => worktree.path.display().to_string(),
    }
}

/// Whether two paths refer to the same location, comparing canonicalized forms
/// when possible (handles `/private` symlinks on macOS).
pub fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Prompts on stderr and reads a yes/no confirmation (default no).
pub fn confirm(cx: &mut Cx, prompt: &str) -> Result<bool> {
    cx.err.text(prompt)?;
    cx.err.flush()?;
    let line = cx.input.read_line()?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// The git directory used for the `.git`-containment check (spec §6).
pub fn git_dir_of(root: &Path, is_bare: bool) -> PathBuf {
    if is_bare {
        root.to_path_buf()
    } else {
        root.join(".git")
    }
}

/// Renders the worktree store path for a branch with the given slug (spec §6).
pub fn render_target(
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
pub fn resolve_target(
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

/// Rolls back a partially-created worktree (spec §13): removes the worktree and
/// prunes, optionally deletes the branch (only when it was created here), and
/// optionally clears the `wt.*` metadata written during the operation, so
/// nothing half-created is left behind. The two flags are independent: `wt pr`
/// on a *pre-existing* branch keeps the branch but still clears the metadata it
/// wrote. Best-effort.
pub fn rollback_worktree(
    git: &dyn GitCli,
    root: &Path,
    target: &Path,
    branch: &str,
    delete_branch: bool,
    clear_metadata: bool,
) {
    let target_str = target.to_string_lossy();
    let _ = git.run_raw(root, &["worktree", "remove", "--force", &target_str]);
    let _ = git.run_raw(root, &["worktree", "prune"]);
    if delete_branch {
        let _ = git.run_raw(root, &["branch", "-D", branch]);
    }
    if clear_metadata {
        // Remove the metadata written before the failure (else a later worktree
        // on this branch name would show stale PR/base info, or a wrongly-set
        // `createdByWt` could cause its branch to be deleted on remove).
        let _ = crate::config::wtconfig::clear_meta(git, root, branch);
    }
}

/// Builds the [`Worktree`] row for `target` (for `--json` results).
pub fn build_target_row(cx: &Cx, target: &Path) -> Result<Worktree> {
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
pub fn log_copy_outcome(cx: &mut Cx, outcome: &crate::copy::CopyOutcome) {
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
pub fn emit_worktree(
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
}
