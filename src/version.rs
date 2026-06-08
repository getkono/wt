//! Build and version metadata surfaced by `wt --version` / `wt -V`.
//!
//! The plain semver alone is rarely enough to diagnose a report, so the version
//! output also carries the build commit, profile, toolchain, and timestamp
//! (issue #22). The raw facts are captured at compile time by `build.rs` and
//! exposed here as constants; [`long_version`] assembles them into the
//! multi-line string handed to `clap`.

/// Crate semver from `Cargo.toml`.
pub const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short Git commit hash at build time (with a `-dirty` suffix when the working
/// tree had uncommitted changes), or `"unknown"` when git was unavailable.
pub const COMMIT_HASH: &str = env!("WT_COMMIT_HASH");

/// ISO-8601 committer date of the build commit, or `"unknown"`.
pub const COMMIT_DATE: &str = env!("WT_COMMIT_DATE");

/// Cargo build profile the binary was compiled with (e.g. `debug`, `release`).
pub const BUILD_PROFILE: &str = env!("WT_BUILD_PROFILE");

/// `rustc` version used to compile the binary, or `"unknown"`.
pub const RUSTC_VERSION: &str = env!("WT_RUSTC_VERSION");

/// UTC build timestamp (RFC-3339), or `"unknown"`.
pub const BUILD_TIMESTAMP: &str = env!("WT_BUILD_TIMESTAMP");

/// Returns the rich, multi-line version string shown by `wt --version` and
/// `wt -V`. `clap` prefixes the first line with the binary name (`wt`).
///
/// The string is built once and cached so it can be handed to `clap` as a
/// `&'static str`.
pub fn long_version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION
        .get_or_init(|| {
            format_version(
                PKG_VERSION,
                COMMIT_HASH,
                COMMIT_DATE,
                BUILD_PROFILE,
                RUSTC_VERSION,
                BUILD_TIMESTAMP,
            )
        })
        .as_str()
}

/// Assembles the multi-line version body from individual build facts. Kept
/// separate from [`long_version`] so the layout is unit-testable without
/// depending on the values baked in at compile time.
fn format_version(
    version: &str,
    commit: &str,
    commit_date: &str,
    profile: &str,
    rustc: &str,
    built: &str,
) -> String {
    format!(
        "{version}\n\
         commit:  {commit} ({commit_date})\n\
         profile: {profile}\n\
         rustc:   {rustc}\n\
         built:   {built}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_version_lays_out_all_facts() {
        let out = format_version(
            "1.2.3",
            "abc123",
            "2026-01-01T00:00:00Z",
            "release",
            "rustc 1.96.0",
            "2026-06-08T12:00:00Z",
        );
        assert_eq!(
            out,
            "1.2.3\n\
             commit:  abc123 (2026-01-01T00:00:00Z)\n\
             profile: release\n\
             rustc:   rustc 1.96.0\n\
             built:   2026-06-08T12:00:00Z"
        );
    }

    #[test]
    fn long_version_starts_with_semver_and_includes_build_facts() {
        let out = long_version();
        assert!(out.starts_with(PKG_VERSION));
        assert!(out.contains("commit:"));
        assert!(out.contains("profile:"));
        assert!(out.contains("rustc:"));
        assert!(out.contains("built:"));
    }

    #[test]
    fn constants_are_populated() {
        // build.rs always emits a value (real or the "unknown" fallback).
        assert!(!COMMIT_HASH.is_empty());
        assert!(!COMMIT_DATE.is_empty());
        assert!(!BUILD_PROFILE.is_empty());
        assert!(!RUSTC_VERSION.is_empty());
        assert!(!BUILD_TIMESTAMP.is_empty());
    }
}
