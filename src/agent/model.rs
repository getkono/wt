//! Model and effort selection for code-agent runs (the AI PR auto-fill path).
//!
//! [`AgentModel`] is a small, curated set of selectable model tiers; [`Effort`]
//! is how hard the agent should work; [`AgentOptions`] bundles the two for one
//! run. All three are pure data with `parse`/`id`/`label`/`next` helpers so they
//! drive the config layer, the CLI flags, and the TUI's live cycle keys without
//! any process or I/O.

use serde::Serialize;

/// A selectable model tier for a code agent. The variants currently encode the
/// Claude tiers (the only supported agent); the `id` doubles as the CLI
/// `--model` value — a stable alias that resolves to the latest model of that
/// tier — so labels can track the current family without breaking selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentModel {
    /// Most capable, highest latency (Claude Opus).
    Opus,
    /// Balanced capability and speed (Claude Sonnet) — the default.
    #[default]
    Sonnet,
    /// Fastest and lightest (Claude Haiku).
    Haiku,
}

impl AgentModel {
    /// Every selectable model, in display and cycle order.
    pub fn all() -> &'static [AgentModel] {
        &[AgentModel::Opus, AgentModel::Sonnet, AgentModel::Haiku]
    }

    /// The stable lowercase identifier, used both in config/flags and as the
    /// agent CLI's `--model` value (e.g. `"sonnet"`).
    pub fn id(self) -> &'static str {
        match self {
            AgentModel::Opus => "opus",
            AgentModel::Sonnet => "sonnet",
            AgentModel::Haiku => "haiku",
        }
    }

    /// A human-readable label for the status display; tracks the current model
    /// family (the `id` alias always selects the latest of that tier).
    pub fn label(self) -> &'static str {
        match self {
            AgentModel::Opus => "Opus 4.8",
            AgentModel::Sonnet => "Sonnet 4.6",
            AgentModel::Haiku => "Haiku 4.5",
        }
    }

    /// Parses a model identifier (case-insensitive: `opus`/`sonnet`/`haiku`),
    /// returning `None` if unknown.
    pub fn parse(s: &str) -> Option<AgentModel> {
        match s.trim().to_ascii_lowercase().as_str() {
            "opus" => Some(AgentModel::Opus),
            "sonnet" => Some(AgentModel::Sonnet),
            "haiku" => Some(AgentModel::Haiku),
            _ => None,
        }
    }

    /// The next model in cycle order (wraps), for the TUI's `Ctrl-M` picker.
    pub fn next(self) -> AgentModel {
        match self {
            AgentModel::Opus => AgentModel::Sonnet,
            AgentModel::Sonnet => AgentModel::Haiku,
            AgentModel::Haiku => AgentModel::Opus,
        }
    }
}

/// How much effort the agent should spend on a draft. Claude has no native
/// headless effort flag, so `wt` conveys effort as a one-line directive
/// prepended to the prompt (see [`Effort::directive`]) — a safe, never-failing
/// lever that shapes the model's deliberation and can map to native reasoning
/// controls per agent in the future.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Quick, minimal deliberation.
    Low,
    /// Balanced effort — the default (no directive).
    #[default]
    Medium,
    /// Maximum deliberation and care.
    High,
}

impl Effort {
    /// Every effort level, in display and cycle order.
    pub fn all() -> &'static [Effort] {
        &[Effort::Low, Effort::Medium, Effort::High]
    }

    /// The stable lowercase identifier, used in config and `--effort`.
    pub fn id(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }

    /// A human-readable label (currently identical to [`Effort::id`]).
    pub fn label(self) -> &'static str {
        self.id()
    }

    /// Parses an effort identifier (case-insensitive: `low`, `medium`/`med`,
    /// `high`), returning `None` if unknown.
    pub fn parse(s: &str) -> Option<Effort> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Effort::Low),
            "medium" | "med" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            _ => None,
        }
    }

    /// The next effort level in cycle order (wraps), for the TUI's `Ctrl-E` key.
    pub fn next(self) -> Effort {
        match self {
            Effort::Low => Effort::Medium,
            Effort::Medium => Effort::High,
            Effort::High => Effort::Low,
        }
    }

    /// A one-line instruction conveying this effort to the agent, prepended to
    /// the prompt; `None` for the balanced baseline (medium).
    pub fn directive(self) -> Option<&'static str> {
        match self {
            Effort::Low => Some("Work quickly and keep your reasoning brief."),
            Effort::Medium => None,
            Effort::High => Some("Think carefully and review the diff thoroughly before writing."),
        }
    }
}

/// The model and effort selected for a single agent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AgentOptions {
    /// The model tier to drive.
    pub model: AgentModel,
    /// How much effort to spend.
    pub effort: Effort,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_parse_roundtrips_and_rejects_unknown() {
        for &m in AgentModel::all() {
            assert_eq!(AgentModel::parse(m.id()), Some(m));
        }
        assert_eq!(AgentModel::parse("OPUS"), Some(AgentModel::Opus));
        assert_eq!(AgentModel::parse(" sonnet "), Some(AgentModel::Sonnet));
        assert_eq!(AgentModel::parse("gpt"), None);
    }

    #[test]
    fn model_cycle_visits_every_variant() {
        let mut seen = vec![AgentModel::Opus];
        let mut cur = AgentModel::Opus;
        for _ in 0..AgentModel::all().len() - 1 {
            cur = cur.next();
            seen.push(cur);
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
    }

    #[test]
    fn effort_parse_accepts_aliases() {
        assert_eq!(Effort::parse("low"), Some(Effort::Low));
        assert_eq!(Effort::parse("MED"), Some(Effort::Medium));
        assert_eq!(Effort::parse("medium"), Some(Effort::Medium));
        assert_eq!(Effort::parse("High"), Some(Effort::High));
        assert_eq!(Effort::parse("max"), None);
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
        assert_eq!(Effort::High.next(), Effort::Low);
    }

    #[test]
    fn defaults_are_sonnet_and_medium() {
        let opts = AgentOptions::default();
        assert_eq!(opts.model, AgentModel::Sonnet);
        assert_eq!(opts.effort, Effort::Medium);
    }
}
