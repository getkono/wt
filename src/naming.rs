//! The branch-name contract for issue-driven worktrees (issue #96): the
//! conventional `TYPE/{number}-SLUG` form, its prompt fragment, its validator,
//! and a deterministic fallback.
//!
//! Everything here is pure — no I/O, no [`Cx`](crate::cx::Cx), no subprocesses —
//! so the same contract can be exercised by `wt`'s own issue flow and by
//! embedders (karet) without an agent or a network. The prompt fragment and the
//! validator live side by side so the rule a model is asked to follow and the
//! rule its output is checked against cannot drift.
//!
//! This module names **branches**. [`crate::slug`] is a different concept — it
//! normalizes a branch name into a filesystem-safe *directory* name.

use std::fmt;

use crate::error::{Error, Result};

/// The nine conventional branch types accepted in `TYPE/{number}-SLUG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BranchKind {
    /// A new feature (`feat`).
    Feat,
    /// A bug fix (`fix`).
    Fix,
    /// Documentation only (`docs`).
    Docs,
    /// A refactor with no behavior change (`refactor`).
    Refactor,
    /// Test-only changes (`test`).
    Test,
    /// Build system or dependency changes (`build`).
    Build,
    /// CI configuration changes (`ci`).
    Ci,
    /// A performance improvement (`perf`).
    Perf,
    /// Maintenance that fits no other type (`chore`).
    Chore,
}

impl BranchKind {
    /// Every kind, in the order the contract lists them.
    pub const ALL: [BranchKind; 9] = [
        BranchKind::Feat,
        BranchKind::Fix,
        BranchKind::Docs,
        BranchKind::Refactor,
        BranchKind::Test,
        BranchKind::Build,
        BranchKind::Ci,
        BranchKind::Perf,
        BranchKind::Chore,
    ];

    /// The lowercase identifier used in branch names (e.g. `"feat"`).
    pub fn as_str(self) -> &'static str {
        match self {
            BranchKind::Feat => "feat",
            BranchKind::Fix => "fix",
            BranchKind::Docs => "docs",
            BranchKind::Refactor => "refactor",
            BranchKind::Test => "test",
            BranchKind::Build => "build",
            BranchKind::Ci => "ci",
            BranchKind::Perf => "perf",
            BranchKind::Chore => "chore",
        }
    }

    /// Parses an exact kind identifier (`"feat"`, `"fix"`, …).
    pub fn parse(text: &str) -> Option<BranchKind> {
        BranchKind::ALL.into_iter().find(|k| k.as_str() == text)
    }

    /// Maps a GitHub issue label or issue-type name to a kind, case-insensitively:
    /// the exact kind identifiers plus the common defaults (`bug` → `fix`,
    /// `enhancement`/`feature` → `feat`, `documentation` → `docs`,
    /// `performance` → `perf`, `tests` → `test`). `None` for anything else, so a
    /// caller can fall through to its own default (issue #98 uses `feat`).
    pub fn from_label(label: &str) -> Option<BranchKind> {
        let lower = label.to_ascii_lowercase();
        if let Some(kind) = BranchKind::parse(&lower) {
            return Some(kind);
        }
        match lower.as_str() {
            "bug" => Some(BranchKind::Fix),
            "enhancement" | "feature" => Some(BranchKind::Feat),
            "documentation" => Some(BranchKind::Docs),
            "performance" => Some(BranchKind::Perf),
            "tests" => Some(BranchKind::Test),
            _ => None,
        }
    }
}

impl fmt::Display for BranchKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A validated `TYPE/{number}-SLUG` branch name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchName {
    kind: BranchKind,
    number: u64,
    slug: String,
}

impl BranchName {
    /// The conventional type prefix.
    pub fn kind(&self) -> BranchKind {
        self.kind
    }

    /// The issue number embedded in the name.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// The lowercase kebab-case slug after the issue number.
    pub fn slug(&self) -> &str {
        &self.slug
    }
}

impl fmt::Display for BranchName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}-{}", self.kind, self.number, self.slug)
    }
}

/// The comma-separated list of kind identifiers, for prompts and errors.
fn kind_list() -> String {
    BranchKind::ALL
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The prompt fragment describing the branch contract for `issue_number`, used
/// verbatim in generation requests so the rule a model is asked to follow is the
/// same rule [`parse_and_validate`] enforces.
pub fn branch_rule(issue_number: u64) -> String {
    format!(
        "Choose a branch in the exact form TYPE/{issue_number}-SLUG. TYPE must be one of {}. SLUG must be lowercase kebab-case.",
        kind_list()
    )
}

/// Whether `slug` is non-empty lowercase kebab-case: `[a-z0-9-]` only, with no
/// leading, trailing, or doubled `-`.
fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Validates a generated branch name against the `TYPE/{issue_number}-SLUG`
/// contract: a legal git branch name, a single `/` separating a known
/// [`BranchKind`], an `{issue_number}-` prefix, and a non-empty lowercase
/// kebab-case slug. Returns [`Error::Usage`] naming the violated rule.
pub fn parse_and_validate(generated: &str, issue_number: u64) -> Result<BranchName> {
    crate::git::validate_branch_name(generated).map_err(Error::usage)?;
    let (kind, suffix) = generated
        .split_once('/')
        .ok_or_else(|| Error::usage("generated branch must have a conventional type prefix"))?;
    let kind = BranchKind::parse(kind)
        .ok_or_else(|| Error::usage(format!("generated branch type {kind:?} is not supported")))?;
    let prefix = format!("{issue_number}-");
    let slug = suffix
        .strip_prefix(&prefix)
        .ok_or_else(|| Error::usage(format!("generated branch must contain {prefix:?}")))?;
    if !is_valid_slug(slug) {
        return Err(Error::usage(
            "generated branch slug must be non-empty lowercase kebab-case",
        ));
    }
    Ok(BranchName {
        kind,
        number: issue_number,
        slug: slug.to_string(),
    })
}

/// Builds the deterministic fallback branch name for an issue: `kind`, the issue
/// number, and the title reduced to a lowercase kebab-case slug via
/// [`crate::slug::slugify`] (`"issue"` when the title yields nothing usable).
/// The result always satisfies [`parse_and_validate`], so worktree creation can
/// proceed no matter what a generation step produced (issue #98).
pub fn fallback(kind: BranchKind, issue_number: u64, title: &str) -> BranchName {
    // `slugify` keeps case and dots (both fine for directories, both illegal
    // here), so lowercase first and fold dots into dashes after.
    let mut slug = crate::slug::slugify(&title.to_lowercase()).replace('.', "-");
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "issue" } else { slug };
    BranchName {
        kind,
        number: issue_number,
        slug: slug.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_parse_display_round_trip() {
        for kind in BranchKind::ALL {
            assert_eq!(BranchKind::parse(kind.as_str()), Some(kind));
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(BranchKind::parse("feature"), None);
        assert_eq!(BranchKind::parse("FEAT"), None);
        assert_eq!(BranchKind::parse(""), None);
    }

    #[test]
    fn kind_from_label_maps_common_hints() {
        assert_eq!(BranchKind::from_label("bug"), Some(BranchKind::Fix));
        assert_eq!(BranchKind::from_label("Bug"), Some(BranchKind::Fix));
        assert_eq!(
            BranchKind::from_label("enhancement"),
            Some(BranchKind::Feat)
        );
        assert_eq!(BranchKind::from_label("Feature"), Some(BranchKind::Feat));
        assert_eq!(
            BranchKind::from_label("documentation"),
            Some(BranchKind::Docs)
        );
        assert_eq!(
            BranchKind::from_label("performance"),
            Some(BranchKind::Perf)
        );
        assert_eq!(BranchKind::from_label("tests"), Some(BranchKind::Test));
        // Exact kind identifiers pass through.
        assert_eq!(BranchKind::from_label("chore"), Some(BranchKind::Chore));
        // Unknown labels leave the choice to the caller.
        assert_eq!(BranchKind::from_label("help wanted"), None);
    }

    #[test]
    fn branch_rule_names_every_kind_and_the_number() {
        let rule = branch_rule(42);
        assert!(rule.contains("TYPE/42-SLUG"));
        for kind in BranchKind::ALL {
            assert!(rule.contains(kind.as_str()), "missing {kind}");
        }
        assert!(rule.contains("lowercase kebab-case"));
    }

    #[test]
    fn valid_branch_parses_into_parts() {
        let name = parse_and_validate("feat/12-add-login", 12).unwrap();
        assert_eq!(name.kind(), BranchKind::Feat);
        assert_eq!(name.number(), 12);
        assert_eq!(name.slug(), "add-login");
        assert_eq!(name.to_string(), "feat/12-add-login");
    }

    #[test]
    fn every_kind_is_accepted() {
        for kind in BranchKind::ALL {
            let text = format!("{kind}/7-x");
            assert_eq!(parse_and_validate(&text, 7).unwrap().kind(), kind);
        }
    }

    #[test]
    fn digits_and_single_dashes_are_a_valid_slug() {
        let name = parse_and_validate("fix/3-v2-api-404s", 3).unwrap();
        assert_eq!(name.slug(), "v2-api-404s");
    }

    /// The full rejection matrix: each case names the violated rule.
    #[test]
    fn invalid_branches_are_rejected_with_the_rule() {
        let cases: &[(&str, u64, &str)] = &[
            // Not a legal git branch name at all.
            ("feat/12-a..b", 12, "invalid branch name"),
            ("feat/12-a b", 12, "invalid branch name"),
            ("feat/12-x.lock", 12, "invalid branch name"),
            // No TYPE/ prefix.
            ("12-add-login", 12, "conventional type prefix"),
            // Unknown TYPE.
            ("feature/12-add-login", 12, "not supported"),
            ("FEAT/12-add-login", 12, "not supported"),
            // A second `/` survives into the slug and fails the charset rule.
            ("feat/12-a/b", 12, "kebab-case"),
            // Wrong or missing issue number.
            ("feat/13-add-login", 12, "\"12-\""),
            ("feat/add-login", 12, "\"12-\""),
            // Slug violations: empty, uppercase, underscore, edge/double dash.
            ("feat/12-", 12, "kebab-case"),
            ("feat/12-Add-Login", 12, "kebab-case"),
            ("feat/12-add_login", 12, "kebab-case"),
            ("feat/12-add--login", 12, "kebab-case"),
            ("feat/12-add-login-", 12, "kebab-case"),
        ];
        for (text, number, expect) in cases {
            let err = parse_and_validate(text, *number).unwrap_err();
            assert!(matches!(err, Error::Usage(_)), "{text}: {err:?}");
            assert!(
                err.to_string().contains(expect),
                "{text}: {err} (expected {expect:?})"
            );
        }
    }

    #[test]
    fn leading_dash_slug_is_rejected() {
        // `feat/12--x` reads as prefix `12-` + slug `-x`; git allows the name,
        // so it must fall to the kebab-case rule.
        let err = parse_and_validate("feat/12--x", 12).unwrap_err();
        assert!(err.to_string().contains("kebab-case"));
    }

    #[test]
    fn fallback_is_deterministic_and_normalizes_titles() {
        let name = fallback(BranchKind::Fix, 482, "NULL owner crashes v1.2 API!");
        assert_eq!(name.to_string(), "fix/482-null-owner-crashes-v1-2-api");
        // Same inputs, same output.
        assert_eq!(
            fallback(BranchKind::Fix, 482, "NULL owner crashes v1.2 API!"),
            name
        );
    }

    #[test]
    fn fallback_survives_hostile_titles() {
        // Nothing slug-worthy at all.
        assert_eq!(
            fallback(BranchKind::Feat, 9, "!!!").to_string(),
            "feat/9-issue"
        );
        assert_eq!(
            fallback(BranchKind::Feat, 9, "").to_string(),
            "feat/9-issue"
        );
        // Dots next to separators must not leave doubled or edge dashes.
        assert_eq!(
            fallback(BranchKind::Chore, 1, "v1.2 . rollout...").to_string(),
            "chore/1-v1-2-rollout"
        );
        // Non-ASCII drops out; case folds.
        assert_eq!(
            fallback(BranchKind::Docs, 5, "Café MENU docs").to_string(),
            "docs/5-caf-menu-docs"
        );
    }

    #[test]
    fn fallback_always_satisfies_the_validator() {
        let titles = [
            "Add login",
            "NULL owner crashes v1.2 API!",
            "",
            "!!!",
            "---",
            "v1.2.3",
            ".hidden",
            "UPPER_case and  spaces",
            "中文 only",
            "trailing dot.",
            "a",
        ];
        for title in titles {
            for kind in BranchKind::ALL {
                let name = fallback(kind, 123, title);
                let text = name.to_string();
                let parsed = parse_and_validate(&text, 123)
                    .unwrap_or_else(|e| panic!("{text:?} from {title:?}: {e}"));
                assert_eq!(parsed, name);
            }
        }
    }
}
