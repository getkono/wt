//! Model and effort selection for code-agent generation.
//!
//! [`AgentModel`] provides curated tiers plus a provider-specific custom
//! identifier; [`Effort`] is how hard the agent should work; [`AgentOptions`]
//! bundles the two for one run. All three are pure data with helpers that drive
//! the config layer, CLI flags, and TUI without process or I/O.

use serde::{Serialize, Serializer};

/// A model selection for a code agent. The curated variants encode Claude tiers
/// and custom identifiers allow the interactive CLI to use newer provider
/// models without waiting for another `wt` release.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum AgentModel {
    /// Let the selected agent CLI choose its configured default model.
    Default,
    /// Most capable, highest latency (Claude Opus).
    Opus,
    /// Balanced capability and speed (Claude Sonnet).
    Sonnet,
    /// Fastest and lightest (Claude Haiku) — the default.
    #[default]
    Haiku,
    /// A provider-specific model identifier entered in the CLI.
    Custom(String),
}

impl AgentModel {
    /// Every curated model, in display and cycle order.
    pub fn all() -> &'static [AgentModel] {
        &[AgentModel::Opus, AgentModel::Sonnet, AgentModel::Haiku]
    }

    /// The stable lowercase identifier, used both in config/flags and as the
    /// agent CLI's `--model` value (e.g. `"sonnet"`).
    pub fn id(&self) -> &str {
        match self {
            AgentModel::Default => "",
            AgentModel::Opus => "opus",
            AgentModel::Sonnet => "sonnet",
            AgentModel::Haiku => "haiku",
            AgentModel::Custom(id) => id,
        }
    }

    /// A human-readable label for the status display; tracks the current model
    /// family (the `id` alias always selects the latest of that tier).
    pub fn label(&self) -> &str {
        match self {
            AgentModel::Default => "Default",
            AgentModel::Opus => "Opus 4.8",
            AgentModel::Sonnet => "Sonnet 4.6",
            AgentModel::Haiku => "Haiku 4.5",
            AgentModel::Custom(id) => id,
        }
    }

    /// Parses a model identifier (case-insensitive: `opus`/`sonnet`/`haiku`),
    /// returning `None` if unknown.
    pub fn parse(s: &str) -> Option<AgentModel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "default" => Some(AgentModel::Default),
            "opus" => Some(AgentModel::Opus),
            "sonnet" => Some(AgentModel::Sonnet),
            "haiku" => Some(AgentModel::Haiku),
            _ => None,
        }
    }

    /// Builds a provider-specific model choice from a non-empty identifier.
    pub fn custom(s: &str) -> Option<AgentModel> {
        let id = s.trim();
        (!id.is_empty()).then(|| AgentModel::Custom(id.to_string()))
    }

    /// The next Claude model in cycle order (wraps).
    pub fn next(&self) -> AgentModel {
        match self {
            AgentModel::Default => AgentModel::Opus,
            AgentModel::Opus => AgentModel::Sonnet,
            AgentModel::Sonnet => AgentModel::Haiku,
            AgentModel::Haiku | AgentModel::Custom(_) => AgentModel::Opus,
        }
    }

    /// The previous Claude model in cycle order (wraps).
    pub fn prev(&self) -> AgentModel {
        match self {
            AgentModel::Default => AgentModel::Haiku,
            AgentModel::Opus => AgentModel::Haiku,
            AgentModel::Sonnet => AgentModel::Opus,
            AgentModel::Haiku | AgentModel::Custom(_) => AgentModel::Sonnet,
        }
    }
}

impl Serialize for AgentModel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(if *self == AgentModel::Default {
            "default"
        } else {
            self.id()
        })
    }
}

/// How much effort the agent should spend on a generation. The production
/// adapter maps this onto `agent_text::ReasoningEffort`; the directive remains
/// available for compatibility with custom clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Quick, minimal deliberation — the default.
    #[default]
    Low,
    /// Balanced effort (no directive).
    Medium,
    /// Maximum deliberation and care.
    High,
    /// Extra-high deliberation.
    XHigh,
    /// The provider's maximum supported deliberation.
    Max,
}

impl Effort {
    /// Every effort level, in display and cycle order.
    pub fn all() -> &'static [Effort] {
        &[
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::XHigh,
            Effort::Max,
        ]
    }

    /// The stable lowercase identifier, used in config and `--effort`.
    pub fn id(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// A human-readable label (currently identical to [`Effort::id`]).
    pub fn label(self) -> &'static str {
        self.id()
    }

    /// Parses a supported effort identifier (case-insensitive), returning
    /// `None` if unknown.
    pub fn parse(s: &str) -> Option<Effort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Effort::Low),
            "medium" | "med" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" | "x-high" => Some(Effort::XHigh),
            "max" => Some(Effort::Max),
            _ => None,
        }
    }

    /// The next effort level in cycle order (wraps), for the TUI's `Ctrl-E` key.
    pub fn next(self) -> Effort {
        match self {
            Effort::Low => Effort::Medium,
            Effort::Medium => Effort::High,
            Effort::High => Effort::XHigh,
            Effort::XHigh => Effort::Max,
            Effort::Max => Effort::Low,
        }
    }

    /// The previous effort level in cycle order (wraps), for navigating the TUI's
    /// effort dropdown upward (`↑`).
    pub fn prev(self) -> Effort {
        match self {
            Effort::Low => Effort::Max,
            Effort::Medium => Effort::Low,
            Effort::High => Effort::Medium,
            Effort::XHigh => Effort::High,
            Effort::Max => Effort::XHigh,
        }
    }

    /// A one-line instruction conveying this effort to the agent, prepended to
    /// the prompt; `None` for the balanced baseline (medium).
    pub fn directive(self) -> Option<&'static str> {
        match self {
            Effort::Low => Some("Work quickly and keep your reasoning brief."),
            Effort::Medium => None,
            Effort::High | Effort::XHigh | Effort::Max => {
                Some("Think carefully and review the diff thoroughly before writing.")
            }
        }
    }
}

/// The model and effort selected for a single agent run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentOptions {
    /// The model to drive.
    pub model: AgentModel,
    /// How much effort to spend.
    pub effort: Effort,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_parse_roundtrips_and_rejects_unknown() {
        for m in AgentModel::all() {
            assert_eq!(AgentModel::parse(m.id()), Some(m.clone()));
        }
        assert_eq!(AgentModel::parse("OPUS"), Some(AgentModel::Opus));
        assert_eq!(AgentModel::parse(" sonnet "), Some(AgentModel::Sonnet));
        assert_eq!(AgentModel::parse("gpt"), None);
    }

    #[test]
    fn model_cycle_visits_every_curated_variant() {
        let mut seen = vec![AgentModel::Opus];
        let mut cur = AgentModel::Opus;
        for _ in 0..AgentModel::all().len() - 1 {
            cur = cur.next();
            seen.push(cur.clone());
        }
        assert_eq!(cur.next(), AgentModel::Opus); // wraps
        assert_eq!(seen.len(), AgentModel::all().len());
    }

    #[test]
    fn model_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&AgentModel::Sonnet).unwrap(),
            "\"sonnet\""
        );
        let custom = AgentModel::custom(" claude-future ").unwrap();
        assert_eq!(custom.id(), "claude-future");
        assert_eq!(custom.label(), "claude-future");
        assert_eq!(serde_json::to_string(&custom).unwrap(), "\"claude-future\"");
        assert!(AgentModel::custom(" ").is_none());
    }

    #[test]
    fn effort_parse_accepts_aliases() {
        assert_eq!(Effort::parse("low"), Some(Effort::Low));
        assert_eq!(Effort::parse("MED"), Some(Effort::Medium));
        assert_eq!(Effort::parse("medium"), Some(Effort::Medium));
        assert_eq!(Effort::parse("High"), Some(Effort::High));
        assert_eq!(Effort::parse("x-high"), Some(Effort::XHigh));
        assert_eq!(Effort::parse("max"), Some(Effort::Max));
    }

    #[test]
    fn effort_directive_only_for_non_baseline() {
        assert!(Effort::Low.directive().is_some());
        assert!(Effort::Medium.directive().is_none());
        assert!(Effort::High.directive().is_some());
    }

    #[test]
    fn effort_cycle_wraps() {
        assert_eq!(Effort::Low.next(), Effort::Medium);
        assert_eq!(Effort::Medium.next(), Effort::High);
        assert_eq!(Effort::High.next(), Effort::XHigh);
        assert_eq!(Effort::XHigh.next(), Effort::Max);
        assert_eq!(Effort::Max.next(), Effort::Low);
    }

    #[test]
    fn prev_is_the_inverse_of_next() {
        for m in AgentModel::all() {
            assert_eq!(m.next().prev(), m.clone());
            assert_eq!(m.prev().next(), m.clone());
        }
        for &e in Effort::all() {
            assert_eq!(e.next().prev(), e);
            assert_eq!(e.prev().next(), e);
        }
    }

    #[test]
    fn defaults_are_haiku_and_low() {
        let opts = AgentOptions::default();
        assert_eq!(opts.model, AgentModel::Haiku);
        assert_eq!(opts.effort, Effort::Low);
    }
}
