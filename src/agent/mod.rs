//! The code-agent boundary (issue #11): detect installed agent CLIs and generate
//! text through the provider-neutral `agent-text` crate. [`AgentClient`] keeps
//! the existing synchronous, injectable public contract; [`RealAgent`] bridges
//! it to `agent_text` on an isolated runtime. A missing binary yields
//! [`Error::AgentUnavailable`]; a non-zero exit yields [`Error::Subprocess`].
//!
//! Subprocess calls are synchronous (`std::process::Command`), matching the
//! other CLI boundaries (`git`, `gh`, hooks).

pub mod model;
pub mod spec;
pub mod types;

use std::path::Path;
use std::process::Command;

use agent_text::{ClaudeCode, Codex, GenerationOptions, GenerationRequest, ReasoningEffort};

use crate::error::{Error, Result};
pub use model::{AgentModel, AgentOptions, Effort};
pub use spec::{AGENTS, AgentKind, AgentSpec, ResultFormat};
pub use types::{AgentRun, AgentVersion, DetectedAgent};

/// Detects and drives code-agent CLIs.
pub trait AgentClient {
    /// Probes one agent on `PATH`. Returns `Ok(None)` if it is not installed,
    /// or `Err` if an installed binary fails to run.
    fn detect(&self, kind: AgentKind) -> Result<Option<DetectedAgent>>;

    /// Generates text non-interactively from `prompt`, with the selected model
    /// and effort (`opts`), and returns the normalized result.
    ///
    /// Production generation is intentionally isolated from `dir` by
    /// `agent-text`; the argument remains part of this compatibility boundary
    /// for injected clients and existing callers.
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
        _dir: &Path,
        opts: &AgentOptions,
    ) -> Result<AgentRun> {
        match kind {
            AgentKind::Claude => generate_with(&ClaudeCode::new(), kind, prompt, opts),
            AgentKind::Codex => generate_with(&Codex::new(), kind, prompt, opts),
        }
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

/// Bridges the synchronous [`AgentClient`] boundary to an async `agent-text`
/// adapter. The scoped thread permits calls from both ordinary CLI code and a
/// running Tokio TUI without nesting runtimes.
fn generate_with(
    agent: &(dyn agent_text::Agent + Sync),
    kind: AgentKind,
    prompt: &str,
    opts: &AgentOptions,
) -> Result<AgentRun> {
    let request = GenerationRequest::new(prompt).with_options(GenerationOptions {
        model: (opts.model != AgentModel::Default).then(|| opts.model.id().to_string()),
        reasoning_effort: Some(match opts.effort {
            Effort::Low => ReasoningEffort::Low,
            Effort::Medium => ReasoningEffort::Medium,
            Effort::High => ReasoningEffort::High,
            Effort::XHigh => ReasoningEffort::XHigh,
            Effort::Max => ReasoningEffort::Max,
        }),
        timeout: None,
    });
    let generated = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(Error::from)?;
                runtime
                    .block_on(agent.generate(&request))
                    .map_err(|error| map_agent_text_error(kind, error))
            })
            .join()
    })
    .map_err(|_| Error::operation("code-agent generation thread panicked"))??;

    let usage = generated.usage.as_ref();
    let raw = serde_json::json!({
        "model": generated.model,
        "elapsed_ms": generated.elapsed.as_millis(),
        "usage": usage.map(|usage| serde_json::json!({
            "input_tokens": usage.total_input_tokens,
            "cached_input_tokens": usage.cached_input_tokens,
            "cache_write_input_tokens": usage.cache_write_input_tokens,
            "output_tokens": usage.output_tokens,
            "cost_usd": usage.cost_usd,
        })),
    });
    Ok(AgentRun {
        kind,
        is_error: false,
        result: generated.text,
        raw,
    })
}

/// Maps provider-neutral generation failures onto wt's stable public errors.
fn map_agent_text_error(kind: AgentKind, error: agent_text::Error) -> Error {
    match error {
        agent_text::Error::Spawn { binary, source } => {
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::AgentUnavailable(format!(
                    "{} is not installed or not on PATH",
                    binary.display()
                ))
            } else {
                Error::AgentUnavailable(format!("failed to run {}: {source}", binary.display()))
            }
        }
        agent_text::Error::Exit { stderr, .. } => Error::Subprocess {
            program: kind.as_str().to_string(),
            stderr,
        },
        other => Error::operation(format!("code-agent generation failed: {other}")),
    }
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

    #[test]
    fn real_agent_detect_codex_does_not_error() {
        // `codex` may or may not be installed; absence is a successful miss.
        assert!(RealAgent.detect(AgentKind::Codex).is_ok());
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
    }
}
