//! Working-tree status via `git status --porcelain` (spec §4 sanctioned
//! subprocess read — the read most likely to need a fallback from `gix`).
//!
//! The parser distinguishes tracked modifications/staging ("dirty") from
//! untracked files (spec §12), and produces the per-file list `wt status` shows.

use std::path::Path;

use crate::error::Result;
use crate::git::cli::GitCli;

/// A single changed path, collapsed to a display marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatusEntry {
    /// `M` for a tracked modification/staging, `?` for an untracked file.
    pub(crate) marker: char,
    /// The file path relative to the worktree.
    pub(crate) path: String,
}

/// A worktree's status summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StatusSummary {
    /// Whether any tracked files are modified or staged.
    pub(crate) dirty: bool,
    /// Whether any untracked files are present.
    pub(crate) has_untracked: bool,
    /// The changed entries, in `git`'s order.
    pub(crate) entries: Vec<StatusEntry>,
}

/// Parses `git status --porcelain=v1 -z` output. Each record is `XY<space>path`
/// terminated by NUL; rename/copy records carry an extra source path that is
/// consumed and ignored.
pub(crate) fn parse_status_porcelain(z: &str) -> StatusSummary {
    let mut summary = StatusSummary::default();
    let mut fields = z.split('\0');
    while let Some(field) = fields.next() {
        if field.len() < 3 {
            continue;
        }
        let xy = &field[..2];
        let path = &field[3..];
        if xy == "??" {
            summary.has_untracked = true;
            summary.entries.push(StatusEntry {
                marker: '?',
                path: path.to_string(),
            });
        } else if xy == "!!" {
            // Ignored; not reported.
        } else {
            summary.dirty = true;
            summary.entries.push(StatusEntry {
                marker: 'M',
                path: path.to_string(),
            });
            // Rename/copy records are followed by the original path.
            if xy.contains('R') || xy.contains('C') {
                fields.next();
            }
        }
    }
    summary
}

/// Runs `git status` in the worktree directory and parses the result.
pub(crate) fn status_of(git: &dyn GitCli, worktree_dir: &Path) -> Result<StatusSummary> {
    let output = git.run(worktree_dir, &["status", "--porcelain=v1", "-z"])?;
    Ok(parse_status_porcelain(&output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::cli::RealGit;
    use crate::testutil::TestRepo;

    #[test]
    fn clean_worktree_is_not_dirty() {
        let s = parse_status_porcelain("");
        assert!(!s.dirty);
        assert!(!s.has_untracked);
        assert!(s.entries.is_empty());
    }

    #[test]
    fn parses_modified_staged_and_untracked() {
        // " M src/a" (modified), "M  src/b" (staged), "?? scratch" (untracked).
        let z = " M src/a\0M  src/b\0?? scratch\0";
        let s = parse_status_porcelain(z);
        assert!(s.dirty);
        assert!(s.has_untracked);
        assert_eq!(s.entries.len(), 3);
        assert_eq!(
            s.entries[0],
            StatusEntry {
                marker: 'M',
                path: "src/a".into()
            }
        );
        assert_eq!(
            s.entries[2],
            StatusEntry {
                marker: '?',
                path: "scratch".into()
            }
        );
    }

    #[test]
    fn untracked_only_is_not_dirty() {
        let s = parse_status_porcelain("?? new.txt\0");
        assert!(!s.dirty);
        assert!(s.has_untracked);
    }

    #[test]
    fn ignored_entries_are_skipped() {
        let s = parse_status_porcelain("!! target/\0");
        assert!(!s.dirty);
        assert!(!s.has_untracked);
        assert!(s.entries.is_empty());
    }

    #[test]
    fn rename_consumes_source_field() {
        // "R  new\0old\0" then a normal untracked entry.
        let z = "R  new\0old\0?? u\0";
        let s = parse_status_porcelain(z);
        assert!(s.dirty);
        assert!(s.has_untracked);
        // The "old" source field must not be treated as its own entry.
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.entries[0].path, "new");
        assert_eq!(s.entries[1].path, "u");
    }

    #[test]
    fn status_of_real_repo() {
        let repo = TestRepo::init();
        // Clean.
        let s = status_of(&RealGit, repo.root()).unwrap();
        assert!(!s.dirty && !s.has_untracked);
        // Modify a tracked file.
        repo.write("README.md", "changed\n");
        let s = status_of(&RealGit, repo.root()).unwrap();
        assert!(s.dirty);
        assert!(!s.has_untracked);
        // Add an untracked file.
        repo.write("scratch.txt", "x\n");
        let s = status_of(&RealGit, repo.root()).unwrap();
        assert!(s.dirty);
        assert!(s.has_untracked);
    }
}
