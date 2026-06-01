//! Pure human renderers for worktree rows and status blocks (spec §7).

use std::fmt::Write as _;
use std::path::Path;

use crate::git::status::StatusEntry;
use crate::model::{Column, Worktree};
use crate::time::{parse_iso8601, relative};

/// Context for rendering a list cell.
pub struct RenderCtx<'a> {
    /// Whether untracked files show a `?` in the dirty column.
    pub show_untracked: bool,
    /// Reference time (Unix seconds) for relative timestamps.
    pub now: i64,
    /// Repository root, for relative path display.
    pub repo_root: &'a Path,
}

/// The status marker for the Status column (spec §7).
pub fn status_marker(worktree: &Worktree) -> char {
    if worktree.is_current {
        '*'
    } else if worktree.is_missing {
        '!'
    } else if worktree.is_detached {
        '~'
    } else {
        ' '
    }
}

/// The dirty marker for the Dirty column (spec §7).
pub fn dirty_marker(worktree: &Worktree, show_untracked: bool) -> char {
    if worktree.dirty == Some(true) {
        'M'
    } else if show_untracked && worktree.has_untracked == Some(true) {
        '?'
    } else {
        ' '
    }
}

/// The branch display: the branch name, or `(HEAD detached @ <hash>)`.
pub fn branch_display(worktree: &Worktree) -> String {
    match &worktree.branch {
        Some(branch) => branch.clone(),
        None => {
            let hash = worktree
                .commit
                .as_ref()
                .map_or("unknown", |c| c.hash.as_str());
            format!("(HEAD detached @ {hash})")
        }
    }
}

/// The ahead/behind cell: `↑N ↓M`, or `–` when there is no upstream.
pub fn ahead_behind_cell(worktree: &Worktree) -> String {
    match (worktree.ahead, worktree.behind) {
        (Some(ahead), Some(behind)) => format!("↑{ahead} ↓{behind}"),
        _ => "–".to_string(),
    }
}

/// The PR cell: `#N (state)`, or empty when no PR is recorded.
pub fn pr_cell(worktree: &Worktree) -> String {
    match &worktree.pr {
        Some(pr) => format!("#{} ({})", pr.number, pr.state.as_str()),
        None => String::new(),
    }
}

/// The path cell: relative to the repo root, or absolute if outside it.
pub fn path_cell(worktree: &Worktree, repo_root: &Path) -> String {
    match worktree.path.strip_prefix(repo_root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => worktree.path.to_string_lossy().into_owned(),
    }
}

/// The commit cell: short hash + subject + relative time, or empty.
pub fn commit_cell(worktree: &Worktree, now: i64) -> String {
    match &worktree.commit {
        Some(commit) => {
            let rel = parse_iso8601(&commit.timestamp)
                .map(|unix| relative(now, unix))
                .unwrap_or_default();
            format!("{} {} ({rel})", commit.hash, commit.subject)
        }
        None => String::new(),
    }
}

/// Renders a single column's cell for a worktree.
pub fn cell(worktree: &Worktree, column: Column, ctx: &RenderCtx) -> String {
    match column {
        Column::Status => status_marker(worktree).to_string(),
        Column::Dirty => dirty_marker(worktree, ctx.show_untracked).to_string(),
        Column::Branch => branch_display(worktree),
        Column::Path => path_cell(worktree, ctx.repo_root),
        Column::AheadBehind => ahead_behind_cell(worktree),
        Column::Commit => commit_cell(worktree, ctx.now),
        Column::Pr => pr_cell(worktree),
    }
}

/// Renders the detailed `wt status` block for one worktree (spec §7).
pub fn status_block(worktree: &Worktree, entries: &[StatusEntry]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "worktree: {}", worktree.path.display());

    let branch = branch_display(worktree);
    match &worktree.upstream {
        Some(upstream) => {
            let _ = writeln!(out, "branch:   {branch} → {upstream}");
        }
        None => {
            let _ = writeln!(out, "branch:   {branch} (no upstream)");
        }
    }
    if let Some(base) = &worktree.base_ref {
        let _ = writeln!(out, "base:     {base}");
    }

    if worktree.is_missing {
        let _ = writeln!(out, "(directory already deleted)");
        return out;
    }

    if let (Some(ahead), Some(behind)) = (worktree.ahead, worktree.behind) {
        let _ = writeln!(out, "ahead:    {ahead}  behind: {behind}");
    }
    if let Some(pr) = &worktree.pr {
        let _ = writeln!(
            out,
            "pr:       #{} ({}) \"{}\"",
            pr.number,
            pr.state.as_str(),
            pr.title
        );
    }
    if !entries.is_empty() {
        let _ = writeln!(out, "dirty:");
        for entry in entries {
            let _ = writeln!(out, "  {}  {}", entry.marker, entry.path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Commit, Pr, PrState};
    use std::path::PathBuf;

    fn base() -> Worktree {
        let mut w = Worktree::new(PathBuf::from("/repo/main"));
        w.branch = Some("main".into());
        w.slug = Some("main".into());
        w
    }

    #[test]
    fn status_markers() {
        let mut w = base();
        assert_eq!(status_marker(&w), ' ');
        w.is_detached = true;
        assert_eq!(status_marker(&w), '~');
        w.is_missing = true;
        assert_eq!(status_marker(&w), '!');
        w.is_current = true;
        assert_eq!(status_marker(&w), '*'); // current wins
    }

    #[test]
    fn dirty_markers_respect_show_untracked() {
        let mut w = base();
        assert_eq!(dirty_marker(&w, true), ' ');
        w.has_untracked = Some(true);
        assert_eq!(dirty_marker(&w, true), '?');
        assert_eq!(dirty_marker(&w, false), ' '); // suppressed
        w.dirty = Some(true);
        assert_eq!(dirty_marker(&w, true), 'M'); // modified wins
    }

    #[test]
    fn ahead_behind_and_no_upstream() {
        let mut w = base();
        assert_eq!(ahead_behind_cell(&w), "–");
        w.ahead = Some(2);
        w.behind = Some(1);
        assert_eq!(ahead_behind_cell(&w), "↑2 ↓1");
    }

    #[test]
    fn branch_display_detached() {
        let mut w = base();
        w.branch = None;
        w.is_detached = true;
        w.commit = Some(Commit {
            hash: "abc1234".into(),
            subject: "x".into(),
            author: "a".into(),
            timestamp: "2024-01-15T10:30:00Z".into(),
        });
        assert_eq!(branch_display(&w), "(HEAD detached @ abc1234)");
    }

    #[test]
    fn path_cell_relative_and_absolute() {
        let root = Path::new("/repo");
        let mut w = base();
        w.path = PathBuf::from("/repo");
        assert_eq!(path_cell(&w, root), ".");
        w.path = PathBuf::from("/repo/.worktrees/x");
        assert_eq!(path_cell(&w, root), ".worktrees/x");
        w.path = PathBuf::from("/elsewhere/y");
        assert_eq!(path_cell(&w, root), "/elsewhere/y");
    }

    #[test]
    fn pr_cell_renders_number_and_state() {
        let mut w = base();
        assert_eq!(pr_cell(&w), "");
        w.pr = Some(Pr {
            number: 42,
            state: PrState::Open,
            title: "t".into(),
        });
        assert_eq!(pr_cell(&w), "#42 (open)");
    }

    #[test]
    fn commit_cell_includes_hash_subject_time() {
        let mut w = base();
        assert_eq!(commit_cell(&w, 0), "");
        let ts = "2024-01-15T10:30:00Z";
        w.commit = Some(Commit {
            hash: "abc1234".into(),
            subject: "Add login".into(),
            author: "Alice".into(),
            timestamp: ts.into(),
        });
        let now = parse_iso8601(ts).unwrap() + 3 * 3600;
        assert_eq!(commit_cell(&w, now), "abc1234 Add login (3h ago)");
    }

    #[test]
    fn status_block_full() {
        let mut w = base();
        w.upstream = Some("origin/main".into());
        w.base_ref = Some("develop".into());
        w.ahead = Some(3);
        w.behind = Some(0);
        w.pr = Some(Pr {
            number: 42,
            state: PrState::Open,
            title: "Add login page".into(),
        });
        let entries = vec![
            StatusEntry {
                marker: 'M',
                path: "src/main.rs".into(),
            },
            StatusEntry {
                marker: '?',
                path: "scratch.txt".into(),
            },
        ];
        let block = status_block(&w, &entries);
        assert!(block.contains("worktree: /repo/main"));
        assert!(block.contains("branch:   main → origin/main"));
        assert!(block.contains("base:     develop"));
        assert!(block.contains("ahead:    3  behind: 0"));
        assert!(block.contains("pr:       #42 (open) \"Add login page\""));
        assert!(block.contains("dirty:\n  M  src/main.rs\n  ?  scratch.txt"));
    }

    #[test]
    fn status_block_no_upstream_omits_ahead_behind() {
        let w = base();
        let block = status_block(&w, &[]);
        assert!(block.contains("main (no upstream)"));
        assert!(!block.contains("ahead:"));
        assert!(!block.contains("dirty:"));
    }

    #[test]
    fn status_block_missing_worktree() {
        let mut w = base();
        w.is_missing = true;
        w.base_ref = Some("main".into());
        let block = status_block(&w, &[]);
        assert!(block.contains("(directory already deleted)"));
        assert!(block.contains("base:     main"));
        assert!(!block.contains("ahead:"));
    }
}
