//! Test-only helpers shared across the crate's unit tests.
//!
//! Provides an in-memory [`SharedBuf`] writer whose contents can be inspected
//! after a command runs, and [`test_cx`] which wires a [`Cx`] to two such
//! buffers plus a fixed environment.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use crate::cx::{Cx, Env, Stream};
use crate::git::cli::{GitCli, RealGit};

/// A cloneable in-memory writer whose contents can be inspected after writes.
///
/// Clones share the same underlying buffer, so a clone handed to a [`Stream`]
/// can be read back through the original handle.
#[derive(Clone, Default)]
pub(crate) struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    /// Creates an empty buffer.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the bytes written so far, decoded as UTF-8 (lossy).
    pub(crate) fn contents(&self) -> String {
        let guard = self.0.lock().expect("buffer lock poisoned");
        String::from_utf8_lossy(&guard).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("buffer lock poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A [`Cx`] wired to in-memory buffers, with handles to inspect what was
/// written to stdout (`out`) and stderr (`err`).
pub(crate) struct TestCx {
    /// The context under test.
    pub cx: Cx,
    /// Captures everything written to stdout.
    pub out: SharedBuf,
    /// Captures everything written to stderr.
    pub err: SharedBuf,
}

/// Builds a [`TestCx`] over in-memory buffers, the given environment pairs, and
/// working directory, using the real `git` handle. Both streams report
/// themselves as non-TTYs.
pub(crate) fn test_cx(env: &[(&str, &str)], cwd: &str) -> TestCx {
    test_cx_with_git(env, cwd, Arc::new(RealGit))
}

/// Like [`test_cx`] but with an injected `git` handle (e.g. a fake).
pub(crate) fn test_cx_with_git(
    env: &[(&str, &str)],
    cwd: &str,
    git: Arc<dyn GitCli + Send + Sync>,
) -> TestCx {
    let out = SharedBuf::new();
    let err = SharedBuf::new();
    let env_map: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let cx = Cx::new(
        Stream::new(Box::new(out.clone()), false),
        Stream::new(Box::new(err.clone()), false),
        Env::from_map(env_map),
        PathBuf::from(cwd),
        git,
    );
    TestCx { cx, out, err }
}

/// A real, throwaway Git repository for integration tests. Worktrees are created
/// as siblings of the repo *inside* the same temp dir, so they are cleaned up
/// when the [`TestRepo`] is dropped. Git runs with an isolated config so the
/// host's `~/.gitconfig` cannot affect tests.
pub(crate) struct TestRepo {
    _dir: TempDir,
    root: PathBuf,
}

// The fixture grows across stages; not every helper is used by every stage.
#[allow(dead_code)]
impl TestRepo {
    /// Initializes a normal repo on branch `main` with one initial commit.
    pub(crate) fn init() -> TestRepo {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).expect("mkdir repo");
        run_git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("README.md"), "init\n").expect("write readme");
        run_git(&root, &["add", "-A"]);
        run_git(&root, &["commit", "-q", "-m", "init"]);
        TestRepo { _dir: dir, root }
    }

    /// Initializes a bare repository on branch `main`.
    pub(crate) fn init_bare() -> TestRepo {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("bare.git");
        std::fs::create_dir_all(&root).expect("mkdir bare");
        run_git(&root, &["init", "-q", "--bare", "-b", "main"]);
        TestRepo { _dir: dir, root }
    }

    /// The primary worktree (or bare repo) root.
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// Runs an arbitrary `git` command in the repo and returns stdout.
    pub(crate) fn git(&self, args: &[&str]) -> String {
        run_git(&self.root, args)
    }

    /// Creates a linked worktree for a new branch at `rel_path` (relative to the
    /// repo root).
    pub(crate) fn add_worktree(&self, branch: &str, rel_path: &str) {
        run_git(
            &self.root,
            &["worktree", "add", "-q", "-b", branch, rel_path],
        );
    }

    /// Writes a file (creating parent directories) in the repo's working tree.
    pub(crate) fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, content).expect("write file");
    }

    /// Stages all changes and commits them.
    pub(crate) fn commit_all(&self, message: &str) {
        run_git(&self.root, &["add", "-A"]);
        run_git(&self.root, &["commit", "-q", "-m", message]);
    }
}

/// Runs `git -C <dir> <args>` with isolated config and identity, asserting
/// success, and returns stdout.
fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "wt Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "wt Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
