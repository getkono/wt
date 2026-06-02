//! Copying Git-ignored local files into newly created worktrees (spec §8).
//!
//! On `new`/`pr`, files in the source worktree matching the configured `copy`
//! glob patterns are copied into the new worktree, except: tracked files (they
//! come from the checkout) and files that already exist in the target (never
//! overwritten). The `.git` directory is never traversed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::error::{Error, Result};
use crate::git::cli::GitCli;

/// The outcome of a copy step.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CopyOutcome {
    /// Relative paths that were copied.
    pub copied: Vec<PathBuf>,
    /// Relative paths skipped because the target already existed.
    pub skipped_existing: Vec<PathBuf>,
}

/// Copies ignored files matching `patterns` from `source` into `target`
/// (spec §8). Tracked files and existing targets are skipped.
pub fn copy_ignored_files(
    git: &dyn GitCli,
    source: &Path,
    target: &Path,
    patterns: &[String],
) -> Result<CopyOutcome> {
    let mut outcome = CopyOutcome::default();
    if patterns.is_empty() {
        return Ok(outcome);
    }
    let globset = build_globset(patterns)?;
    let tracked = tracked_files(git, source)?;

    for rel in walk_files(source) {
        if !globset.is_match(&rel) || tracked.contains(&rel) {
            continue;
        }
        let destination = target.join(&rel);
        if destination.exists() {
            outcome.skipped_existing.push(rel);
            continue;
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source.join(&rel), &destination)?;
        outcome.copied.push(rel);
    }
    Ok(outcome)
}

/// Compiles the copy patterns into a [`GlobSet`]; an invalid glob is a config
/// error.
fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|e| Error::Config {
            file: "copy".into(),
            key: pattern.clone(),
            reason: format!("invalid glob: {e}"),
        })?;
        builder.add(glob);
    }
    builder.build().map_err(|e| Error::Config {
        file: "copy".into(),
        key: "copy".into(),
        reason: format!("invalid glob set: {e}"),
    })
}

/// The set of tracked files (relative paths) in `source`. A failure to list
/// them is propagated rather than swallowed: copying would otherwise risk
/// overwriting/duplicating tracked files, which spec §8 forbids.
fn tracked_files(git: &dyn GitCli, source: &Path) -> Result<HashSet<PathBuf>> {
    let output = git.run(source, &["ls-files", "-z"])?;
    Ok(output
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect())
}

/// Recursively lists files under `root` (relative paths), skipping the `.git`
/// directory.
fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    walk_into(root, Path::new(""), &mut files);
    files
}

/// Recursive helper for [`walk_files`].
fn walk_into(base: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let dir = base.join(rel);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let child_rel = rel.join(&name);
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => walk_into(base, &child_rel, out),
            Ok(ft) if ft.is_file() => out.push(child_rel),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    #[test]
    fn copies_ignored_files_skipping_tracked_and_existing() {
        let repo = TestRepo::init();
        // Tracked file matching a pattern is skipped.
        repo.write("config.local", "tracked\n");
        repo.commit_all("add tracked local");
        // Untracked ignored files to copy.
        repo.write(".env", "SECRET=1\n");
        repo.write(".config/settings", "x\n");
        repo.write("keep.local", "local\n");

        let target = repo.root().parent().unwrap().join("target");
        std::fs::create_dir_all(&target).unwrap();
        // Pre-existing target file must not be overwritten.
        std::fs::write(target.join(".env"), "EXISTING\n").unwrap();

        let patterns = vec![
            ".env".to_string(),
            "*.local".to_string(),
            ".config/**".to_string(),
        ];
        let outcome = copy_ignored_files(&RealGit, repo.root(), &target, &patterns).unwrap();

        // .env exists in target -> skipped; keep.local + .config/settings copied;
        // config.local is tracked -> not copied.
        assert!(outcome.skipped_existing.contains(&PathBuf::from(".env")));
        assert!(outcome.copied.contains(&PathBuf::from("keep.local")));
        assert!(outcome.copied.contains(&PathBuf::from(".config/settings")));
        assert!(!outcome.copied.contains(&PathBuf::from("config.local")));

        // The pre-existing target was preserved.
        assert_eq!(
            std::fs::read_to_string(target.join(".env")).unwrap(),
            "EXISTING\n"
        );
        assert_eq!(
            std::fs::read_to_string(target.join(".config/settings")).unwrap(),
            "x\n"
        );
    }

    #[test]
    fn empty_patterns_copy_nothing() {
        let repo = TestRepo::init();
        repo.write(".env", "x\n");
        let target = repo.root().parent().unwrap().join("t2");
        std::fs::create_dir_all(&target).unwrap();
        let outcome = copy_ignored_files(&RealGit, repo.root(), &target, &[]).unwrap();
        assert!(outcome.copied.is_empty());
        assert!(!target.join(".env").exists());
    }

    #[test]
    fn invalid_glob_is_config_error() {
        let repo = TestRepo::init();
        let target = repo.root().parent().unwrap().join("t3");
        let err =
            copy_ignored_files(&RealGit, repo.root(), &target, &["[".to_string()]).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
    }

    #[test]
    fn walk_skips_git_directory() {
        let repo = TestRepo::init();
        let files = walk_files(repo.root());
        assert!(files.iter().all(|p| !p.starts_with(".git")));
        assert!(files.contains(&PathBuf::from("README.md")));
    }

    #[test]
    fn ls_files_failure_is_propagated_not_silent() {
        use crate::git::cli::{GitCli, GitOutput};
        // A git that fails `ls-files` must abort the copy (so tracked files are
        // never copied), not silently treat the tracked set as empty (spec §8).
        struct FailLs;
        impl GitCli for FailLs {
            fn run_raw(&self, _repo: &Path, args: &[&str]) -> Result<GitOutput> {
                if args.first() == Some(&"ls-files") {
                    return Ok(GitOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "boom".into(),
                    });
                }
                Ok(GitOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
        }
        let repo = TestRepo::init();
        repo.write(".env", "x\n");
        let target = repo.root().parent().unwrap().join("tfail");
        std::fs::create_dir_all(&target).unwrap();
        let err =
            copy_ignored_files(&FailLs, repo.root(), &target, &[".env".to_string()]).unwrap_err();
        assert!(matches!(err, Error::Subprocess { .. }));
    }
}
