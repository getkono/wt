//! Query resolution (spec §7): match a query against a set of worktrees in a
//! defined precedence order, reporting a unique match, ambiguity, or no match.

use crate::model::Worktree;

/// The outcome of resolving a query against a worktree set. Indices refer into
/// the slice passed to [`resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Exactly one worktree matched (its index).
    One(usize),
    /// Several worktrees matched (their indices); the query is ambiguous.
    Ambiguous(Vec<usize>),
    /// No worktree matched.
    NotFound,
}

/// The directory name of a worktree (the last path component).
fn dir_name(worktree: &Worktree) -> Option<String> {
    worktree
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// Resolves `query` against `worktrees` (spec §7): exact branch, then exact
/// slug, then exact directory name; then an unambiguous prefix across all three.
/// The first tier with any match wins; if that tier has more than one match the
/// query is ambiguous.
pub fn resolve(worktrees: &[Worktree], query: &str) -> Resolved {
    let exact = |pick: &dyn Fn(&Worktree) -> Option<String>| -> Vec<usize> {
        worktrees
            .iter()
            .enumerate()
            .filter(|(_, w)| pick(w).as_deref() == Some(query))
            .map(|(i, _)| i)
            .collect()
    };

    for matches in [
        exact(&|w| w.branch.clone()),
        exact(&|w| w.slug.clone()),
        exact(&dir_name),
    ] {
        if !matches.is_empty() {
            return decide(matches);
        }
    }

    // Tier 2: unambiguous prefix across branch / slug / directory name.
    let starts = |value: Option<String>| value.as_deref().is_some_and(|v| v.starts_with(query));
    let prefix: Vec<usize> = worktrees
        .iter()
        .enumerate()
        .filter(|(_, w)| starts(w.branch.clone()) || starts(w.slug.clone()) || starts(dir_name(w)))
        .map(|(i, _)| i)
        .collect();
    if prefix.is_empty() {
        Resolved::NotFound
    } else {
        decide(prefix)
    }
}

/// Maps a non-empty match list to [`Resolved::One`] or [`Resolved::Ambiguous`].
fn decide(matches: Vec<usize>) -> Resolved {
    if matches.len() == 1 {
        Resolved::One(matches[0])
    } else {
        Resolved::Ambiguous(matches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn wt(path: &str, branch: Option<&str>, slug: Option<&str>) -> Worktree {
        let mut w = Worktree::new(PathBuf::from(path));
        w.branch = branch.map(str::to_string);
        w.slug = slug.map(str::to_string);
        w
    }

    fn set() -> Vec<Worktree> {
        vec![
            wt("/r/main", Some("main"), Some("main")),
            wt(
                "/r/feature-login",
                Some("feature/login"),
                Some("feature-login"),
            ),
            wt(
                "/r/feature-logout",
                Some("feature/logout"),
                Some("feature-logout"),
            ),
            wt("/r/detached", None, None),
        ]
    }

    #[test]
    fn exact_branch_wins() {
        assert_eq!(resolve(&set(), "feature/login"), Resolved::One(1));
        assert_eq!(resolve(&set(), "main"), Resolved::One(0));
    }

    #[test]
    fn exact_slug_matches() {
        assert_eq!(resolve(&set(), "feature-logout"), Resolved::One(2));
    }

    #[test]
    fn exact_dir_name_matches() {
        assert_eq!(resolve(&set(), "detached"), Resolved::One(3));
    }

    #[test]
    fn unambiguous_prefix_matches() {
        // "feature/log" is a prefix of both feature/login and feature/logout.
        assert_eq!(resolve(&set(), "feature/login"), Resolved::One(1));
        // "feature-login" exact-slug beats prefix.
        assert_eq!(resolve(&set(), "feature-login"), Resolved::One(1));
        // A prefix that hits only one.
        assert_eq!(resolve(&set(), "feature/logi"), Resolved::One(1));
    }

    #[test]
    fn ambiguous_prefix_lists_candidates() {
        match resolve(&set(), "feature/log") {
            Resolved::Ambiguous(ix) => assert_eq!(ix, vec![1, 2]),
            other => panic!("expected ambiguous, got {other:?}"),
        }
        // The slug prefix "feature-log" is also ambiguous.
        match resolve(&set(), "feature-log") {
            Resolved::Ambiguous(ix) => assert_eq!(ix, vec![1, 2]),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn no_match_is_not_found() {
        assert_eq!(resolve(&set(), "nonexistent"), Resolved::NotFound);
    }

    #[test]
    fn exact_tier_does_not_fall_through_to_prefix() {
        // Two worktrees share an exact slug -> ambiguous at the exact tier,
        // never reaching prefix matching.
        let worktrees = vec![
            wt("/r/a", Some("topic-a"), Some("dup")),
            wt("/r/b", Some("topic-b"), Some("dup")),
        ];
        match resolve(&worktrees, "dup") {
            Resolved::Ambiguous(ix) => assert_eq!(ix, vec![0, 1]),
            other => panic!("expected ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn prefix_matches_on_slug_alone() {
        // A worktree whose slug — but neither its branch nor its directory name —
        // prefix-matches the query must still resolve. The three prefix checks
        // are independent alternatives, not a conjunction.
        let worktrees = vec![
            wt("/r/main", Some("main"), Some("main")),
            wt("/r/alpha", Some("alpha"), Some("zztop")),
        ];
        assert_eq!(resolve(&worktrees, "zz"), Resolved::One(1));
    }
}
