//! Build script: captures build-time metadata (commit, profile, toolchain,
//! timestamp) and exposes it to the crate as `WT_*` compile-time env vars,
//! consumed by `src/version.rs` for `wt --version` (issue #22).
//!
//! Every fact is best-effort: when git or `rustc` cannot be queried the value
//! falls back to `"unknown"` so the build never fails in a source tarball or a
//! checkout without a `.git` directory.

use std::process::Command;

fn main() {
    // A CI-provided commit hash takes precedence over the local `git` probe: the
    // cross container that builds the Linux release binaries has no `git`, so the
    // release workflow exports `WT_BUILD_SHA` (where git *is* available) and
    // forwards it into the container via Cross.toml.
    let commit = env_override("WT_BUILD_SHA").or_else(|| {
        git(&["rev-parse", "--short=12", "HEAD"]).map(|sha| {
            if is_dirty() {
                format!("{sha}-dirty")
            } else {
                sha
            }
        })
    });
    let commit_date = git(&["log", "-1", "--format=%cI"]);

    emit("WT_COMMIT_HASH", commit.as_deref().unwrap_or("unknown"));
    emit(
        "WT_COMMIT_DATE",
        commit_date.as_deref().unwrap_or("unknown"),
    );
    emit("WT_BUILD_PROFILE", &env_or("PROFILE", "unknown"));
    emit("WT_RUSTC_VERSION", &rustc_version());
    emit("WT_BUILD_TIMESTAMP", &build_timestamp());

    // Rebuild when HEAD moves so the embedded commit stays current. In a git
    // worktree `.git` is a file, so resolve the real git dir rather than
    // hard-coding `.git/HEAD`.
    if let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo::rerun-if-changed={git_dir}/HEAD");
    }
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo::rerun-if-env-changed=WT_BUILD_SHA");
}

/// Reads an environment variable, returning its trimmed value only when set and
/// non-empty. Used to let CI override otherwise git-probed build facts.
fn env_override(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Emits a `WT_*` value as a compile-time environment variable.
fn emit(key: &str, value: &str) {
    println!("cargo::rustc-env={key}={value}");
}

/// Runs `git` with `args`, returning trimmed stdout on a clean exit.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Reports whether the working tree has uncommitted changes.
fn is_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

/// Resolves the `rustc` version string from the compiler Cargo selected.
fn rustc_version() -> String {
    let rustc = env_or("RUSTC", "rustc");
    Command::new(rustc)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// UTC build time as RFC-3339, honoring `SOURCE_DATE_EPOCH` for reproducible
/// builds and falling back to the current instant otherwise.
fn build_timestamp() -> String {
    let ts = match std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(epoch) => jiff::Timestamp::from_second(epoch).ok(),
        None => Some(jiff::Timestamp::now()),
    };
    ts.map(|t| t.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Reads an environment variable, falling back to `default` when unset.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
