//! Git submodule detection and initialization (issue #50).
//!
//! `git submodule status` is a sanctioned subprocess read (spec §4); the parsers
//! live in [`porcelain`](super::porcelain). Initialization is a mutating/network
//! operation, so it goes through the `git` CLI. Both run against a worktree
//! directory: a freshly created worktree (or one switched to a branch that adds
//! submodules) reports its submodules as uninitialized until `update --init`
//! populates them.
//!
//! # Why a new worktree re-clones everything
//!
//! A linked worktree does **not** share the superproject's submodule object
//! stores. Git resolves a linked worktree's submodule gitdir to
//! `$GIT_COMMON_DIR/worktrees/<id>/modules/<name>`, not the shared
//! `$GIT_COMMON_DIR/modules/<name>`, so a plain `update --init --recursive` in a
//! new worktree performs a full network clone of every submodule even though
//! byte-identical objects already sit on the same disk.
//!
//! Two consequences show up here: enumeration has to recurse to be honest about
//! nested submodules, and `git worktree remove` refuses any worktree that has a
//! populated submodule.

use std::path::Path;

use crate::error::{Error, Result};
use crate::git::cli::GitCli;
use crate::git::porcelain::{Submodule, parse_gitmodules, parse_submodule_status};

pub(crate) mod reattach;
pub(crate) mod seed;

/// Returns the paths of submodules that are defined but not yet initialized in
/// `worktree_dir` (the `-` marker of `git submodule status`). Best-effort: a repo
/// with no submodules, or a directory where the command cannot run, yields an
/// empty list rather than an error, so callers can treat "no submodules" and
/// "could not tell" alike.
///
/// Recursive, so nested submodules count too. Note that `--recursive` can only
/// descend into submodules that are already initialized: an uninitialized parent
/// hides its children until it is populated. The count is therefore honest about
/// what is knowable now, not a prediction of the eventual total.
pub fn uninitialized(git: &dyn GitCli, worktree_dir: &Path) -> Result<Vec<String>> {
    let output = git.run_raw(worktree_dir, &["submodule", "status", "--recursive"])?;
    if !output.success {
        return Ok(Vec::new());
    }
    Ok(parse_submodule_status(&output.stdout)
        .into_iter()
        .filter(|s| s.is_uninitialized())
        .map(|s| s.path)
        .collect())
}

/// Whether `worktree_dir` currently has at least one *populated* submodule.
///
/// This is the condition `git worktree remove` refuses on
/// (`fatal: working trees containing submodules cannot be moved or removed`),
/// so removal consults it to decide whether git needs forcing. Best-effort, like
/// [`uninitialized`]: "could not tell" reports `false`.
pub(crate) fn any_initialized(git: &dyn GitCli, worktree_dir: &Path) -> Result<bool> {
    let output = git.run_raw(worktree_dir, &["submodule", "status", "--recursive"])?;
    if !output.success {
        return Ok(false);
    }
    Ok(parse_submodule_status(&output.stdout)
        .into_iter()
        .any(|s| !s.is_uninitialized()))
}

/// Whether `value` is a plain relative path — non-empty, and made only of
/// normal components, so it can never climb out of a directory it is joined
/// onto.
///
/// Submodule names and paths come from `.gitmodules`, which is repository
/// content and therefore untrusted, and callers here join them onto real
/// directories. Git has rejected `..` in submodule names and paths since 2.20,
/// but nothing in that check is reachable from
/// [`git config -f .gitmodules`](declared) — it reads the file as plain config —
/// so this code must not lean on it.
pub(crate) fn is_plain_relative(value: &str) -> bool {
    use std::path::Component;

    !value.is_empty()
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
}

/// Returns the submodules declared in `worktree_dir`'s `.gitmodules`, in
/// declaration order.
///
/// Reads the file through `git config -f`, so git's own config parser handles
/// quoting, comments and line continuations rather than a hand-rolled reader.
/// Best-effort: no `.gitmodules` (or an unreadable one) yields an empty list,
/// which is the same shape as "this directory has no submodules".
///
/// Entries whose name or path is not a [plain relative path](is_plain_relative)
/// are dropped here, at the boundary where the untrusted file is read, so no
/// caller downstream can join one onto a directory. Git refuses such a
/// submodule anyway, so dropping it costs nothing: the stock
/// `update --init --recursive` pass reports it exactly as it would have.
pub(crate) fn declared(git: &dyn GitCli, worktree_dir: &Path) -> Result<Vec<Submodule>> {
    if !worktree_dir.join(".gitmodules").is_file() {
        return Ok(Vec::new());
    }
    let output = git.run_raw(
        worktree_dir,
        &[
            "config",
            "-f",
            ".gitmodules",
            "-z",
            "--get-regexp",
            "^submodule\\..*",
        ],
    )?;
    if !output.success {
        return Ok(Vec::new());
    }
    Ok(parse_gitmodules(&output.stdout)
        .into_iter()
        .filter(|s| {
            let ok = is_plain_relative(&s.name) && is_plain_relative(&s.path);
            if !ok {
                tracing::debug!(
                    name = %s.name,
                    path = %s.path,
                    "ignoring a .gitmodules entry that is not a plain relative path"
                );
            }
            ok
        })
        .collect())
}

/// Initializes and updates all submodules in `worktree_dir`, recursively
/// (`git submodule update --init --recursive`). Propagates a subprocess error;
/// callers decide whether that is fatal.
///
/// Parallelism is deliberately not a `wt` option: git already honours its own
/// `submodule.fetchJobs` here, and a second knob for the same thing would only
/// apply on the paths `wt` happens to own.
pub fn update_init(git: &dyn GitCli, worktree_dir: &Path) -> Result<()> {
    git.run(
        worktree_dir,
        &["submodule", "update", "--init", "--recursive"],
    )?;
    Ok(())
}

/// Populates every submodule in `worktree_dir`, optionally seeding from the
/// repository's own local mirrors first.
///
/// The ordering is the correctness contract:
///
/// 1. Seed what has a local mirror (near-instant, hardlinked, no network).
/// 2. [`sync`] the URLs back to `.gitmodules`, undoing the mirror `origin` the
///    seed clones left behind.
/// 3. Run the stock [`update_init`] pass, which decides the result: anything
///    seeding skipped is cloned normally, anything it got wrong is corrected.
///
/// So seeding is only ever an accelerator. If it fails outright the end state is
/// byte-identical to not seeding at all, just slower. The returned
/// [`SeedReport`](seed::SeedReport) is informational; the `Result` reflects only
/// the stock pass.
pub(crate) fn populate(
    git: &dyn GitCli,
    worktree_dir: &Path,
    seed_from_mirrors: bool,
) -> (seed::SeedReport, Result<()>) {
    let span = tracing::info_span!("submodules", worktree = %worktree_dir.display());
    let _guard = span.enter();

    let mut report = seed::SeedReport::default();
    if seed_from_mirrors {
        match linked_worktree_common_dir(git, worktree_dir) {
            Ok(Some(common)) => report = seed::seed_from_mirrors(git, worktree_dir, &common),
            // The primary worktree already resolves its submodules to the shared
            // `.git/modules`, so there is nothing to seed from and nothing to
            // save — seeding is purely a linked-worktree concern.
            Ok(None) => tracing::debug!("primary worktree; submodules already share the mirrors"),
            Err(e) => {
                tracing::debug!(error = %e, "could not resolve the git dirs; not seeding");
            }
        }
        // Restore the real upstreams before anything can fetch. A failure here
        // must stop the reconcile pass, which would otherwise fetch through a
        // mirror path and could persist it as the submodule's origin.
        if !report.is_empty()
            && let Err(e) = sync(git, worktree_dir)
        {
            return (report, Err(e));
        }
    }
    let result = update_init(git, worktree_dir);
    (report, result)
}

/// The repository's common git directory — the shared `.git` holding
/// `modules/` — but only when `worktree_dir` is a *linked* worktree.
///
/// `None` means `worktree_dir` is the primary worktree, where the private and
/// common git dirs are the same path and submodules already live in the shared
/// mirrors.
fn linked_worktree_common_dir(
    git: &dyn GitCli,
    worktree_dir: &Path,
) -> Result<Option<std::path::PathBuf>> {
    let out = git.run(
        worktree_dir,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-dir",
            "--git-common-dir",
        ],
    )?;
    let mut lines = out.lines();
    let (Some(git_dir), Some(common)) = (lines.next(), lines.next()) else {
        return Err(Error::operation(
            "could not resolve the repository git dirs",
        ));
    };
    if git_dir.trim() == common.trim() {
        return Ok(None);
    }
    Ok(Some(std::path::PathBuf::from(common.trim())))
}

/// Re-reads every submodule URL from `.gitmodules` into the worktree's config
/// and into each populated submodule's `remote.origin.url`
/// (`git submodule sync --recursive`).
///
/// Seeding clones a submodule from a local mirror, which leaves that mirror path
/// as the submodule's `origin`. This restores the real upstream, so it must run
/// before anything that could contact a remote. Propagates a subprocess error:
/// leaving mirror paths as origins would be a silently wrong repository.
pub(crate) fn sync(git: &dyn GitCli, worktree_dir: &Path) -> Result<()> {
    git.run(worktree_dir, &["submodule", "sync", "--recursive"])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    #[test]
    fn no_submodules_yields_empty() {
        let repo = TestRepo::init();
        assert!(uninitialized(&RealGit, repo.root()).unwrap().is_empty());
    }

    #[test]
    fn reports_uninitialized_submodule() {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        // After `add` the submodule is initialized; deinit makes it report `-`.
        repo.deinit_submodule("libs/sub");
        let pending = uninitialized(&RealGit, repo.root()).unwrap();
        assert_eq!(pending, vec!["libs/sub".to_string()]);
    }

    #[test]
    fn update_init_populates_submodule() {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        repo.deinit_submodule("libs/sub");
        // Sanity: empty before, populated after (reuses .git/modules, no clone).
        assert!(!repo.root().join("libs/sub/sub.txt").exists());
        update_init(&RealGit, repo.root()).unwrap();
        assert!(repo.root().join("libs/sub/sub.txt").exists());
        assert!(uninitialized(&RealGit, repo.root()).unwrap().is_empty());
    }

    #[test]
    fn is_plain_relative_accepts_only_normal_components() {
        assert!(is_plain_relative("libs/sub"));
        assert!(is_plain_relative("a.b.c"));
        assert!(!is_plain_relative(""));
        assert!(!is_plain_relative(".."));
        assert!(!is_plain_relative("../outside"));
        assert!(!is_plain_relative("libs/../../outside"));
        assert!(!is_plain_relative("/etc"));
        assert!(!is_plain_relative("./libs"));
    }

    #[test]
    fn declared_drops_entries_that_could_escape_the_repository() {
        // `.gitmodules` is untrusted repository content, and `git config -f`
        // reads it as plain config — none of git's own submodule name/path
        // validation is in play. Callers join both fields onto real directories
        // (`.git/modules/<name>`, `<worktree>/<path>`), so a traversing entry
        // must not reach them.
        let repo = TestRepo::init();
        repo.write(
            ".gitmodules",
            concat!(
                "[submodule \"good\"]\n\tpath = libs/good\n\turl = ../g\n",
                "[submodule \"../../../../tmp/evil\"]\n\tpath = libs/good2\n\turl = ../e\n",
                "[submodule \"escaper\"]\n\tpath = ..\n\turl = ../e\n",
                "[submodule \"absolute\"]\n\tpath = /etc/passwd\n\turl = ../e\n",
            ),
        );
        repo.commit_all("declare hostile submodules");

        let subs = declared(&RealGit, repo.root()).unwrap();
        assert_eq!(
            subs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["good"],
            "a traversing name or path survived the .gitmodules read"
        );
    }

    #[test]
    fn uninitialized_is_empty_outside_a_repo() {
        // `git submodule status` fails (non-success) in a non-repo dir; the
        // best-effort contract returns an empty list rather than erroring.
        let dir = tempfile::tempdir().unwrap();
        assert!(uninitialized(&RealGit, dir.path()).unwrap().is_empty());
    }

    #[test]
    fn uninitialized_recurses_into_nested_submodules() {
        let repo = TestRepo::init();
        repo.add_nested_submodule("libs/sub", "deep");
        // Everything is populated after the fixture builds it.
        assert!(uninitialized(&RealGit, repo.root()).unwrap().is_empty());
        // Deinit only the *nested* one. A non-recursive `submodule status`
        // reports nothing here, which is the undercount this recursion fixes.
        repo.git(&["-C", "libs/sub", "submodule", "deinit", "-q", "-f", "deep"]);
        let pending = uninitialized(&RealGit, repo.root()).unwrap();
        assert_eq!(pending, vec!["libs/sub/deep".to_string()]);
    }

    #[test]
    fn any_initialized_tracks_population() {
        let repo = TestRepo::init();
        assert!(!any_initialized(&RealGit, repo.root()).unwrap());
        repo.add_submodule("libs/sub");
        assert!(any_initialized(&RealGit, repo.root()).unwrap());
        repo.deinit_submodule("libs/sub");
        assert!(!any_initialized(&RealGit, repo.root()).unwrap());
    }
}
