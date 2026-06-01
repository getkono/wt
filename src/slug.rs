//! Branch-slug normalization (spec §3).
//!
//! A slug is a filesystem-safe rendering of a branch name, used only for
//! directory names; the real branch name is always preserved in Git. The rules:
//! (1) replace `/` and `\` with `-`; (2) replace any run of characters outside
//! `[a-zA-Z0-9.-]` with `-`; (3) collapse consecutive `-` into one; (4) strip
//! leading/trailing `-`; (5) if the result is empty, fall back to the short
//! commit hash of the base ref (supplied by the caller via
//! [`slugify_with_fallback`]).

/// Normalizes `branch` into a slug, applying rules 1–4. May return an empty
/// string (e.g. for a branch consisting only of separators); use
/// [`slugify_with_fallback`] to apply rule 5.
pub fn slugify(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len());
    let mut prev_dash = false;
    for ch in branch.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' {
            // Rule 2: kept characters (alphanumeric and `.`) pass through.
            out.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            // Rules 1–3: `/`, `\`, a literal `-`, and any other disallowed
            // character all become a dash, and consecutive dashes collapse.
            out.push('-');
            prev_dash = true;
        }
    }
    // Rule 4: strip leading/trailing dashes.
    out.trim_matches('-').to_string()
}

/// Like [`slugify`], but applies rule 5: when the normalized slug is empty,
/// return `fallback` (the short commit hash of the base ref).
pub fn slugify_with_fallback(branch: &str, fallback: &str) -> String {
    let slug = slugify(branch);
    if slug.is_empty() {
        fallback.to_string()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_slashes_and_backslashes() {
        assert_eq!(slugify("feature/login"), "feature-login");
        assert_eq!(slugify("a\\b"), "a-b");
        assert_eq!(slugify("a/b/c"), "a-b-c");
    }

    #[test]
    fn replaces_disallowed_runs_with_single_dash() {
        assert_eq!(slugify("feat@#login"), "feat-login");
        assert_eq!(slugify("hello world"), "hello-world");
        assert_eq!(slugify("a   b"), "a-b");
    }

    #[test]
    fn keeps_dots_and_digits_and_case() {
        assert_eq!(slugify("v1.2.3"), "v1.2.3");
        assert_eq!(slugify("Feature-XYZ"), "Feature-XYZ");
    }

    #[test]
    fn collapses_consecutive_dashes() {
        assert_eq!(slugify("a--b"), "a-b");
        assert_eq!(slugify("a//b"), "a-b");
        assert_eq!(slugify("a-/-b"), "a-b");
    }

    #[test]
    fn strips_leading_and_trailing_dashes() {
        assert_eq!(slugify("/feature/"), "feature");
        assert_eq!(slugify("---x---"), "x");
        assert_eq!(slugify("@@@edge@@@"), "edge");
    }

    #[test]
    fn non_ascii_becomes_dashes() {
        assert_eq!(slugify("café"), "caf");
        assert_eq!(slugify("中文branch"), "branch");
    }

    #[test]
    fn empty_result_uses_fallback() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("///"), "");
        assert_eq!(slugify("@@@"), "");
        assert_eq!(slugify_with_fallback("///", "abc1234"), "abc1234");
        assert_eq!(slugify_with_fallback("feature/x", "abc1234"), "feature-x");
    }
}
