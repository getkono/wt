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

use std::collections::VecDeque;

use crate::agent::{AgentClient, AgentKind, AgentRun, AgentVersion, DetectedAgent};
use crate::cx::{Cx, Env, Input, Stream};
use crate::error::Error;
use crate::gh::{GhClient, OpenPr, PrSummary, PrView, RealGh};
use crate::git::cli::{GitCli, RealGit};

/// A fake [`GhClient`] returning canned PR data or simulating an unavailable
/// `gh`. Records `create_pr`/`edit_pr` args so submit tests can assert them.
#[derive(Default)]
pub(crate) struct FakeGh {
    list: Vec<PrSummary>,
    view: Option<PrView>,
    available: bool,
    default_branch: Option<String>,
    existing_pr: Option<OpenPr>,
    create_stdout: String,
    edit_stdout: String,
    create_args: Arc<Mutex<Vec<Vec<String>>>>,
    edit_args: Arc<Mutex<Vec<Vec<String>>>>,
}

#[allow(dead_code)]
impl FakeGh {
    /// A fake that returns `view` from `view_pr`.
    pub(crate) fn with_view(view: PrView) -> Self {
        FakeGh {
            view: Some(view),
            available: true,
            ..Default::default()
        }
    }

    /// A fake that returns `list` from `list_open_prs`.
    pub(crate) fn with_list(list: Vec<PrSummary>) -> Self {
        FakeGh {
            list,
            available: true,
            ..Default::default()
        }
    }

    /// A fake whose `create_pr`/`edit_pr` succeed and return `stdout`.
    pub(crate) fn sender(stdout: &str) -> Self {
        FakeGh {
            available: true,
            create_stdout: stdout.to_string(),
            edit_stdout: stdout.to_string(),
            ..Default::default()
        }
    }

    /// A fake simulating a missing/unauthenticated `gh`.
    pub(crate) fn unavailable() -> Self {
        FakeGh::default()
    }

    /// Sets the default branch returned by `default_branch`.
    pub(crate) fn with_default_branch(mut self, name: &str) -> Self {
        self.default_branch = Some(name.to_string());
        self
    }

    /// Sets the existing open PR returned by `find_pr_for_branch`.
    pub(crate) fn with_existing_pr(mut self, pr: OpenPr) -> Self {
        self.existing_pr = Some(pr);
        self
    }

    /// The recorded `create_pr` arg lists (one per call).
    pub(crate) fn created_args(&self) -> Vec<Vec<String>> {
        self.create_args.lock().expect("lock poisoned").clone()
    }

    /// The recorded `edit_pr` arg lists (one per call).
    pub(crate) fn edited_args(&self) -> Vec<Vec<String>> {
        self.edit_args.lock().expect("lock poisoned").clone()
    }
}

impl GhClient for FakeGh {
    fn list_open_prs(&self, _dir: &std::path::Path) -> crate::error::Result<Vec<PrSummary>> {
        if self.available {
            Ok(self.list.clone())
        } else {
            Err(Error::GhUnavailable("gh unavailable".into()))
        }
    }

    fn view_pr(&self, _dir: &std::path::Path, _target: &str) -> crate::error::Result<PrView> {
        if !self.available {
            return Err(Error::GhUnavailable("gh unavailable".into()));
        }
        self.view
            .clone()
            .ok_or_else(|| Error::operation("no PR configured"))
    }

    fn default_branch(&self, _dir: &std::path::Path) -> crate::error::Result<Option<String>> {
        // Mirror RealGh: non-fatal, so an unavailable `gh` yields None here.
        Ok(self.default_branch.clone())
    }

    fn find_pr_for_branch(
        &self,
        _dir: &std::path::Path,
        _branch: &str,
    ) -> crate::error::Result<Option<OpenPr>> {
        if !self.available {
            return Err(Error::GhUnavailable("gh unavailable".into()));
        }
        Ok(self.existing_pr.clone())
    }

    fn create_pr(&self, _dir: &std::path::Path, args: &[String]) -> crate::error::Result<String> {
        if !self.available {
            return Err(Error::GhUnavailable("gh unavailable".into()));
        }
        self.create_args
            .lock()
            .expect("lock poisoned")
            .push(args.to_vec());
        Ok(self.create_stdout.clone())
    }

    fn edit_pr(&self, _dir: &std::path::Path, args: &[String]) -> crate::error::Result<String> {
        if !self.available {
            return Err(Error::GhUnavailable("gh unavailable".into()));
        }
        self.edit_args
            .lock()
            .expect("lock poisoned")
            .push(args.to_vec());
        Ok(self.edit_stdout.clone())
    }
}

/// What a [`FakeAgent`] does when driven.
pub(crate) enum AgentBehavior {
    /// Agent present; `run` returns a successful [`AgentRun`] with this result text.
    Draft(String),
    /// Agent present; `run` returns an [`AgentRun`] flagged `is_error` with this text.
    Erroring(String),
    /// Agent absent: `detect` returns `Ok(None)` and `run` returns `AgentUnavailable`.
    Unavailable,
}

/// A fake [`AgentClient`] for tests: returns a canned draft, simulates an
/// absent agent, or an erroring run.
pub(crate) struct FakeAgent(AgentBehavior);

#[allow(dead_code)]
impl FakeAgent {
    /// A present agent whose `run` returns `result` (a successful draft).
    pub(crate) fn drafting(result: &str) -> Self {
        FakeAgent(AgentBehavior::Draft(result.to_string()))
    }

    /// A present agent whose `run` returns an error-flagged result.
    pub(crate) fn erroring(result: &str) -> Self {
        FakeAgent(AgentBehavior::Erroring(result.to_string()))
    }

    /// An absent agent (`detect` → `None`, `run` → `AgentUnavailable`).
    pub(crate) fn unavailable() -> Self {
        FakeAgent(AgentBehavior::Unavailable)
    }
}

impl AgentClient for FakeAgent {
    fn detect(&self, kind: AgentKind) -> crate::error::Result<Option<DetectedAgent>> {
        match self.0 {
            AgentBehavior::Unavailable => Ok(None),
            _ => Ok(Some(DetectedAgent {
                kind,
                binary: kind.as_str().to_string(),
                version: AgentVersion {
                    version: None,
                    raw: String::new(),
                },
            })),
        }
    }

    fn run(&self, kind: AgentKind, _prompt: &str, _dir: &Path) -> crate::error::Result<AgentRun> {
        match &self.0 {
            AgentBehavior::Draft(result) => Ok(AgentRun {
                kind,
                is_error: false,
                result: result.clone(),
                raw: serde_json::Value::Null,
            }),
            AgentBehavior::Erroring(result) => Ok(AgentRun {
                kind,
                is_error: true,
                result: result.clone(),
                raw: serde_json::Value::Null,
            }),
            AgentBehavior::Unavailable => Err(Error::AgentUnavailable("claude unavailable".into())),
        }
    }
}

/// An [`Input`] that returns queued lines (then empty strings), for testing
/// confirmation prompts.
#[derive(Default)]
pub(crate) struct CannedInput(VecDeque<String>);

impl CannedInput {
    /// Builds a canned input from the given responses (newlines are appended).
    pub(crate) fn new(lines: &[&str]) -> Self {
        CannedInput(lines.iter().map(|l| format!("{l}\n")).collect())
    }
}

impl Input for CannedInput {
    fn read_line(&mut self) -> crate::error::Result<String> {
        Ok(self.0.pop_front().unwrap_or_default())
    }
}

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
        Arc::new(RealGh),
        Arc::new(FakeAgent::unavailable()),
        Box::new(CannedInput::default()),
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
///
/// Inherited `GIT_*` location variables are scrubbed first. Git honours
/// `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE` over the `-C <dir>` argument, so if
/// these are present in the environment — as they are when the suite runs from
/// inside a git hook (e.g. the `pre-push` hook git invokes with `GIT_DIR` set) —
/// every `TestRepo` mutation would operate on the developer's real repository
/// instead of the throwaway temp repo, silently corrupting it. Removing them
/// makes `-C <dir>` authoritative so the fixture stays sandboxed.
fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .env_remove("GIT_PREFIX")
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
