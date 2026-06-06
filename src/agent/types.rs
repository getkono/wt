//! Normalized code-agent results and the JSON shapes emitted by agents'
//! output modes (issue #11).

use serde::{Deserialize, Serialize};

use crate::agent::spec::AgentKind;

/// A code-agent CLI detected on `PATH`, with its resolved version.
#[derive(Debug, Clone, Serialize)]
pub struct DetectedAgent {
    /// Which agent was detected.
    pub kind: AgentKind,
    /// The binary name found on `PATH`.
    pub binary: String,
    /// The parsed version information.
    pub version: AgentVersion,
}

/// A parsed `--version` result: a best-effort version number plus the raw line.
#[derive(Debug, Clone, Serialize)]
pub struct AgentVersion {
    /// Best-effort `MAJOR.MINOR[.PATCH]` extracted from the output, or `None`
    /// if no version-shaped token was found.
    pub version: Option<String>,
    /// The raw first line of `--version` output, trimmed.
    pub raw: String,
}

/// A normalized end-to-end run result, independent of which agent produced it.
#[derive(Debug, Clone, Serialize)]
pub struct AgentRun {
    /// Which agent produced this result.
    pub kind: AgentKind,
    /// Whether the agent reported an error.
    pub is_error: bool,
    /// The agent's final textual result.
    pub result: String,
    /// The raw JSON value the agent emitted, preserved for agent-specific
    /// fields not in the normalized shape.
    pub raw: serde_json::Value,
}

/// The JSON shape of `claude -p --output-format json`. Only the normalized
/// fields are named; everything else rides along in [`AgentRun::raw`].
#[derive(Debug, Clone, Deserialize)]
pub struct ClaudeResult {
    /// The agent's final result text.
    #[serde(default)]
    pub result: String,
    /// Whether the agent flagged an error.
    #[serde(default)]
    pub is_error: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_claude_result() {
        let json = r#"{"type":"result","is_error":false,"result":"hello","total_cost_usd":0.01}"#;
        let parsed: ClaudeResult = serde_json::from_str(json).unwrap();
        assert!(!parsed.is_error);
        assert_eq!(parsed.result, "hello");
    }

    #[test]
    fn claude_result_defaults_missing_fields() {
        let parsed: ClaudeResult = serde_json::from_str("{}").unwrap();
        assert!(!parsed.is_error);
        assert_eq!(parsed.result, "");
    }

    #[test]
    fn agent_run_serializes_to_json() {
        let run = AgentRun {
            kind: AgentKind::Claude,
            is_error: false,
            result: "hi".into(),
            raw: serde_json::json!({"result": "hi"}),
        };
        let serialized = serde_json::to_string(&run).unwrap();
        assert!(serialized.contains("\"kind\":\"claude\""));
        assert!(serialized.contains("\"result\":\"hi\""));
        assert!(serialized.contains("\"is_error\":false"));
    }

    #[test]
    fn detected_agent_serializes() {
        let detected = DetectedAgent {
            kind: AgentKind::Claude,
            binary: "claude".into(),
            version: AgentVersion {
                version: Some("1.2.3".into()),
                raw: "1.2.3 (Claude Code)".into(),
            },
        };
        let serialized = serde_json::to_string(&detected).unwrap();
        assert!(serialized.contains("\"binary\":\"claude\""));
        assert!(serialized.contains("\"version\":\"1.2.3\""));
    }
}
