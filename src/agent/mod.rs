//! The code-agent boundary (issue #11): detect installed agent CLIs and drive
//! them in their JSON output mode. [`AgentClient`] isolates the subprocess work
//! so callers can inject a fake; [`RealAgent`] spawns the real binaries. A
//! missing binary yields [`Error::AgentUnavailable`]; a non-zero exit yields
//! [`Error::Subprocess`].
//!
//! Subprocess calls are synchronous (`std::process::Command`), matching the
//! other CLI boundaries (`git`, `gh`, hooks).

pub mod spec;
pub mod types;

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};
pub use spec::{AGENTS, AgentKind, AgentSpec, ResultFormat};
pub use types::{AgentRun, AgentVersion, DetectedAgent};

/// Detects and drives code-agent CLIs.
pub trait AgentClient {
    /// Probes one agent on `PATH`. Returns `Ok(None)` if it is not installed,
    /// or `Err` if an installed binary fails to run.
    fn detect(&self, kind: AgentKind) -> Result<Option<DetectedAgent>>;

    /// Runs `kind` non-interactively on `prompt` in `dir`, in the agent's JSON
    /// output mode, and returns the normalized result.
    fn run(&self, kind: AgentKind, prompt: &str, dir: &Path) -> Result<AgentRun>;

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

    fn run(&self, kind: AgentKind, prompt: &str, dir: &Path) -> Result<AgentRun> {
        run_with(kind.spec().binary, kind, kind.spec(), prompt, dir)
    }
}

/// Detects `kind` by running `binary` with the spec's version args. Split from
/// [`RealAgent::detect`] so tests can drive every branch with a stand-in
/// binary. A missing binary maps to `Ok(None)`; other failures propagate.
fn detect_with(binary: &str, kind: AgentKind, spec: &AgentSpec) -> Result<Option<DetectedAgent>> {
    match run_agent(binary, None, &spec::version_argv(spec)) {
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
) -> Result<AgentRun> {
    let argv = spec::prompt_argv(spec, prompt);
    let stdout = run_agent(binary, Some(dir), &argv)?;
    spec::parse_result(kind, spec.result_format, &stdout)
}

/// Runs an agent `binary` (optionally in `dir`), mapping a missing binary to
/// [`Error::AgentUnavailable`] and a non-zero exit to [`Error::Subprocess`].
/// Mirrors `gh`'s `run_gh` helper.
fn run_agent(binary: &str, dir: Option<&Path>, args: &[String]) -> Result<String> {
    let mut cmd = Command::new(binary);
    if let Some(dir) = dir {
        cmd.current_dir(dir);
    }
    cmd.args(args);
    let output = match cmd.output() {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::AgentUnavailable(format!(
                "{binary} is not installed or not on PATH"
            )));
        }
        Err(e) => {
            return Err(Error::AgentUnavailable(format!(
                "failed to run {binary}: {e}"
            )));
        }
    };
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Err(Error::Subprocess {
        program: binary.to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
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

        fn run(&self, kind: AgentKind, prompt: &str, _dir: &Path) -> Result<AgentRun> {
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
            .run(AgentKind::Claude, "hi", dir.path())
            .unwrap();
        assert_eq!(run.result, "hi");
        assert!(!run.is_error);
    }

    #[test]
    fn run_agent_maps_missing_binary_to_unavailable() {
        let err = run_agent(MISSING, None, &["--version".to_string()]).unwrap_err();
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
            result_format: ResultFormat::SingleObject,
        };

        #[test]
        fn run_agent_returns_stdout_on_success() {
            let out =
                run_agent("sh", None, &["-c".to_string(), "printf hello".to_string()]).unwrap();
            assert_eq!(out, "hello");
        }

        #[test]
        fn run_agent_maps_nonzero_exit_to_subprocess() {
            let err = run_agent("sh", None, &["-c".to_string(), "exit 3".to_string()]).unwrap_err();
            match err {
                Error::Subprocess { program, .. } => assert_eq!(program, "sh"),
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
            )
            .unwrap();
            assert!(!run.is_error);
            assert_eq!(run.result, "ok");
        }
    }
}
