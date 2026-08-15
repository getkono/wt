//! The stateless worktree service (issue #95): discover, enumerate, create,
//! and remove worktrees non-interactively.
//!
//! Nothing here prompts, reads stdin, or writes to stdout/stderr. Operations
//! take injected [`GitCli`] and [`HookRunner`] handles, return typed errors
//! from [`crate::error::Error`], and report side observations (hook failures,
//! submodule outcomes, copied files) as data on the outcome structs so callers
//! decide how to present them. The CLI commands and the TUI wrap this service
//! with prompting and rendering; embedders call it directly.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::wtconfig::WtMeta;
use crate::config::{self, Config, wtconfig};
use crate::copy::{CopyOutcome, copy_ignored_files};
use crate::cx::Env;
use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::discover::Repo;
use crate::git::{branch_ref, default_branch, is_ancestor, ops, resolve_hex};
use crate::hooks::{HookContext, HookRunner};
use crate::model::Worktree;
use crate::query::{self, Resolved};
use crate::slug::slugify_with_fallback;
use crate::template::{self, TemplateVars};
use crate::worktree::rows;

/// How long a mutation waits for the advisory repository lock (issue #99)
/// before failing with [`Error::LockUnavailable`].
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// A held advisory repository lock (issue #99): while alive, no other `wt` (or
/// embedder going through this library) can mutate worktrees or `wt.*`
/// metadata in the repository. Released on drop.
///
/// [`Workspace::create`] and [`Workspace::remove`] take the lock internally
/// around their mutation regions (hooks run *outside* it, so a hook that
/// re-enters `wt` cannot deadlock) — do not hold a `RepoLock` while calling
/// them. Take one directly to make your own read-check-write sequence over
/// `wt.*` metadata atomic against concurrent writers.
pub struct RepoLock {
    _marker: gix_lock::Marker,
}

/// The directory holding the advisory lock file: the repository's common git
/// directory, shared by every linked worktree (`.git` of the primary worktree,
/// or the repository itself when bare).
fn lock_dir(root: &Path) -> PathBuf {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        dot_git
    } else {
        root.to_path_buf()
    }
}

/// Acquires the repo-level advisory mutation lock, waiting (with backoff) up
/// to `timeout` for a concurrent holder to finish.
pub(crate) fn acquire_repo_lock(root: &Path, timeout: Duration) -> Result<RepoLock> {
    let resource = lock_dir(root).join("wt-mutation");
    let marker = gix_lock::Marker::acquire_to_hold_resource(
        &resource,
        gix_lock::acquire::Fail::AfterDurationWithBackoff(timeout),
        None,
    )
    .map_err(|e| Error::LockUnavailable {
        path: format!("{}.lock", resource.display()),
        reason: e.to_string(),
    })?;
    Ok(RepoLock { _marker: marker })
}

/// A discovered repository with its resolved configuration and environment
/// snapshot: the entry point of the stateless worktree API.
pub struct Workspace {
    repo: Repo,
    primary_root: PathBuf,
    config: Config,
    env: Env,
}

/// Borrowed workspace state threaded through the service functions, so the CLI
/// (which owns the same parts inside its `Session`) can call them without
/// constructing a [`Workspace`].
pub(crate) struct WorkspaceParts<'a> {
    /// The discovered repository.
    pub(crate) repo: &'a Repo,
    /// The merged configuration.
    pub(crate) config: &'a Config,
    /// The primary worktree root (or bare repo path).
    pub(crate) root: &'a Path,
    /// The environment snapshot (template `{home}` expansion).
    pub(crate) env: &'a Env,
}

impl Workspace {
    /// Discovers the repository containing `dir`, resolves the primary worktree
    /// root, and loads the merged configuration. Returns
    /// [`Error::NotInRepo`] when `dir` is not inside a Git repository.
    pub fn discover(dir: &Path, env: &Env, git: &dyn GitCli) -> Result<Workspace> {
        let repo = Repo::discover(dir)?;
        let workdir = repo.current_workdir().unwrap_or_else(|| repo.git_dir());
        // `gix`'s common-dir resolution is unreliable through linked worktrees,
        // so the primary root comes from `git rev-parse` (spec §4).
        let common = git.run(
            &workdir,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = PathBuf::from(common.trim());
        let primary_root = if repo.is_bare() {
            common
        } else {
            common.parent().map(Path::to_path_buf).unwrap_or(common)
        };
        let config = config::load(Some(&primary_root), env)?;
        // Refuse a repository stamped with a future metadata schema up front
        // (issue #99); reading it could silently misinterpret `wt.*` keys.
        wtconfig::ensure_schema_supported(repo.gix())?;
        Ok(Workspace {
            repo,
            primary_root,
            config,
            env: env.clone(),
        })
    }

    /// Acquires the repository's advisory mutation lock (issue #99). See
    /// [`RepoLock`] for the holding rules.
    pub fn lock(&self) -> Result<RepoLock> {
        acquire_repo_lock(&self.primary_root, LOCK_TIMEOUT)
    }

    /// The primary worktree root (or the repository path when bare). This is
    /// the `repo_root` expected by the [`wtconfig`] write functions.
    pub fn root(&self) -> &Path {
        &self.primary_root
    }

    /// The merged configuration (defaults, global `config.toml`, repo
    /// `.wt.toml`).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether the primary repository is bare.
    pub fn is_bare(&self) -> bool {
        self.repo.is_bare()
    }

    /// Enumerates worktrees with their synchronous fields only (path, branch,
    /// slug, current/main/missing/detached markers).
    pub fn enumerate(&self, git: &dyn GitCli) -> Result<Vec<Worktree>> {
        rows::enumerate_worktrees(&self.fresh_repo()?, git)
    }

    /// Enumerates worktrees fully enriched: dirty/untracked status,
    /// ahead/behind, merge state, tip commits, and the cached PR metadata from
    /// `wt.*` config.
    pub fn list(&self, git: &dyn GitCli) -> Result<Vec<Worktree>> {
        rows::build_worktrees(&self.fresh_repo()?, git)
    }

    /// Reads the `wt.*` metadata recorded for `branch`. Writes go through the
    /// [`wtconfig`] functions with [`Workspace::root`] as the repo root.
    pub fn read_meta(&self, branch: &str) -> Result<WtMeta> {
        Ok(wtconfig::read_meta(self.fresh_repo()?.gix(), branch))
    }

    /// Creates (or reuses) a linked worktree per `options`: resolves the target
    /// from the configured path template, creates the branch off its base when
    /// needed, records `wt.*` metadata, runs the copy step, the `post_create`
    /// hook, and (when requested) submodule initialization. Partial failures
    /// before the hook are rolled back (spec §13).
    pub fn create(
        &self,
        git: &dyn GitCli,
        hooks: &dyn HookRunner,
        options: &CreateOptions,
    ) -> Result<CreatedWorktree> {
        let repo = self.fresh_repo()?;
        create_in(&self.parts(&repo), git, hooks, options)
    }

    /// Removes `worktree` under `options`, enforcing the dirty/unpushed safety
    /// guards (returning [`Error::RemoveGuarded`] when they block), running the
    /// `pre_remove` hook, pruning a missing worktree, and deleting a
    /// fully-merged wt-created branch per the configuration.
    pub fn remove(
        &self,
        git: &dyn GitCli,
        hooks: &dyn HookRunner,
        worktree: &Worktree,
        options: &RemoveOptions,
    ) -> Result<RemovedWorktree> {
        let repo = self.fresh_repo()?;
        remove_in(&self.parts(&repo), git, hooks, worktree, options)
    }

    /// Re-opens the repository for one operation. `gix` snapshots the git
    /// config when a repository is opened, while `wt.*` metadata is written
    /// through the `git` subprocess — a long-lived `Workspace` reading through
    /// the discovery-time handle would see stale metadata, so every operation
    /// reads through a fresh one.
    fn fresh_repo(&self) -> Result<Repo> {
        let dir = self
            .repo
            .current_workdir()
            .unwrap_or_else(|| self.repo.git_dir());
        Repo::discover(&dir)
    }

    /// The borrowed parts view of this workspace over `repo`.
    fn parts<'a>(&'a self, repo: &'a Repo) -> WorkspaceParts<'a> {
        WorkspaceParts {
            repo,
            config: &self.config,
            root: &self.primary_root,
            env: &self.env,
        }
    }

    /// Decomposes into the parts the CLI session keeps.
    #[cfg_attr(not(feature = "cli"), allow(dead_code))]
    pub(crate) fn into_session_parts(self) -> (Repo, PathBuf, Config) {
        (self.repo, self.primary_root, self.config)
    }
}

/// Options for [`Workspace::create`].
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// The branch to check out, created off `base` when it does not exist.
    pub branch: String,
    /// Explicit base ref for a new branch. `None` resolves the configured
    /// `default_base`, then the repository default branch, then `HEAD`.
    /// Ignored when the branch already exists.
    pub base: Option<String>,
    /// Set this ref as the new branch's upstream (`--track`).
    pub track: Option<String>,
    /// Copy-source worktree query (spec §8); `None` copies from the current
    /// worktree (or the primary root).
    pub copy_from: Option<String>,
    /// Initialize uninitialized submodules after creation. The service never
    /// prompts: callers resolve their `submodules.init` policy (and any flag
    /// override) to a boolean first.
    pub init_submodules: bool,
    /// Skip the `post_create` hook.
    pub no_hooks: bool,
}

/// Options for [`Workspace::remove`]. The worktree-removal force is decoupled
/// from the branch-deletion force: the TUI confirm dialog forces removal of a
/// dirty worktree without ever force-deleting an unmerged branch (spec §10/§12).
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveOptions {
    /// Skip the dirty/unpushed guards and pass `--force` to
    /// `git worktree remove`.
    pub force_remove: bool,
    /// Permit deleting a branch that is not fully merged into its base.
    pub force_branch: bool,
    /// Always keep the local branch.
    pub keep_branch: bool,
    /// Skip the `pre_remove` hook.
    pub no_hooks: bool,
}

/// How a hook invocation went, reported as data (the service never prints).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookOutcome {
    /// No hook was configured, hooks were disabled, or the operation's path
    /// does not run the hook (e.g. reusing an existing worktree).
    Skipped,
    /// The hook ran and exited zero.
    Succeeded,
    /// The hook ran and exited with this non-zero status.
    ExitedNonZero(i32),
    /// The hook could not be run at all.
    Failed(String),
}

/// How the post-create submodule initialization went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmodulesOutcome {
    /// Initialization was not requested or nothing was uninitialized.
    Skipped,
    /// This many pending submodules were initialized.
    Initialized(usize),
    /// Initialization was attempted for `pending` submodules but failed.
    /// Non-fatal: the worktree exists and is usable.
    Failed {
        /// How many submodules were uninitialized when the attempt started.
        pending: usize,
        /// The failure, rendered.
        error: String,
    },
}

/// The outcome of [`Workspace::create`].
#[derive(Debug, Clone)]
pub struct CreatedWorktree {
    /// The worktree's path.
    pub path: PathBuf,
    /// The checked-out branch.
    pub branch: String,
    /// The base the branch was created from, or `None` when it already
    /// existed.
    pub base_ref: Option<String>,
    /// Whether an existing worktree at the configured target was reused
    /// instead of created (the idempotent path; no copy step, no hook).
    pub reused: bool,
    /// What the copy step did (spec §8).
    pub copy: CopyOutcome,
    /// How the `post_create` hook went. Never fatal (spec §8).
    pub post_create: HookOutcome,
    /// How submodule initialization went. Never fatal.
    pub submodules: SubmodulesOutcome,
}

/// The outcome of [`Workspace::remove`].
#[derive(Debug, Clone)]
pub struct RemovedWorktree {
    /// Whether the local branch was deleted along with the worktree.
    pub branch_deleted: bool,
    /// Whether the dirty/unpushed guards would have blocked and were
    /// overridden by [`RemoveOptions::force_remove`] — the data-loss-risk case
    /// a caller should surface.
    pub forced_past_guards: bool,
    /// How the `pre_remove` hook went. A failing hook aborts the removal
    /// (with a typed error) unless `force_remove` downgraded it to an outcome
    /// reported here.
    pub pre_remove: HookOutcome,
}

/// Resolves the base ref for a new branch: `explicit`, then the configured
/// `default_base`, then the repository default branch, then `HEAD`. The second
/// element is `true` on the final `HEAD` fallback, which callers may want to
/// surface as a warning.
pub(crate) fn resolve_base(repo: &Repo, config: &Config, explicit: Option<&str>) -> (String, bool) {
    if let Some(explicit) = explicit {
        return (explicit.to_string(), false);
    }
    if let Some(base) = &config.default_base {
        return (base.clone(), false);
    }
    if let Some(branch) = default_branch(repo.gix()) {
        return (branch, false);
    }
    ("HEAD".to_string(), true)
}

/// Creates (or reuses) a worktree per `options`; see [`Workspace::create`].
pub(crate) fn create_in(
    ws: &WorkspaceParts<'_>,
    git: &dyn GitCli,
    hooks: &dyn HookRunner,
    options: &CreateOptions,
) -> Result<CreatedWorktree> {
    wtconfig::ensure_schema_supported(ws.repo.gix())?;
    let branch = options.branch.clone();
    let worktrees = rows::enumerate_worktrees(ws.repo, git)?;
    let branch_exists = resolve_hex(ws.repo.gix(), &branch_ref(&branch)).is_some();

    let base_ref = if branch_exists {
        None
    } else {
        Some(resolve_base(ws.repo, ws.config, options.base.as_deref()).0)
    };
    let base_commit = match &base_ref {
        Some(base) => resolve_hex(ws.repo.gix(), base)
            .ok_or_else(|| Error::operation(format!("base ref {base:?} not found")))?,
        None => resolve_hex(ws.repo.gix(), &branch_ref(&branch)).unwrap_or_default(),
    };
    let short_hash = base_commit.get(..7).unwrap_or(&base_commit).to_string();
    let slug = slugify_with_fallback(&branch, &short_hash);

    // If the branch is already checked out, either reuse (same target) or
    // refuse. The reuse path is idempotent: no copy step, no hook.
    if let Some(existing) = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(branch.as_str()))
    {
        let preview = render_target(ws.config, ws.root, &branch, &slug, ws.env)?;
        if same_path(&existing.path, &preview) {
            return Ok(CreatedWorktree {
                path: existing.path.clone(),
                branch,
                base_ref: None,
                reused: true,
                copy: CopyOutcome::default(),
                post_create: HookOutcome::Skipped,
                submodules: SubmodulesOutcome::Skipped,
            });
        }
        return Err(Error::operation(format!(
            "branch {branch:?} is already checked out at {}",
            existing.path.display()
        )));
    }

    // The mutation region — target resolution through metadata + copy — runs
    // under the advisory repository lock (issue #99) so two concurrent
    // creators cannot interleave into a corrupt state. The lock is released
    // before the hook runs: a hook that re-enters `wt` must not deadlock.
    let lock = acquire_repo_lock(ws.root, LOCK_TIMEOUT)?;
    let target = resolve_target(
        ws.config,
        ws.root,
        &branch,
        &slug,
        &short_hash,
        ws.env,
        ws.repo.is_bare(),
    )?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Create the worktree (git is atomic here).
    let target_str = target.to_string_lossy().into_owned();
    if let Some(base) = &base_ref {
        // `--no-track` keeps the new branch from inheriting the base as its
        // upstream (issue #43); `--track` opts into an explicit one.
        ops::worktree_add_branch(git, ws.root, &branch, &target_str, base, true)?;
    } else {
        ops::worktree_add(git, ws.root, &target_str, &branch)?;
    }

    // Steps after creation but before the hook are rolled back on failure (§13).
    let copy = match post_create_steps(ws, git, &worktrees, &branch, &base_ref, &target, options) {
        Ok(outcome) => outcome,
        Err(e) => {
            // Metadata is written only for a wt-created branch, so delete the
            // branch and clear metadata together on that condition.
            let created = base_ref.is_some();
            rollback_worktree(git, ws.root, &target, &branch, created, created);
            return Err(e);
        }
    };
    drop(lock);

    // The post-create hook: a failure is an outcome, not a rollback (§8).
    let ctx = HookContext {
        worktree_path: target.clone(),
        branch: branch.clone(),
        repo_root: ws.root.to_path_buf(),
        base_ref: base_ref.clone(),
        pr_number: None,
    };
    let post_create = match (options.no_hooks, ws.config.hooks_post_create.as_deref()) {
        (true, _) | (false, None) => HookOutcome::Skipped,
        (false, Some(command)) => match hooks.run(command, &ctx) {
            Ok(0) => HookOutcome::Succeeded,
            Ok(code) => HookOutcome::ExitedNonZero(code),
            Err(e) => HookOutcome::Failed(e.to_string()),
        },
    };

    // Submodule initialization, when the caller resolved its policy to "yes".
    // Non-fatal: the worktree already exists.
    let submodules = if options.init_submodules {
        let pending = crate::git::submodule::uninitialized(git, &target)?;
        if pending.is_empty() {
            SubmodulesOutcome::Skipped
        } else {
            match crate::git::submodule::update_init(git, &target) {
                Ok(()) => SubmodulesOutcome::Initialized(pending.len()),
                Err(e) => SubmodulesOutcome::Failed {
                    pending: pending.len(),
                    error: e.to_string(),
                },
            }
        }
    } else {
        SubmodulesOutcome::Skipped
    };

    Ok(CreatedWorktree {
        path: target,
        branch,
        base_ref,
        reused: false,
        copy,
        post_create,
        submodules,
    })
}

/// Records metadata, sets an explicit upstream, and runs the copy step — the
/// region rolled back when any step fails.
fn post_create_steps(
    ws: &WorkspaceParts<'_>,
    git: &dyn GitCli,
    worktrees: &[Worktree],
    branch: &str,
    base_ref: &Option<String>,
    target: &Path,
    options: &CreateOptions,
) -> Result<CopyOutcome> {
    if let Some(base) = base_ref {
        // A wt-created branch records its base and "created by wt" (§3/§10).
        wtconfig::write_base_ref(git, ws.root, branch, base)?;
        wtconfig::mark_created_by_wt(git, ws.root, branch)?;
    }
    // `--track <REF>` sets an explicit upstream (issue #43); a bad ref fails
    // here, inside the rolled-back region.
    if let Some(upstream) = &options.track {
        ops::set_upstream(git, ws.root, branch, upstream)?;
    }
    let source = copy_source(ws, worktrees, options.copy_from.as_deref())?;
    copy_ignored_files(git, &source, target, &ws.config.copy)
}

/// Resolves the copy source worktree: the `copy_from` query, else the current
/// worktree, else the primary root (spec §8).
fn copy_source(
    ws: &WorkspaceParts<'_>,
    worktrees: &[Worktree],
    copy_from: Option<&str>,
) -> Result<PathBuf> {
    if let Some(q) = copy_from {
        return match query::resolve(worktrees, q) {
            Resolved::One(index) => Ok(worktrees[index].path.clone()),
            Resolved::Ambiguous(_) => {
                Err(Error::operation(format!("--copy-from {q:?} is ambiguous")))
            }
            Resolved::NotFound => Err(Error::NotFound {
                query: q.to_string(),
            }),
        };
    }
    Ok(ws
        .repo
        .current_workdir()
        .unwrap_or_else(|| ws.root.to_path_buf()))
}

/// Removes an already-resolved `worktree`; see [`Workspace::remove`].
pub(crate) fn remove_in(
    ws: &WorkspaceParts<'_>,
    git: &dyn GitCli,
    hooks: &dyn HookRunner,
    worktree: &Worktree,
    options: &RemoveOptions,
) -> Result<RemovedWorktree> {
    wtconfig::ensure_schema_supported(ws.repo.gix())?;
    if worktree.is_main {
        return Err(Error::operation("refusing to remove the primary worktree"));
    }
    let meta = worktree
        .branch
        .as_deref()
        .map(|b| wtconfig::read_meta(ws.repo.gix(), b))
        .unwrap_or_default();
    let default = default_branch(ws.repo.gix());

    // A missing worktree: prune the admin record; no guards or hook apply.
    if worktree.is_missing {
        let _lock = acquire_repo_lock(ws.root, LOCK_TIMEOUT)?;
        ops::worktree_prune(git, ws.root)?;
        let branch_deleted = maybe_delete_branch(ws, git, worktree, &meta, options, &default);
        clear_metadata(git, ws.root, worktree);
        return Ok(RemovedWorktree {
            branch_deleted,
            forced_past_guards: false,
            pre_remove: HookOutcome::Skipped,
        });
    }

    // Safety guards (spec §10/§12).
    let guard = rows::guard_status(worktree, ws.config.remove_untracked_blocks);
    if guard.blocks() && !options.force_remove {
        return Err(Error::RemoveGuarded {
            dirty: guard.dirty,
            unpushed: guard.unpushed,
        });
    }
    let forced_past_guards = guard.blocks() && options.force_remove;

    // The pre-remove hook may abort; `force_remove` downgrades a failure to an
    // outcome and proceeds.
    let ctx = HookContext {
        worktree_path: worktree.path.clone(),
        branch: worktree.branch.clone().unwrap_or_default(),
        repo_root: ws.root.to_path_buf(),
        base_ref: meta.base_ref.clone(),
        pr_number: meta.pr_number,
    };
    let pre_remove = match (options.no_hooks, ws.config.hooks_pre_remove.as_deref()) {
        (true, _) | (false, None) => HookOutcome::Skipped,
        (false, Some(command)) => match hooks.run(command, &ctx) {
            Ok(0) => HookOutcome::Succeeded,
            Ok(code) if options.force_remove => HookOutcome::ExitedNonZero(code),
            Ok(code) => {
                return Err(Error::operation(format!(
                    "pre_remove hook exited with status {code}; aborting (use --force to override)"
                )));
            }
            Err(e) if options.force_remove => HookOutcome::Failed(e.to_string()),
            Err(e) => return Err(e),
        },
    };

    // Remove the worktree, holding the advisory lock (issue #99). Acquired
    // *after* the hook so a hook that re-enters `wt` cannot deadlock.
    let _lock = acquire_repo_lock(ws.root, LOCK_TIMEOUT)?;
    let path = worktree.path.to_string_lossy().into_owned();
    ops::worktree_remove(git, ws.root, &path, options.force_remove)?;

    let branch_deleted = maybe_delete_branch(ws, git, worktree, &meta, options, &default);
    clear_metadata(git, ws.root, worktree);
    Ok(RemovedWorktree {
        branch_deleted,
        forced_past_guards,
        pre_remove,
    })
}

/// Deletes the branch if it is wt-created and either fully merged (and the
/// config allows it) or `force_branch` (for an unmerged branch). Returns
/// whether the branch was deleted.
fn maybe_delete_branch(
    ws: &WorkspaceParts<'_>,
    git: &dyn GitCli,
    worktree: &Worktree,
    meta: &WtMeta,
    options: &RemoveOptions,
    default: &Option<String>,
) -> bool {
    let Some(branch) = &worktree.branch else {
        return false;
    };
    if options.keep_branch || !meta.created_by_wt {
        return false;
    }
    let base = meta.base_ref.clone().or_else(|| default.clone());
    let merged = base
        .as_deref()
        .is_some_and(|b| is_ancestor(ws.repo.gix(), &branch_ref(branch), b));
    let should_delete = if merged {
        ws.config.remove_delete_merged_branch
    } else {
        options.force_branch
    };
    if !should_delete {
        return false;
    }
    ops::delete_branch(git, ws.root, branch, true).is_ok()
}

/// Clears the worktree's `wt.*` metadata, best-effort.
fn clear_metadata(git: &dyn GitCli, root: &Path, worktree: &Worktree) {
    if let Some(branch) = &worktree.branch {
        let _ = wtconfig::clear_meta(git, root, branch);
    }
}

/// Whether two paths refer to the same location, comparing canonicalized forms
/// when possible (handles `/private` symlinks on macOS).
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
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

/// Resolves the final target path: renders it, rejects the `.git` directory,
/// and on collision with an unrelated path appends `-<short_hash>` (erroring if
/// both are occupied). Spec §6.
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
    clear_meta: bool,
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
    if clear_meta {
        // Remove the metadata written before the failure (else a later worktree
        // on this branch name would show stale PR/base info, or a wrongly-set
        // `createdByWt` could cause its branch to be deleted on remove).
        let _ = wtconfig::clear_meta(git, root, branch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::hooks::RealHookRunner;
    use crate::testutil::TestRepo;
    use std::collections::HashMap;

    fn env() -> Env {
        Env::from_map(HashMap::new())
    }

    fn workspace(repo: &TestRepo) -> Workspace {
        Workspace::discover(repo.root(), &env(), &RealGit).unwrap()
    }

    fn create_opts(branch: &str) -> CreateOptions {
        CreateOptions {
            branch: branch.to_string(),
            no_hooks: true,
            ..Default::default()
        }
    }

    #[test]
    fn discover_resolves_root_config_and_bareness() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        assert!(!ws.is_bare());
        assert_eq!(
            std::fs::canonicalize(ws.root()).unwrap(),
            std::fs::canonicalize(repo.root()).unwrap()
        );
        assert_eq!(ws.config().pr_default_remote, "origin");
    }

    #[test]
    fn discover_outside_a_repo_is_not_in_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Workspace::discover(dir.path(), &env(), &RealGit),
            Err(Error::NotInRepo)
        ));
    }

    #[test]
    fn discover_from_linked_worktree_finds_primary_root() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let linked = repo.root().parent().unwrap().join("wt-x");
        let ws = Workspace::discover(&linked, &env(), &RealGit).unwrap();
        assert_eq!(
            std::fs::canonicalize(ws.root()).unwrap(),
            std::fs::canonicalize(repo.root()).unwrap()
        );
    }

    #[test]
    fn create_new_branch_records_metadata_and_copies_nothing() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        let created = ws
            .create(&RealGit, &RealHookRunner, &create_opts("feature/login"))
            .unwrap();
        assert!(!created.reused);
        assert_eq!(created.branch, "feature/login");
        assert_eq!(created.base_ref.as_deref(), Some("main"));
        assert!(created.path.is_dir());
        assert!(
            created.path.ends_with("feature-login")
                || created.path.to_string_lossy().contains("feature-login")
        );
        assert_eq!(created.post_create, HookOutcome::Skipped);
        assert_eq!(created.submodules, SubmodulesOutcome::Skipped);
        assert!(created.copy.copied.is_empty());
        let meta = ws.read_meta("feature/login").unwrap();
        assert_eq!(meta.base_ref.as_deref(), Some("main"));
        assert!(meta.created_by_wt);
    }

    #[test]
    fn create_existing_branch_does_not_mark_created() {
        let repo = TestRepo::init();
        repo.git(&["branch", "existing"]);
        let ws = workspace(&repo);
        let created = ws
            .create(&RealGit, &RealHookRunner, &create_opts("existing"))
            .unwrap();
        assert!(created.base_ref.is_none());
        assert!(!ws.read_meta("existing").unwrap().created_by_wt);
    }

    #[test]
    fn create_is_idempotent_at_the_same_target() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        let first = ws
            .create(&RealGit, &RealHookRunner, &create_opts("feature/x"))
            .unwrap();
        let second = ws
            .create(&RealGit, &RealHookRunner, &create_opts("feature/x"))
            .unwrap();
        assert!(!first.reused);
        assert!(second.reused);
        assert_eq!(second.path, first.path);
        assert_eq!(second.post_create, HookOutcome::Skipped);
    }

    #[test]
    fn create_refuses_branch_checked_out_elsewhere() {
        let repo = TestRepo::init();
        repo.add_worktree("dup", "../manual-dup");
        let ws = workspace(&repo);
        let err = ws
            .create(&RealGit, &RealHookRunner, &create_opts("dup"))
            .unwrap_err();
        assert!(err.to_string().contains("already checked out"));
    }

    #[test]
    fn create_with_explicit_base_records_it() {
        let repo = TestRepo::init();
        repo.git(&["branch", "base-branch"]);
        let ws = workspace(&repo);
        let mut opts = create_opts("derived");
        opts.base = Some("base-branch".into());
        let created = ws.create(&RealGit, &RealHookRunner, &opts).unwrap();
        assert_eq!(created.base_ref.as_deref(), Some("base-branch"));
        assert_eq!(
            ws.read_meta("derived").unwrap().base_ref.as_deref(),
            Some("base-branch")
        );
    }

    #[test]
    fn create_with_unknown_base_errors() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        let mut opts = create_opts("orphan");
        opts.base = Some("no-such-ref".into());
        let err = ws.create(&RealGit, &RealHookRunner, &opts).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn create_reports_hook_outcomes_without_failing() {
        let repo = TestRepo::init();
        repo.write(".wt.toml", "[hooks]\npost_create = \"exit 3\"\n");
        repo.commit_all("config");
        let ws = workspace(&repo);
        let mut opts = create_opts("hooked");
        opts.no_hooks = false;
        let created = ws.create(&RealGit, &RealHookRunner, &opts).unwrap();
        assert_eq!(created.post_create, HookOutcome::ExitedNonZero(3));
        assert!(created.path.is_dir());
    }

    #[test]
    fn create_copies_ignored_files() {
        let repo = TestRepo::init();
        std::fs::write(repo.root().join(".wt.toml"), "copy = [\".env\"]\n").unwrap();
        repo.write(".env", "SECRET=1\n");
        let ws = workspace(&repo);
        let created = ws
            .create(&RealGit, &RealHookRunner, &create_opts("withenv"))
            .unwrap();
        assert_eq!(created.copy.copied.len(), 1);
        assert!(created.path.join(".env").exists());
    }

    #[test]
    fn create_rolls_back_when_a_post_add_step_fails() {
        use crate::git::cli::{GitCli, GitOutput};
        struct FailConfig(RealGit);
        impl GitCli for FailConfig {
            fn run_raw(&self, repo: &Path, args: &[&str]) -> Result<GitOutput> {
                if args.first() == Some(&"config") && args.iter().any(|a| a.starts_with("wt.")) {
                    return Ok(GitOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "simulated failure".into(),
                    });
                }
                self.0.run_raw(repo, args)
            }
        }
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        let err = ws
            .create(
                &FailConfig(RealGit),
                &RealHookRunner,
                &create_opts("rollme"),
            )
            .unwrap_err();
        assert!(err.to_string().contains("simulated failure"));
        assert!(repo.git(&["branch", "--list", "rollme"]).trim().is_empty());
        assert!(!repo.git(&["worktree", "list"]).contains("rollme"));
    }

    /// Finds the row for `branch` in a fresh enriched listing.
    fn row_for(ws: &Workspace, branch: &str) -> Worktree {
        ws.list(&RealGit)
            .unwrap()
            .into_iter()
            .find(|w| w.branch.as_deref() == Some(branch))
            .unwrap()
    }

    #[test]
    fn list_enumerate_and_meta_expose_worktrees() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        ws.create(&RealGit, &RealHookRunner, &create_opts("feature/x"))
            .unwrap();
        let shallow = ws.enumerate(&RealGit).unwrap();
        assert_eq!(shallow.len(), 2);
        // The shallow pass has no status; the enriched pass does.
        assert!(shallow.iter().all(|w| w.dirty.is_none()));
        let feat = row_for(&ws, "feature/x");
        assert_eq!(feat.dirty, Some(false));
        assert_eq!(feat.base_ref.as_deref(), Some("main"));
    }

    #[test]
    fn remove_blocked_by_guards_is_a_typed_error() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        ws.create(&RealGit, &RealHookRunner, &create_opts("topic"))
            .unwrap();
        // No upstream -> unpushed; clean -> not dirty.
        let row = row_for(&ws, "topic");
        let err = ws
            .remove(
                &RealGit,
                &RealHookRunner,
                &row,
                &RemoveOptions {
                    no_hooks: true,
                    ..Default::default()
                },
            )
            .unwrap_err();
        match err {
            Error::RemoveGuarded { dirty, unpushed } => {
                assert!(!dirty);
                assert!(unpushed);
            }
            other => panic!("expected RemoveGuarded, got {other:?}"),
        }
    }

    #[test]
    fn remove_force_reports_forced_past_guards() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        ws.create(&RealGit, &RealHookRunner, &create_opts("forced"))
            .unwrap();
        let row = row_for(&ws, "forced");
        let removed = ws
            .remove(
                &RealGit,
                &RealHookRunner,
                &row,
                &RemoveOptions {
                    force_remove: true,
                    force_branch: true,
                    no_hooks: true,
                    keep_branch: false,
                },
            )
            .unwrap();
        assert!(removed.forced_past_guards);
        assert!(!repo.git(&["worktree", "list"]).contains("forced"));
        // Merged wt-created branch is deleted per config.
        assert!(removed.branch_deleted);
        // Metadata is cleared.
        assert_eq!(ws.read_meta("forced").unwrap(), WtMeta::default());
    }

    #[test]
    fn remove_refuses_the_primary_worktree() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        let main = row_for(&ws, "main");
        let err = ws
            .remove(&RealGit, &RealHookRunner, &main, &RemoveOptions::default())
            .unwrap_err();
        assert!(err.to_string().contains("primary"));
    }

    #[test]
    fn remove_missing_worktree_prunes_without_guards() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        let created = ws
            .create(&RealGit, &RealHookRunner, &create_opts("gone"))
            .unwrap();
        std::fs::remove_dir_all(&created.path).unwrap();
        let row = row_for(&ws, "gone");
        assert!(row.is_missing);
        let removed = ws
            .remove(
                &RealGit,
                &RealHookRunner,
                &row,
                &RemoveOptions {
                    no_hooks: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(removed.pre_remove, HookOutcome::Skipped);
        assert!(!repo.git(&["worktree", "list"]).contains("gone"));
    }

    #[test]
    fn remove_failing_pre_remove_hook_aborts_unless_forced() {
        let repo = TestRepo::init();
        repo.write(".wt.toml", "[hooks]\npre_remove = \"exit 5\"\n");
        repo.commit_all("config");
        let ws = workspace(&repo);
        ws.create(&RealGit, &RealHookRunner, &create_opts("hooked"))
            .unwrap();
        // Give the branch an upstream so guards do not block first.
        let head = repo.git(&["rev-parse", "HEAD"]).trim().to_string();
        repo.git(&["update-ref", "refs/remotes/origin/hooked", &head]);
        repo.git(&["config", "branch.hooked.remote", "origin"]);
        repo.git(&["config", "branch.hooked.merge", "refs/heads/hooked"]);
        let row = row_for(&ws, "hooked");
        let err = ws
            .remove(&RealGit, &RealHookRunner, &row, &RemoveOptions::default())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("pre_remove hook exited with status 5")
        );
        // Forced: the failure is downgraded to an outcome and removal proceeds.
        let removed = ws
            .remove(
                &RealGit,
                &RealHookRunner,
                &row,
                &RemoveOptions {
                    force_remove: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(removed.pre_remove, HookOutcome::ExitedNonZero(5));
        assert!(!repo.git(&["worktree", "list"]).contains("hooked"));
    }

    #[test]
    fn discover_refuses_a_future_schema() {
        let repo = TestRepo::init();
        repo.git(&["config", "wt.schema", "99"]);
        let err = Workspace::discover(repo.root(), &env(), &RealGit)
            .err()
            .expect("a future schema must refuse discovery");
        assert!(matches!(err, Error::SchemaTooNew { found: 99, .. }));
    }

    #[test]
    fn mutations_refuse_a_schema_stamped_after_discovery() {
        // A long-lived Workspace must not mutate a repo that was upgraded
        // underneath it: create/remove re-check through a fresh handle.
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        repo.git(&["config", "wt.schema", "2"]);
        let err = ws
            .create(&RealGit, &RealHookRunner, &create_opts("late"))
            .unwrap_err();
        assert!(matches!(err, Error::SchemaTooNew { found: 2, .. }));
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        let held = ws.lock().unwrap();
        // A second acquisition times out while the first is held...
        let err = acquire_repo_lock(ws.root(), Duration::from_millis(50))
            .err()
            .expect("the held lock must exclude a second holder");
        match &err {
            Error::LockUnavailable { path, .. } => {
                assert!(path.ends_with("wt-mutation.lock"), "{path}");
            }
            other => panic!("expected LockUnavailable, got {other:?}"),
        }
        // ...and succeeds once the holder is dropped.
        drop(held);
        acquire_repo_lock(ws.root(), Duration::from_millis(50)).unwrap();
    }

    #[test]
    fn create_releases_the_lock_before_the_post_create_hook() {
        // A hook that re-enters wt must not deadlock (issue #99): the hook
        // itself proves the lock file is gone by the time it runs.
        let repo = TestRepo::init();
        repo.write(
            ".wt.toml",
            "[hooks]\npost_create = \"test ! -e \\\"$WT_REPO_ROOT/.git/wt-mutation.lock\\\"\"\n",
        );
        repo.commit_all("config");
        let ws = workspace(&repo);
        let mut opts = create_opts("hookfree");
        opts.no_hooks = false;
        let created = ws.create(&RealGit, &RealHookRunner, &opts).unwrap();
        assert_eq!(created.post_create, HookOutcome::Succeeded);
    }

    #[test]
    fn concurrent_creates_on_one_branch_do_not_corrupt_metadata() {
        // Two writers race to create the same branch (issue #99): exactly one
        // wins, the loser gets a clean error, and the metadata ends up
        // consistent — one worktree, one baseRef, createdByWt set once.
        let repo = TestRepo::init();
        let root = repo.root().to_path_buf();
        let spawn = |root: PathBuf| {
            std::thread::spawn(move || {
                let ws = Workspace::discover(&root, &env(), &RealGit)?;
                ws.create(&RealGit, &RealHookRunner, &create_opts("feat/race"))
            })
        };
        let a = spawn(root.clone());
        let b = spawn(root);
        let results = [a.join().unwrap(), b.join().unwrap()];
        let ok = results.iter().filter(|r| r.is_ok()).count();
        // Both may succeed only if one reused the other's finished worktree;
        // never may both claim to have created it.
        let created = results
            .iter()
            .filter(|r| r.as_ref().is_ok_and(|c| !c.reused))
            .count();
        assert!(ok >= 1, "at least one racer must win: {results:?}");
        assert_eq!(created, 1, "exactly one racer creates: {results:?}");

        let ws = workspace(&repo);
        let rows = ws.list(&RealGit).unwrap();
        let race_rows: Vec<_> = rows
            .iter()
            .filter(|w| w.branch.as_deref() == Some("feat/race"))
            .collect();
        assert_eq!(race_rows.len(), 1);
        let meta = ws.read_meta("feat/race").unwrap();
        assert_eq!(meta.base_ref.as_deref(), Some("main"));
        assert!(meta.created_by_wt);
    }

    #[test]
    fn resolve_base_falls_back_to_head_only_without_default() {
        let repo = TestRepo::init();
        let ws = workspace(&repo);
        let r = Repo::discover(repo.root()).unwrap();
        assert_eq!(
            resolve_base(&r, ws.config(), Some("explicit")),
            ("explicit".into(), false)
        );
        // The repo default branch resolves without a HEAD fallback.
        assert_eq!(resolve_base(&r, ws.config(), None), ("main".into(), false));
        // A configured default_base wins over the repo default branch.
        let mut config = ws.config().clone();
        config.default_base = Some("trunk".into());
        assert_eq!(resolve_base(&r, &config, None), ("trunk".into(), false));
    }
}
