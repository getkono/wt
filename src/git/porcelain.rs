//! Pure parsers for `git` porcelain output (spec §4 sanctioned subprocess
//! reads). Kept separate from the I/O so they are unit-testable on fixed input.

use std::path::PathBuf;

/// A worktree as reported by `git worktree list --porcelain`, before any
/// filesystem checks or enrichment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWorktree {
    /// Absolute path of the worktree.
    pub path: PathBuf,
    /// Checked-out commit (hex OID), or `None` for the bare entry.
    pub head: Option<String>,
    /// Branch name (without the `refs/heads/` prefix), or `None` if
    /// detached/bare.
    pub branch: Option<String>,
    /// Whether this is the bare repository entry.
    pub is_bare: bool,
    /// Whether the worktree has a detached HEAD.
    pub is_detached: bool,
    /// Whether the worktree is locked.
    pub is_locked: bool,
    /// Whether Git considers the worktree prunable.
    pub is_prunable: bool,
    /// Whether this is the main (first) worktree.
    pub is_main: bool,
    /// Whether the worktree's directory is missing on disk. Left `false` by the
    /// parser; filled in by enumeration, which can touch the filesystem.
    pub is_missing: bool,
}

impl RawWorktree {
    fn new(path: PathBuf) -> Self {
        RawWorktree {
            path,
            head: None,
            branch: None,
            is_bare: false,
            is_detached: false,
            is_locked: false,
            is_prunable: false,
            is_main: false,
            is_missing: false,
        }
    }
}

/// Parses `git worktree list --porcelain` output. The first record is marked as
/// the main worktree. Missing-directory detection is applied separately (it
/// requires filesystem access).
pub fn parse_worktree_list(porcelain: &str) -> Vec<RawWorktree> {
    let mut result: Vec<RawWorktree> = Vec::new();
    let mut current: Option<RawWorktree> = None;
    for line in porcelain.lines() {
        if line.is_empty() {
            if let Some(wt) = current.take() {
                result.push(wt);
            }
            continue;
        }
        let (key, rest) = match line.split_once(' ') {
            Some((k, r)) => (k, Some(r)),
            None => (line, None),
        };
        match key {
            "worktree" => {
                if let Some(wt) = current.take() {
                    result.push(wt);
                }
                current = Some(RawWorktree::new(PathBuf::from(rest.unwrap_or_default())));
            }
            "HEAD" => {
                if let Some(wt) = current.as_mut() {
                    wt.head = rest.map(str::to_string);
                }
            }
            "branch" => {
                if let Some(wt) = current.as_mut() {
                    wt.branch = rest.map(strip_branch_ref);
                }
            }
            "bare" => {
                if let Some(wt) = current.as_mut() {
                    wt.is_bare = true;
                }
            }
            "detached" => {
                if let Some(wt) = current.as_mut() {
                    wt.is_detached = true;
                }
            }
            "locked" => {
                if let Some(wt) = current.as_mut() {
                    wt.is_locked = true;
                }
            }
            "prunable" => {
                if let Some(wt) = current.as_mut() {
                    wt.is_prunable = true;
                }
            }
            _ => {}
        }
    }
    if let Some(wt) = current.take() {
        result.push(wt);
    }
    if let Some(first) = result.first_mut() {
        first.is_main = true;
    }
    result
}

/// Strips the `refs/heads/` prefix from a branch ref.
fn strip_branch_ref(reference: &str) -> String {
    reference
        .strip_prefix("refs/heads/")
        .unwrap_or(reference)
        .to_string()
}

/// One submodule as reported by `git submodule status`. The leading marker is
/// `' '` (in sync), `'-'` (not initialized), `'+'` (checked-out commit differs
/// from the index), or `'U'` (merge conflicts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleStatus {
    /// The leading status marker character.
    pub state: char,
    /// The submodule's path, relative to the superproject.
    pub path: String,
}

impl SubmoduleStatus {
    /// Whether the submodule is not yet initialized (marker `'-'`).
    pub fn is_uninitialized(&self) -> bool {
        self.state == '-'
    }
}

/// Parses `git submodule status` output. Each line is
/// `<marker><sha> <path>[ (<describe>)]`; the marker is the first character and
/// is *not* separated from the SHA by a space. Lines that do not parse are
/// skipped.
pub fn parse_submodule_status(output: &str) -> Vec<SubmoduleStatus> {
    let mut result = Vec::new();
    for line in output.lines() {
        let mut chars = line.chars();
        let Some(state) = chars.next() else {
            continue;
        };
        // After the marker char comes `<sha> <path>[ (<describe>)]`.
        let rest = chars.as_str();
        let Some((_sha, after_sha)) = rest.split_once(' ') else {
            continue;
        };
        // Drop a trailing ` (<describe>)` annotation, keeping paths intact.
        let path = match after_sha.rfind(" (") {
            Some(i) => &after_sha[..i],
            None => after_sha,
        }
        .trim();
        if path.is_empty() {
            continue;
        }
        result.push(SubmoduleStatus {
            state,
            path: path.to_string(),
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_main_and_linked() {
        let input = "worktree /repo\nHEAD aaa111\nbranch refs/heads/main\n\
            \n\
            worktree /repo.worktrees/feat\nHEAD bbb222\nbranch refs/heads/feature/x\n\n";
        let wts = parse_worktree_list(input);
        assert_eq!(wts.len(), 2);
        assert_eq!(wts[0].path, PathBuf::from("/repo"));
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert_eq!(wts[0].head.as_deref(), Some("aaa111"));
        assert!(wts[0].is_main);
        assert_eq!(wts[1].path, PathBuf::from("/repo.worktrees/feat"));
        assert_eq!(wts[1].branch.as_deref(), Some("feature/x"));
        assert!(!wts[1].is_main);
    }

    #[test]
    fn parses_detached_and_bare_and_locked_and_prunable() {
        let input = "worktree /bare\nbare\n\
            \n\
            worktree /d\nHEAD ccc333\ndetached\n\
            \n\
            worktree /l\nHEAD ddd\nbranch refs/heads/x\nlocked being used\n\
            \n\
            worktree /p\nHEAD eee\nbranch refs/heads/y\nprunable gitdir gone\n\n";
        let wts = parse_worktree_list(input);
        assert_eq!(wts.len(), 4);
        assert!(wts[0].is_bare && wts[0].is_main);
        assert!(wts[0].branch.is_none() && wts[0].head.is_none());
        assert!(wts[1].is_detached);
        assert!(wts[1].branch.is_none());
        assert!(wts[2].is_locked);
        assert_eq!(wts[2].branch.as_deref(), Some("x"));
        assert!(wts[3].is_prunable);
    }

    #[test]
    fn handles_trailing_record_without_blank_line() {
        let input = "worktree /only\nHEAD f00\nbranch refs/heads/main";
        let wts = parse_worktree_list(input);
        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn handles_paths_with_spaces() {
        let input = "worktree /my repo/wt\nHEAD a1\nbranch refs/heads/main\n";
        let wts = parse_worktree_list(input);
        assert_eq!(wts[0].path, PathBuf::from("/my repo/wt"));
    }

    #[test]
    fn empty_input_yields_no_worktrees() {
        assert!(parse_worktree_list("").is_empty());
    }

    #[test]
    fn parses_submodule_status_markers() {
        let input = "-aaa111 libs/uninit\n cccddd libs/ok (heads/main)\n\
            +bbb222 vendor/drift (v1.2-3-gabcdef)\nUeee444 vendor/conflict\n";
        let subs = parse_submodule_status(input);
        assert_eq!(subs.len(), 4);
        assert_eq!(subs[0].state, '-');
        assert_eq!(subs[0].path, "libs/uninit");
        assert!(subs[0].is_uninitialized());
        assert_eq!(subs[1].state, ' ');
        assert_eq!(subs[1].path, "libs/ok");
        assert!(!subs[1].is_uninitialized());
        assert_eq!(subs[2].state, '+');
        assert_eq!(subs[2].path, "vendor/drift");
        assert_eq!(subs[3].state, 'U');
        assert_eq!(subs[3].path, "vendor/conflict");
    }

    #[test]
    fn submodule_status_keeps_paths_with_spaces() {
        let subs = parse_submodule_status("-deadbeef my libs/sub\n");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].path, "my libs/sub");
    }

    #[test]
    fn submodule_status_skips_unparseable_lines() {
        // Empty input, a marker with no SHA/path, and a marker+SHA with no path.
        assert!(parse_submodule_status("").is_empty());
        assert!(parse_submodule_status("-\n").is_empty());
        assert!(parse_submodule_status("-onlysha\n").is_empty());
    }
}
