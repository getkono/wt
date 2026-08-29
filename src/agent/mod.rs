//! The code-agent boundary (issue #11): detect installed agent CLIs and drive
//! them in their JSON output mode. [`AgentClient`] isolates the subprocess work
//! so callers can inject a fake; [`RealAgent`] spawns the real binaries. A
//! missing binary yields [`Error::AgentUnavailable`]; a non-zero exit yields
//! [`Error::Subprocess`].
//!
//! Subprocess calls are synchronous (`std::process::Command`), matching the
//! other CLI boundaries (`git`, `gh`, hooks).

pub mod model;
pub mod spec;
pub mod types;

use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
pub use model::{AgentModel, AgentOptions, Effort};
pub use spec::{AGENTS, AgentKind, AgentSpec, ResultFormat};
pub use types::{AgentRun, AgentVersion, DetectedAgent};

/// Detects and drives code-agent CLIs.
pub trait AgentClient {
    /// Probes one agent on `PATH`. Returns `Ok(None)` if it is not installed,
    /// or `Err` if an installed binary fails to run.
    fn detect(&self, kind: AgentKind) -> Result<Option<DetectedAgent>>;

    /// Runs `kind` non-interactively on `prompt` in `dir`, in the agent's JSON
    /// output mode, with the selected model and effort (`opts`), and returns the
    /// normalized result.
    fn run(
        &self,
        kind: AgentKind,
        prompt: &str,
        dir: &Path,
        opts: &AgentOptions,
    ) -> Result<AgentRun>;

    /// Probes every known agent on `PATH`, returning those found. Agents that
    /// are not installed are omitted (that is not an error).
    fn detect_all(&self) -> Vec<DetectedAgent> {
        AgentKind::all()
            .iter()
            .filter_map(|&kind| self.detect(kind).ok().flatten())
            .collect()
    }
}

/// The production [`AgentClient`] that spawns the real agent binaries.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealAgent;

impl AgentClient for RealAgent {
    fn detect(&self, kind: AgentKind) -> Result<Option<DetectedAgent>> {
        detect_with(kind.spec().binary, kind, kind.spec())
    }

    fn run(
        &self,
        kind: AgentKind,
        prompt: &str,
        dir: &Path,
        opts: &AgentOptions,
    ) -> Result<AgentRun> {
        run_with(kind.spec().binary, kind, kind.spec(), prompt, dir, opts)
    }
}

/// Detects `kind` by running `binary` with the spec's version args. Split from
/// [`RealAgent::detect`] so tests can drive every branch with a stand-in
/// binary. A missing binary maps to `Ok(None)`; other failures propagate.
fn detect_with(binary: &str, kind: AgentKind, spec: &AgentSpec) -> Result<Option<DetectedAgent>> {
    match run_agent(binary, None, &spec::version_argv(spec), None) {
        Ok(stdout) => Ok(Some(DetectedAgent {
            kind,
            binary: binary.to_string(),
            version: spec::parse_version(&stdout),
        })),
        Err(Error::AgentUnavailable(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Runs `binary` on `prompt` in `dir` per `spec`, parsing the JSON result.
/// Split from [`RealAgent::run`] for the same testability reason.
fn run_with(
    binary: &str,
    kind: AgentKind,
    spec: &AgentSpec,
    prompt: &str,
    dir: &Path,
    opts: &AgentOptions,
) -> Result<AgentRun> {
    let prompt = spec::apply_effort(opts.effort, prompt);
    let argv = spec::prompt_argv(spec, &prompt, opts.model);
    let stdout = run_agent(binary, Some(dir), &argv, opts.timeout)?;
    spec::parse_result(kind, spec.result_format, &stdout)
}

/// Runs an agent `binary` (optionally in `dir`), mapping a missing binary to
/// [`Error::AgentUnavailable`] and a non-zero exit to [`Error::Subprocess`].
/// Mirrors `gh`'s `run_gh` helper.
///
/// With `timeout` set, the child is killed and [`Error::AgentTimeout`] returned
/// once the deadline passes. `None` waits indefinitely — the historical
/// behaviour, kept for version detection and for callers that pass no deadline.
fn run_agent(
    binary: &str,
    dir: Option<&Path>,
    args: &[String],
    timeout: Option<Duration>,
) -> Result<String> {
    let mut cmd = Command::new(binary);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    cmd.args(args);

    let Some(limit) = timeout else {
        return match cmd.output() {
            Ok(output) => finish(binary, output.status, &output.stdout, &output.stderr),
            Err(e) => Err(spawn_error(binary, &e)),
        };
    };

    // `Command::output()` blocks until the pipes close, so it cannot be given a
    // deadline. Read the pipes on their own threads and keep the `Child` here,
    // so this thread still owns the handle it needs to kill.
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Err(spawn_error(binary, &e)),
    };
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    // Draining both pipes concurrently matters: a child that fills the stderr
    // pipe buffer blocks forever if only stdout is being read.
    let out_reader = std::thread::spawn(move || read_pipe(out_pipe.as_mut()));
    let err_reader = std::thread::spawn(move || read_pipe(err_pipe.as_mut()));

    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => return Err(spawn_error(binary, &e)),
        }
        if Instant::now() >= deadline {
            // Killing closes the child's pipes, which is what lets the reader
            // threads below finish rather than block forever.
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    match status {
        Some(status) => {
            // A panicking reader yields no output rather than poisoning the run.
            let stdout = out_reader.join().unwrap_or_default();
            let stderr = err_reader.join().unwrap_or_default();
            finish(binary, status, &stdout, &stderr)
        }
        None => {
            // Deliberately *not* joined. Killing the child does not close the
            // pipes if it left a grandchild holding them — an agent CLI that is
            // a wrapper script is exactly that shape — so a reader would block
            // for as long as the grandchild lives, which is precisely what the
            // deadline exists to prevent. The output is unwanted anyway, so the
            // readers are detached; they end on their own when the pipes close.
            drop(out_reader);
            drop(err_reader);
            Err(Error::AgentTimeout {
                binary: binary.to_string(),
                // Sub-second deadlines still report `1s`; the message is for
                // humans, and "did not respond within 0s" reads as a bug.
                seconds: limit.as_secs().max(1),
            })
        }
    }
}

/// How often the deadline loop checks whether the child has exited. Short
/// enough that a fast agent is not held up perceptibly, long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Drains a child pipe to end, yielding empty bytes if it is absent or fails.
fn read_pipe(pipe: Option<&mut impl Read>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(pipe) = pipe {
        let _ = pipe.read_to_end(&mut buf);
    }
    buf
}

/// Maps a spawn failure to [`Error::AgentUnavailable`], distinguishing a missing
/// binary from any other launch failure.
fn spawn_error(binary: &str, e: &std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::AgentUnavailable(format!("{binary} is not installed or not on PATH"))
    } else {
        Error::AgentUnavailable(format!("failed to run {binary}: {e}"))
    }
}

/// Maps a finished process to its stdout, or to [`Error::Subprocess`].
fn finish(binary: &str, status: ExitStatus, stdout: &[u8], stderr: &[u8]) -> Result<String> {
    if status.success() {
        return Ok(String::from_utf8_lossy(stdout).into_owned());
    }
    Err(Error::Subprocess {
        program: binary.to_string(),
        stderr: String::from_utf8_lossy(stderr).trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A nonexistent binary name, used to exercise the not-found path.
    const MISSING: &str = "wt-nonexistent-agent-binary-xyzzy";

    /// Behaviors for the in-test [`AgentClient`] fake, to cover `detect_all`.
    enum Behavior {
        Found,
        Missing,
        Failing,
    }

    struct Fake(Behavior);

    impl AgentClient for Fake {
        fn detect(&self, kind: AgentKind) -> Result<Option<DetectedAgent>> {
            match self.0 {
                Behavior::Found => Ok(Some(DetectedAgent {
                    kind,
                    binary: kind.as_str().to_string(),
                    version: AgentVersion {
                        version: None,
                        raw: String::new(),
                    },
                })),
                Behavior::Missing => Ok(None),
                Behavior::Failing => Err(Error::operation("boom")),
            }
        }

        fn run(
            &self,
            kind: AgentKind,
            prompt: &str,
            _dir: &Path,
            _opts: &AgentOptions,
        ) -> Result<AgentRun> {
            Ok(AgentRun {
                kind,
                is_error: false,
                result: prompt.to_string(),
                raw: serde_json::Value::Null,
            })
        }
    }

    #[test]
    fn detect_all_keeps_found_drops_missing_and_failing() {
        assert_eq!(
            Fake(Behavior::Found).detect_all().len(),
            AgentKind::all().len()
        );
        assert!(Fake(Behavior::Missing).detect_all().is_empty());
        // An installed-but-erroring agent is dropped by `detect_all` (errors
        // surface only through `detect`).
        assert!(Fake(Behavior::Failing).detect_all().is_empty());
    }

    #[test]
    fn fake_run_returns_normalized_result() {
        let dir = tempfile::tempdir().unwrap();
        let run = Fake(Behavior::Found)
            .run(
                AgentKind::Claude,
                "hi",
                dir.path(),
                &AgentOptions::default(),
            )
            .unwrap();
        assert_eq!(run.result, "hi");
        assert!(!run.is_error);
    }

    #[test]
    fn run_agent_maps_missing_binary_to_unavailable() {
        let err = run_agent(MISSING, None, &["--version".to_string()], None).unwrap_err();
        assert!(matches!(err, Error::AgentUnavailable(_)));
    }

    #[test]
    fn detect_with_returns_none_for_missing_binary() {
        let result = detect_with(MISSING, AgentKind::Claude, AgentKind::Claude.spec()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn real_agent_detect_claude_does_not_error() {
        // `claude` may or may not be installed in the test environment; either
        // way detection must not error (absent => Ok(None)).
        assert!(RealAgent.detect(AgentKind::Claude).is_ok());
    }

    // The real-subprocess paths below shell out to `sh`, which the existing
    // hook tests also rely on; they run on the Unix CI where coverage is taken.
    #[cfg(unix)]
    mod unix {
        use super::*;

        /// A spec that drives `sh` to print a version-shaped line.
        const SH_VERSION: AgentSpec = AgentSpec {
            kind: AgentKind::Claude,
            binary: "sh",
            version_args: &["-c", "echo '9.9.9 (test agent)'"],
            run_args: &["-c", "printf '{\"is_error\":false,\"result\":\"ok\"}'"],
            prompt_positional: true,
            json_args: &[],
            model_flag: "",
            result_format: ResultFormat::SingleObject,
        };

        /// A spec whose version probe exits non-zero.
        const SH_FAIL: AgentSpec = AgentSpec {
            kind: AgentKind::Claude,
            binary: "sh",
            version_args: &["-c", "exit 1"],
            run_args: &["-c", "true"],
            prompt_positional: true,
            json_args: &[],
            model_flag: "",
            result_format: ResultFormat::SingleObject,
        };

        #[test]
        fn run_agent_returns_stdout_on_success() {
            let out = run_agent(
                "sh",
                None,
                &["-c".to_string(), "printf hello".to_string()],
                None,
            )
            .unwrap();
            assert_eq!(out, "hello");
        }

        #[test]
        fn run_agent_maps_nonzero_exit_to_subprocess() {
            let err =
                run_agent("sh", None, &["-c".to_string(), "exit 3".to_string()], None).unwrap_err();
            match err {
                Error::Subprocess { program, .. } => assert_eq!(program, "sh"),
                other => panic!("expected subprocess error, got {other:?}"),
            }
        }

        #[test]
        fn run_agent_kills_a_child_that_outlives_its_deadline() {
            // Two defects in one test.
            //
            // First: `Command::output()` waits forever, so an agent that hangs
            // used to hang `wt`. Sleeping far longer than the deadline proves the
            // deadline — not the sleep — is what ends the run.
            //
            // Second, and the subtler one: `sleep 30 & wait` makes `sh` fork a
            // *grandchild* that inherits the stdout/stderr pipes. Killing the
            // child does not close them, so joining the reader threads blocks
            // until the grandchild dies — reintroducing the full 30s wait behind
            // a timeout that appears to work. An agent CLI that is a wrapper
            // script has exactly this shape, so the plain `sleep 30` this test
            // first used was too weak: it passes on a shell that `exec`s.
            let started = Instant::now();
            let err = run_agent(
                "sh",
                None,
                &["-c".to_string(), "sleep 30 & wait".to_string()],
                Some(Duration::from_millis(100)),
            )
            .unwrap_err();
            let elapsed = started.elapsed();
            match err {
                Error::AgentTimeout { binary, seconds } => {
                    assert_eq!(binary, "sh");
                    // Sub-second deadlines still report a whole second.
                    assert_eq!(seconds, 1);
                }
                other => panic!("expected a timeout, got {other:?}"),
            }
            assert!(
                elapsed < Duration::from_secs(10),
                "returned after {elapsed:?}; the child was not killed"
            );
        }

        #[test]
        fn a_deadline_does_not_disturb_a_process_that_finishes() {
            // The deadline path reads the pipes on separate threads, so prove it
            // still returns stdout intact rather than only working on timeout.
            let out = run_agent(
                "sh",
                None,
                &["-c".to_string(), "printf hello".to_string()],
                Some(Duration::from_secs(30)),
            )
            .unwrap();
            assert_eq!(out, "hello");
        }

        #[test]
        fn a_deadline_still_maps_a_nonzero_exit_to_subprocess() {
            let err = run_agent(
                "sh",
                None,
                &["-c".to_string(), "printf oops >&2; exit 3".to_string()],
                Some(Duration::from_secs(30)),
            )
            .unwrap_err();
            match err {
                Error::Subprocess { program, stderr } => {
                    assert_eq!(program, "sh");
                    // Proves stderr is drained too, not just stdout.
                    assert_eq!(stderr, "oops");
                }
                other => panic!("expected subprocess error, got {other:?}"),
            }
        }

        #[test]
        fn detect_with_parses_version_from_real_process() {
            let detected = detect_with("sh", AgentKind::Claude, &SH_VERSION)
                .unwrap()
                .unwrap();
            assert_eq!(detected.binary, "sh");
            assert_eq!(detected.version.version, Some("9.9.9".to_string()));
        }

        #[test]
        fn detect_with_propagates_non_unavailable_errors() {
            let err = detect_with("sh", AgentKind::Claude, &SH_FAIL).unwrap_err();
            assert!(matches!(err, Error::Subprocess { .. }));
        }

        #[test]
        fn run_with_invokes_and_parses_result() {
            let dir = tempfile::tempdir().unwrap();
            let run = run_with(
                "sh",
                AgentKind::Claude,
                &SH_VERSION,
                "my prompt",
                dir.path(),
                &AgentOptions::default(),
            )
            .unwrap();
            assert!(!run.is_error);
            assert_eq!(run.result, "ok");
        }
    }
}
