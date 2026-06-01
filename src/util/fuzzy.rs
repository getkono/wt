//! Fuzzy filtering for `wt list --filter` and the TUI `/` filter (spec §7/§10),
//! backed by `nucleo-matcher`.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Returns the indices of `haystacks` that fuzzy-match `query`, ordered by
/// descending score (ties keep input order). An empty query matches everything
/// in input order.
pub fn filter_indices(haystacks: &[String], query: &str) -> Vec<usize> {
    if query.trim().is_empty() {
        return (0..haystacks.len()).collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let mut scored: Vec<(usize, u32)> = Vec::new();
    let mut buf = Vec::new();
    for (index, haystack) in haystacks.iter().enumerate() {
        let utf32 = Utf32Str::new(haystack, &mut buf);
        if let Some(score) = pattern.score(utf32, &mut matcher) {
            scored.push((index, score));
        }
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.into_iter().map(|(index, _)| index).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn haystacks() -> Vec<String> {
        vec![
            "feature/login".to_string(),
            "feature/logout".to_string(),
            "main".to_string(),
            "hotfix/crash".to_string(),
        ]
    }

    #[test]
    fn empty_query_returns_all_in_order() {
        assert_eq!(filter_indices(&haystacks(), ""), vec![0, 1, 2, 3]);
        assert_eq!(filter_indices(&haystacks(), "   "), vec![0, 1, 2, 3]);
    }

    #[test]
    fn matches_subsequence() {
        // "flog" fuzzily matches the feature/log* entries.
        let result = filter_indices(&haystacks(), "flog");
        assert!(result.contains(&0));
        assert!(result.contains(&1));
        assert!(!result.contains(&2));
    }

    #[test]
    fn exact_substring_matches() {
        let result = filter_indices(&haystacks(), "main");
        assert_eq!(result, vec![2]);
    }

    #[test]
    fn no_match_is_empty() {
        assert!(filter_indices(&haystacks(), "zzzzz").is_empty());
    }

    #[test]
    fn ranks_better_matches_first() {
        // "hotfix" should rank the hotfix entry first.
        let result = filter_indices(&haystacks(), "hotfix");
        assert_eq!(result.first(), Some(&3));
    }
}
