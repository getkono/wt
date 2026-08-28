//! Seeding a new worktree's submodules from the repository's own local mirrors.
//!
//! # The problem
//!
//! A linked worktree does not share the superproject's submodule object stores.
//! Git resolves a linked worktree's submodule gitdir to
//! `$GIT_COMMON_DIR/worktrees/<id>/modules/<name>`, so
//! `git submodule update --init --recursive` in a fresh worktree clones every
//! submodule over the network — even though `$GIT_COMMON_DIR/modules/<name>`
//! already holds byte-identical objects on the same disk. On a superproject with
//! many submodules that dominates the cost of creating a worktree.
//!
//! # The approach
//!
//! For each submodule that already has a local mirror, clone from the mirror
//! instead of the network by overriding `submodule.<name>.url` **for a single
//! `git` invocation**. Git's local-clone path hardlinks the object store, so the
//! clone is near-instant and costs almost no disk.
//!
//! Two properties make this safe rather than clever:
//!
//! 1. **Nothing is persisted.** The mirror path is passed with `git -c`, never
//!    written to config. `submodule.<name>.url` lives in the *shared* repo
//!    config, so persisting it would rewrite what every other worktree sees, and
//!    a crash mid-seed would leave the repository pointing at mirror paths.
//!    The URL that does get persisted is the one stock `git submodule init`
//!    writes, straight from `.gitmodules`.
//! 2. **Seeding is only ever an accelerator.** Callers must follow it with
//!    [`sync`](super::sync) and a stock
//!    [`update_init`](super::update_init) reconcile pass. Anything seeding
//!    skipped or got wrong is then fixed by git itself, so the end state matches
//!    what stock git would have produced. A failure here is reported, never
//!    fatal.
//!
//! # A rejected alternative
//!
//! Making each submodule a *linked worktree of its own mirror*
//! (`git -C .git/modules/<name> worktree add --detach <wt>/<path> <sha>`) needs
//! no object copying at all, and every superproject command accepts the result.
//! It is nonetheless wrong: the first time anyone runs `git submodule update` in
//! such a worktree, git writes `core.worktree` into the mirror's **shared**
//! config, computed relative to the per-worktree git directory. That path is
//! wrong for every other consumer, and it permanently breaks the *primary*
//! worktree's submodule — every later command against the mirror dies with
//! `fatal: cannot chdir to '../../../../../../../<wt>/<path>'`. Do not revisit
//! this approach.
//!
//! # Security
//!
//! `protocol.file.allow=always` lifts the CVE-2022-39253 protection against
//! file-protocol submodule clones, so it is passed **only** on an invocation
//! that targets one named submodule path whose URL this module has just
//! overridden with a mirror directory inside the repository's own
//! `$GIT_COMMON_DIR`. A submodule without a local mirror is left entirely alone
//! — its URL comes from `.gitmodules`, which is untrusted content, and it is
//! never fetched under a relaxed protocol allowlist. [`Submodule`] deliberately
//! does not even carry the `.gitmodules` URL, so there is no way to reach for it
//! here by accident.

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::error::Result;
use crate::git::cli::GitCli;

/// What seeding managed to do, for reporting. Paths are relative to the
/// superproject worktree, so nested submodules read as `outer/inner`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SeedReport {
    /// Submodules populated from a local mirror.
    pub(crate) seeded: Vec<String>,
    /// Submodules with no local mirror, left for the reconcile pass to clone.
    pub(crate) skipped: Vec<String>,
    /// Submodules whose seeding failed, with the rendered error. The reconcile
    /// pass still gets a chance to populate these.
    pub(crate) failed: Vec<(String, String)>,
}

impl SeedReport {
    /// Whether seeding did no useful work, so callers can skip the extra
    /// `submodule sync` that only matters after a mirror clone.
    pub(crate) fn is_empty(&self) -> bool {
        self.seeded.is_empty() && self.failed.is_empty()
    }
}

/// Populates `worktree_dir`'s submodules from the mirrors under
/// `common_dir/modules`, recursively.
///
/// Never fails: every problem is recorded in the returned [`SeedReport`] and
/// left for the caller's reconcile pass. `common_dir` is the repository's
/// `$GIT_COMMON_DIR` (the shared `.git`, not the worktree's private one).
pub(crate) fn seed_from_mirrors(
    git: &dyn GitCli,
    worktree_dir: &Path,
    common_dir: &Path,
) -> SeedReport {
    let mut report = SeedReport::default();
    let root = common_dir.join("modules");
    walk(git, worktree_dir, &root, &root, "", &mut report);
    report
}

/// Seeds one level of submodules, then recurses into each one it populated.
///
/// The recursion has to be level-by-level: a nested submodule's `.gitmodules`
/// lives inside its parent's working tree, so the children are not knowable
/// until the parent is on disk. Mirrors nest the same way, at
/// `<parent mirror>/modules/<child name>`.
fn walk(
    git: &dyn GitCli,
    dir: &Path,
    mirror_prefix: &Path,
    mirror_root: &Path,
    rel_prefix: &str,
    report: &mut SeedReport,
) {
    let declared = match super::declared(git, dir) {
        Ok(subs) => subs,
        Err(e) => {
            debug!(dir = %dir.display(), error = %e, "could not read .gitmodules");
            return;
        }
    };
    for sub in declared {
        let rel = if rel_prefix.is_empty() {
            sub.path.clone()
        } else {
            format!("{rel_prefix}/{}", sub.path)
        };
        let Some(mirror) = mirror_within(mirror_prefix, &sub.name, mirror_root) else {
            debug!(submodule = %rel, "no usable local mirror; leaving it to the reconcile pass");
            report.skipped.push(rel);
            continue;
        };
        match seed_one(git, dir, &sub.name, &sub.path, &mirror) {
            Ok(()) => {
                debug!(submodule = %rel, "seeded from local mirror");
                report.seeded.push(rel.clone());
                walk(
                    git,
                    &dir.join(&sub.path),
                    &mirror.join("modules"),
                    mirror_root,
                    &rel,
                    report,
                );
            }
            Err(e) => {
                debug!(submodule = %rel, error = %e, "seeding failed");
                report.failed.push((rel, e.to_string()));
            }
        }
    }
}

/// Resolves the mirror directory for a submodule `name`, or `None` when there
/// is no usable one.
///
/// Submodule names come from `.gitmodules`, which is repository content and
/// therefore untrusted, and this path is about to be handed to a clone. A name
/// like `../../../elsewhere` would otherwise walk out of the repository and make
/// `wt` clone from an attacker-chosen local directory.
///
/// Two independent checks: reject any name that is not a
/// [plain relative path](super::is_plain_relative) of normal components, and
/// verify the canonicalized result still sits inside the canonicalized mirror
/// root (which also catches escapes through a symlink). The first is redundant
/// with the filtering [`declared`](super::declared) does, deliberately — every
/// path built under `.git/modules` is checked where it is built, not only where
/// its name was read.
pub(crate) fn mirror_within(prefix: &Path, name: &str, root: &Path) -> Option<PathBuf> {
    if !super::is_plain_relative(name) {
        return None;
    }
    let candidate = prefix.join(name);
    if !candidate.is_dir() {
        return None;
    }
    let real = candidate.canonicalize().ok()?;
    let real_root = root.canonicalize().ok()?;
    real.starts_with(&real_root).then_some(real)
}

/// Populates a single submodule from `mirror`.
///
/// `submodule init` runs first and unmodified, so the URL persisted into the
/// shared config is the real one from `.gitmodules`. Only the follow-up
/// `update` sees the mirror, via a process-scoped `-c` override that is never
/// written to disk.
fn seed_one(git: &dyn GitCli, dir: &Path, name: &str, path: &str, mirror: &Path) -> Result<()> {
    git.run(dir, &["submodule", "init", "--", path])?;
    let url_override = format!("submodule.{name}.url={}", mirror.display());
    git.run(
        dir,
        &[
            // Safe here and only here: the URL being allowed is the mirror path
            // built above, not anything out of `.gitmodules`, and `-- <path>`
            // keeps the relaxation scoped to this one submodule.
            "-c",
            "protocol.file.allow=always",
            "-c",
            &url_override,
            "submodule",
            "update",
            "--",
            path,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::{GitOutput, RealGit};
    use crate::testutil::TestRepo;
    use std::sync::Mutex;

    /// Records every argv the code sends to git, so tests can assert on the
    /// security-relevant shape of the commands rather than only their effects.
    struct Recording {
        inner: RealGit,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl Recording {
        fn new() -> Self {
            Self {
                inner: RealGit,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GitCli for Recording {
        fn run_raw(&self, repo: &Path, args: &[&str]) -> Result<GitOutput> {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|a| a.to_string()).collect());
            self.inner.run_raw(repo, args)
        }
    }

    /// A worktree of `repo` on a new branch, plus the repo's common git dir.
    fn worktree_of(repo: &TestRepo, branch: &str) -> (PathBuf, PathBuf) {
        repo.add_worktree(branch, "../wt-seed");
        let path = repo.root().parent().unwrap().join("wt-seed");
        let common = repo.root().join(".git");
        (path, common)
    }

    #[test]
    fn seeds_a_submodule_from_the_local_mirror() {
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        let (wt, common) = worktree_of(&repo, "topic");
        // Nothing is populated in a fresh worktree.
        assert!(!wt.join("libs/sub/sub.txt").exists());

        let report = seed_from_mirrors(&RealGit, &wt, &common);
        assert_eq!(report.seeded, vec!["libs/sub".to_string()]);
        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert!(wt.join("libs/sub/sub.txt").exists());
    }

    #[test]
    fn seeded_objects_are_shared_with_the_mirror() {
        // The whole point: the clone must hardlink the mirror's objects rather
        // than transfer a second copy.
        let repo = TestRepo::init();
        repo.add_submodule("libs/sub");
        let (wt, common) = worktree_of(&repo, "topic");
        seed_from_mirrors(&RealGit, &wt, &common);

        let mirror_objects = common.join("modules/libs/sub/objects");
        let linked = count_multiply_linked(&mirror_objects);
        assert!(
            linked > 0,
            "no object in {} is hardlinked; the clone copied instead of sharing",
            mirror_objects.display()
        );
    }

    /// Counts loose object files with more than one hardlink.
    fn count_multiply_linked(dir: &Path) -> usize {
        use std::os::unix::fs::MetadataExt;
        let mut n = 0;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|f| f == "info" || f == "pack") {
                    continue;
                }
                n += count_multiply_linked(&path);
            } else if let Ok(md) = path.metadata()
                && md.nlink() > 1
            {
                n += 1;
            }
        }
        n
    }

    #[test]
    fn never_persists_the_mirror_url_into_the_shared_config() {
        // `submodule.<name>.url` is shared across every worktree, so writing a
        // mirror path there would rewrite what the primary worktree sees.
        let repo = TestRepo::init();
        let src = repo.add_submodule("libs/sub");
        let before = repo.git(&["config", "submodule.libs/sub.url"]);
        assert_eq!(before.trim(), src.to_string_lossy());

        let (wt, common) = worktree_of(&repo, "topic");
        seed_from_mirrors(&RealGit, &wt, &common);

        let after = repo.git(&["config", "submodule.libs/sub.url"]);
        assert_eq!(
            after.trim(),
            src.to_string_lossy(),
            "seeding rewrote the shared submodule URL"
        );
    }

    #[test]
    fn file_protocol_is_only_relaxed_for_a_mirror_backed_update() {
        let repo = TestRepo::init();
        repo.add_submodule("mirrored");
        // A second submodule declared in .gitmodules but with no mirror on disk.
        repo.write(
            ".gitmodules",
            &format!(
                "{}\n[submodule \"orphan\"]\n\tpath = orphan\n\turl = ../nowhere\n",
                std::fs::read_to_string(repo.root().join(".gitmodules"))
                    .unwrap()
                    .trim_end()
            ),
        );
        repo.commit_all("declare an unmirrored submodule");

        let (wt, common) = worktree_of(&repo, "topic");
        let git = Recording::new();
        let report = seed_from_mirrors(&git, &wt, &common);
        assert_eq!(report.seeded, vec!["mirrored".to_string()]);
        assert_eq!(report.skipped, vec!["orphan".to_string()]);

        for call in git.calls() {
            if !call.iter().any(|a| a == "protocol.file.allow=always") {
                continue;
            }
            // Every relaxed invocation must name exactly one submodule, and it
            // must be the mirrored one.
            let after_sep = call.iter().skip_while(|a| *a != "--").skip(1).count();
            assert_eq!(after_sep, 1, "relaxed call is not path-scoped: {call:?}");
            assert!(
                call.iter().any(|a| a == "mirrored"),
                "file protocol relaxed for a non-mirrored submodule: {call:?}"
            );
            assert!(
                !call.iter().any(|a| a.contains("orphan")),
                "file protocol relaxed for an unmirrored submodule: {call:?}"
            );
        }
    }

    #[test]
    fn a_mirror_name_cannot_escape_the_modules_directory() {
        // `.gitmodules` is untrusted repository content, and the mirror path it
        // names is about to be cloned from with the file protocol allowed.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("modules");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        assert!(mirror_within(&root, "real", &root).is_some());
        assert_eq!(mirror_within(&root, "../outside", &root), None);
        assert_eq!(mirror_within(&root, "../../outside", &root), None);
        assert_eq!(mirror_within(&root, "", &root), None);
        assert_eq!(
            mirror_within(&root, outside.to_str().unwrap(), &root),
            None,
            "an absolute name must not be accepted"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_mirror_symlinked_out_of_the_modules_directory_is_refused() {
        // The component check alone would pass this; the canonicalized
        // containment check is what catches it.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("modules");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("sneaky")).unwrap();

        assert_eq!(mirror_within(&root, "sneaky", &root), None);
    }

    #[test]
    fn a_submodule_without_a_mirror_is_skipped_not_failed() {
        let repo = TestRepo::init();
        repo.write(
            ".gitmodules",
            "[submodule \"orphan\"]\n\tpath = orphan\n\turl = ../nowhere\n",
        );
        repo.commit_all("declare an unmirrored submodule");
        let (wt, common) = worktree_of(&repo, "topic");

        let report = seed_from_mirrors(&RealGit, &wt, &common);
        assert_eq!(report.skipped, vec!["orphan".to_string()]);
        assert!(report.seeded.is_empty());
        assert!(report.failed.is_empty());
    }

    #[test]
    fn no_submodules_is_an_empty_report() {
        let repo = TestRepo::init();
        let (wt, common) = worktree_of(&repo, "topic");
        let report = seed_from_mirrors(&RealGit, &wt, &common);
        assert!(report.is_empty());
        assert!(report.skipped.is_empty());
    }

    #[test]
    fn seeding_produces_the_same_state_as_a_stock_update() {
        // The correctness claim of the whole feature: seeding is an accelerator,
        // not a different algorithm. Populate one worktree the seeded way and
        // another the stock way, then compare what git reports.
        let repo = TestRepo::init();
        repo.add_nested_submodule("libs/sub", "deep");

        repo.add_worktree("seeded", "../wt-seeded");
        let seeded = repo.root().parent().unwrap().join("wt-seeded");
        let common = repo.root().join(".git");
        seed_from_mirrors(&RealGit, &seeded, &common);
        super::super::sync(&RealGit, &seeded).unwrap();
        super::super::update_init(&RealGit, &seeded).unwrap();

        repo.add_worktree("stock", "../wt-stock");
        let stock = repo.root().parent().unwrap().join("wt-stock");
        // The fixture's submodules are file:// URLs, which git refuses to clone
        // for submodules by default; the stock control needs the same opt-in the
        // fixture uses. Production URLs need no such thing.
        repo.git(&[
            "-C",
            &stock.to_string_lossy(),
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--recursive",
        ]);

        let status = |dir: &Path| {
            repo.git(&[
                "-C",
                &dir.to_string_lossy(),
                "submodule",
                "status",
                "--recursive",
            ])
        };
        assert_eq!(status(&seeded), status(&stock));

        // And the recorded origins must be the real upstreams in both, not the
        // mirror paths seeding cloned through.
        let origin = |dir: &Path, sub: &str| {
            repo.git(&[
                "-C",
                &dir.join(sub).to_string_lossy(),
                "config",
                "remote.origin.url",
            ])
        };
        assert_eq!(origin(&seeded, "libs/sub"), origin(&stock, "libs/sub"));
        assert_eq!(
            origin(&seeded, "libs/sub/deep"),
            origin(&stock, "libs/sub/deep")
        );
    }

    #[test]
    fn seeds_nested_submodules_recursively() {
        let repo = TestRepo::init();
        repo.add_nested_submodule("libs/sub", "deep");
        let (wt, common) = worktree_of(&repo, "topic");

        let report = seed_from_mirrors(&RealGit, &wt, &common);
        assert_eq!(
            report.seeded,
            vec!["libs/sub".to_string(), "libs/sub/deep".to_string()]
        );
        assert!(wt.join("libs/sub/deep/deep.txt").exists());
    }
}
