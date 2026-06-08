//! The known code-agent CLIs and their per-agent invocation differences,
//! encoded as a static table so adding an agent is a single data literal
//! (issue #11). All per-agent-variable logic — argv construction and version
//! and result parsing — lives here as pure functions, directly unit-testable
//! without spawning a process.

use serde::Serialize;

use crate::agent::types::{AgentRun, AgentVersion, ClaudeResult};
use crate::error::Result;

/// A code-agent CLI that `wt` knows how to detect and drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    /// Anthropic's Claude Code (`claude`).
    Claude,
}

impl AgentKind {
    /// Every known agent kind, in display order.
    pub fn all() -> &'static [AgentKind] {
        &[AgentKind::Claude]
    }

    /// The static [`AgentSpec`] for this kind. An exhaustive `match` (rather
    /// than a fallible table lookup) keeps this panic-free.
    pub fn spec(self) -> &'static AgentSpec {
        match self {
            AgentKind::Claude => &AGENTS[0],
        }
    }

    /// The stable lowercase identifier (the binary name, e.g. `"claude"`).
    pub fn as_str(self) -> &'static str {
        self.spec().binary
    }

    /// Parses a lowercase kind identifier, returning `None` if unknown.
    pub fn parse(s: &str) -> Option<AgentKind> {
        AgentKind::all().iter().copied().find(|k| k.as_str() == s)
    }
}

/// How an agent's JSON output mode frames its result. New formats (e.g. a
/// JSON-lines event stream) are added here as more agents are supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultFormat {
    /// A single JSON object on stdout (e.g. `claude -p --output-format json`).
    SingleObject,
}

/// Everything `wt` needs to detect and drive one agent CLI.
#[derive(Debug, Clone, Copy)]
pub struct AgentSpec {
    /// The agent kind this spec describes.
    pub kind: AgentKind,
    /// The binary name as found on `PATH` (e.g. `"claude"`).
    pub binary: &'static str,
    /// Arguments that print the version (e.g. `["--version"]`).
    pub version_args: &'static [&'static str],
    /// Fixed leading arguments for a non-interactive run, before the prompt and
    /// the JSON flag (e.g. `["-p"]`).
    pub run_args: &'static [&'static str],
    /// Whether the prompt is passed as a positional argument after `run_args`.
    pub prompt_positional: bool,
    /// Arguments that select JSON output (e.g. `["--output-format", "json"]`).
    pub json_args: &'static [&'static str],
    /// How to parse stdout in JSON mode.
    pub result_format: ResultFormat,
}

/// The known agents. Add a new agent by appending one literal here.
pub static AGENTS: &[AgentSpec] = &[AgentSpec {
    kind: AgentKind::Claude,
    binary: "claude",
    version_args: &["--version"],
    run_args: &["-p"],
    prompt_positional: true,
    json_args: &["--output-format", "json"],
    result_format: ResultFormat::SingleObject,
}];

/// Builds the version-probe argv for `spec`.
pub fn version_argv(spec: &AgentSpec) -> Vec<String> {
    spec.version_args.iter().map(|s| s.to_string()).collect()
}

/// Builds the full non-interactive, JSON-mode argv for `spec` and `prompt`:
/// `run_args`, then the prompt (when positional), then `json_args`. The prompt
/// is a single argv element — never shell-interpolated — so it needs no
/// quoting and cannot inject extra arguments.
pub fn prompt_argv(spec: &AgentSpec, prompt: &str) -> Vec<String> {
    let mut argv: Vec<String> = spec.run_args.iter().map(|s| s.to_string()).collect();
    if spec.prompt_positional {
        argv.push(prompt.to_string());
    }
    argv.extend(spec.json_args.iter().map(|s| s.to_string()));
    argv
}

/// Extracts a best-effort version from `--version` output: the first
/// `MAJOR.MINOR[.PATCH]`-shaped token on the first line. No semver crate is
/// used (matching repo convention); the trimmed raw line is preserved too.
pub fn parse_version(raw_stdout: &str) -> AgentVersion {
    let raw = raw_stdout.lines().next().unwrap_or("").trim().to_string();
    AgentVersion {
        version: extract_version(&raw),
        raw,
    }
}

/// Finds the first `\d+\.\d+(\.\d+)*`-shaped run in `text` (at least
/// `MAJOR.MINOR`), trimming any trailing dot.
fn extract_version(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        let token = text[start..i].trim_end_matches('.');
        if token.split('.').count() >= 2 && token.split('.').all(|part| !part.is_empty()) {
            return Some(token.to_string());
        }
    }
    None
}

/// Parses JSON-mode stdout into a normalized [`AgentRun`] for `kind`, per
/// `format`. Malformed JSON maps to [`crate::error::Error::Json`].
pub fn parse_result(kind: AgentKind, format: ResultFormat, stdout: &str) -> Result<AgentRun> {
    match format {
        ResultFormat::SingleObject => {
            let raw: serde_json::Value = serde_json::from_str(stdout)?;
            let parsed: ClaudeResult = serde_json::from_value(raw.clone())?;
            Ok(AgentRun {
                kind,
                is_error: parsed.is_error,
                result: parsed.result,
                raw,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_a_matching_spec() {
        for &kind in AgentKind::all() {
            assert_eq!(kind.spec().kind, kind);
        }
    }

    #[test]
    fn kind_parse_roundtrips_and_rejects_unknown() {
        for &kind in AgentKind::all() {
            assert_eq!(AgentKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(AgentKind::parse("nope"), None);
    }

    #[test]
    fn kind_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&AgentKind::Claude).unwrap(),
            "\"claude\""
        );
    }

    #[test]
    fn version_argv_is_version_args() {
        assert_eq!(
            version_argv(AgentKind::Claude.spec()),
            vec!["--version".to_string()]
        );
    }

    #[test]
    fn prompt_argv_orders_run_then_prompt_then_json() {
        let argv = prompt_argv(AgentKind::Claude.spec(), "do a thing");
        assert_eq!(
            argv,
            vec![
                "-p".to_string(),
                "do a thing".to_string(),
                "--output-format".to_string(),
                "json".to_string(),
            ]
        );
        // A prompt with spaces and quotes stays a single argv element.
        let tricky = prompt_argv(AgentKind::Claude.spec(), "a \"quoted\" $arg; rm -rf");
        assert_eq!(tricky[1], "a \"quoted\" $arg; rm -rf");
    }

    #[test]
    fn parse_version_extracts_semver() {
        assert_eq!(
            parse_version("1.2.3 (Claude Code)").version,
            Some("1.2.3".to_string())
        );
        assert_eq!(parse_version("claude 0.4").version, Some("0.4".to_string()));
        assert_eq!(
            parse_version("v2.10.0\nextra line").version,
            Some("2.10.0".to_string())
        );
        // A trailing dot is trimmed; a lone integer is not a version.
        assert_eq!(parse_version("1.2.").version, Some("1.2".to_string()));
        assert_eq!(parse_version("build 12").version, None);
        let none = parse_version("weird-output");
        assert_eq!(none.version, None);
        assert_eq!(none.raw, "weird-output");
    }

    #[test]
    fn parse_result_single_object_ok() {
        let run = parse_result(
            AgentKind::Claude,
            ResultFormat::SingleObject,
            r#"{"is_error": false, "result": "done", "extra": 1}"#,
        )
        .unwrap();
        assert!(!run.is_error);
        assert_eq!(run.result, "done");
        assert_eq!(run.kind, AgentKind::Claude);
        // Unmodeled fields are preserved in `raw`.
        assert_eq!(run.raw.get("extra").and_then(|v| v.as_i64()), Some(1));
    }

    #[test]
    fn parse_result_single_object_error_flag() {
        let run = parse_result(
            AgentKind::Claude,
            ResultFormat::SingleObject,
            r#"{"is_error": true, "result": "boom"}"#,
        )
        .unwrap();
        assert!(run.is_error);
        assert_eq!(run.result, "boom");
    }

    #[test]
    fn parse_result_rejects_malformed_json() {
        let err =
            parse_result(AgentKind::Claude, ResultFormat::SingleObject, "not json").unwrap_err();
        assert!(matches!(err, crate::error::Error::Json(_)));
    }
}
