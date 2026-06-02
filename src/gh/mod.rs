//! The GitHub boundary (spec §4): all pull-request operations shell out to the
//! `gh` CLI. [`GhClient`] isolates this so tests can inject a fake; [`RealGh`]
//! spawns the real binary. A missing or unauthenticated `gh` yields
//! [`Error::GhUnavailable`] with an actionable message (§12).

pub mod types;

use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};
pub use types::{Author, PrSummary, PrView, pr_state};

/// Performs GitHub pull-request operations via `gh`.
pub trait GhClient {
    /// Lists open PRs for the repository at `dir`.
    fn list_open_prs(&self, dir: &Path) -> Result<Vec<PrSummary>>;

    /// Views the PR identified by `target` (a number, URL, or head branch).
    fn view_pr(&self, dir: &Path, target: &str) -> Result<PrView>;

    /// Lists open PR numbers (for completion; best-effort).
    fn open_pr_numbers(&self, dir: &Path) -> Result<Vec<u64>> {
        Ok(self
            .list_open_prs(dir)?
            .into_iter()
            .map(|p| p.number)
            .collect())
    }
}

/// The production [`GhClient`] that spawns the real `gh` binary.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealGh;

impl GhClient for RealGh {
    fn list_open_prs(&self, dir: &Path) -> Result<Vec<PrSummary>> {
        let output = run_gh(
            dir,
            &[
                "pr",
                "list",
                "--state",
                "open",
                "--json",
                "number,title,author,state,isDraft,headRefName,createdAt",
            ],
        )?;
        serde_json::from_str(&output).map_err(Error::from)
    }

    fn view_pr(&self, dir: &Path, target: &str) -> Result<PrView> {
        let output = run_gh(
            dir,
            &[
                "pr",
                "view",
                target,
                "--json",
                "number,title,state,isDraft,headRefName,baseRefName,url",
            ],
        )?;
        serde_json::from_str(&output).map_err(Error::from)
    }
}

/// Runs `gh` in `dir`, mapping a missing binary or auth failure to
/// [`Error::GhUnavailable`] and other failures to [`Error::Subprocess`].
fn run_gh(dir: &Path, args: &[&str]) -> Result<String> {
    let result = Command::new("gh").current_dir(dir).args(args).output();
    let output = match result {
        Ok(output) => output,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::GhUnavailable(
                "gh is not installed; install it and run `gh auth login`".into(),
            ));
        }
        Err(e) => return Err(Error::GhUnavailable(format!("failed to run gh: {e}"))),
    };
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let lowered = stderr.to_ascii_lowercase();
    if lowered.contains("auth")
        || lowered.contains("logged in")
        || lowered.contains("gh auth login")
    {
        Err(Error::GhUnavailable(format!(
            "{stderr}\nrun `gh auth login`"
        )))
    } else {
        Err(Error::Subprocess {
            program: "gh".into(),
            stderr,
        })
    }
}
