//! Compact table layout for `wt list` (spec §7). Columns are padded to their
//! natural width; the Commit column flexes and truncates to fit the terminal.

use std::fmt::Write as _;

use crate::model::{Column, Worktree};
use crate::output::color::{ansi, paint};
use crate::output::render::{RenderCtx, cell};

/// The minimum width allotted to the (flexible) Commit column.
const MIN_COMMIT_WIDTH: usize = 12;
/// The column separator.
const SEPARATOR: &str = "  ";

/// Approximate display width (one column per character).
fn display_width(text: &str) -> usize {
    text.chars().count()
}

/// Truncates `text` to `width` display columns, appending `…` if shortened.
fn truncate(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

/// Pads `text` on the right with spaces to `width` columns.
fn pad_right(text: &str, width: usize) -> String {
    let len = display_width(text);
    if len >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

/// Colorizes a cell's content based on its column and value (ANSI is zero-width,
/// so this is applied after layout). Returns `None` for uncolored columns.
fn cell_color(column: Column, value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    match column {
        Column::Status => match trimmed {
            "*" => Some(ansi::GREEN),
            "!" => Some(ansi::RED),
            "~" => Some(ansi::YELLOW),
            _ => None,
        },
        Column::Dirty => match trimmed {
            "M" => Some(ansi::RED),
            "?" => Some(ansi::YELLOW),
            _ => None,
        },
        Column::Pr => match () {
            _ if trimmed.contains("(open)") => Some(ansi::GREEN),
            _ if trimmed.contains("(merged)") => Some(ansi::MAGENTA),
            _ if trimmed.contains("(closed)") => Some(ansi::RED),
            _ if trimmed.contains("(draft)") => Some(ansi::DIM),
            _ => None,
        },
        _ => None,
    }
}

/// Renders the worktrees as an aligned table for the given columns and terminal
/// width. ANSI color is applied to status/dirty/PR cells when `color` is set.
pub fn render_table(
    worktrees: &[Worktree],
    columns: &[Column],
    ctx: &RenderCtx,
    width: usize,
    color: bool,
) -> String {
    if worktrees.is_empty() || columns.is_empty() {
        return String::new();
    }
    let rows: Vec<Vec<String>> = worktrees
        .iter()
        .map(|w| columns.iter().map(|&c| cell(w, c, ctx)).collect())
        .collect();

    let mut widths: Vec<usize> = (0..columns.len())
        .map(|ci| {
            rows.iter()
                .map(|r| display_width(&r[ci]))
                .max()
                .unwrap_or(0)
        })
        .collect();

    // Flex the Commit column to fit the terminal.
    if let Some(ci) = columns.iter().position(|c| *c == Column::Commit) {
        let others: usize = widths
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != ci)
            .map(|(_, w)| *w)
            .sum();
        let seps = SEPARATOR.len() * columns.len().saturating_sub(1);
        let budget = width.saturating_sub(others + seps).max(MIN_COMMIT_WIDTH);
        widths[ci] = widths[ci].min(budget);
    }

    let mut out = String::new();
    let last = columns.len() - 1;
    for row in &rows {
        let mut line = String::new();
        for (ci, value) in row.iter().enumerate() {
            let truncated = truncate(value, widths[ci]);
            // Pad the plain text first (ANSI is zero-width); color afterward.
            let content = if ci == last {
                truncated
            } else {
                pad_right(&truncated, widths[ci])
            };
            let painted = match cell_color(columns[ci], &content) {
                Some(code) => paint(&content, code, color),
                None => content,
            };
            line.push_str(&painted);
            if ci != last {
                line.push_str(SEPARATOR);
            }
        }
        let _ = writeln!(out, "{}", line.trim_end());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Commit, Worktree};
    use std::path::{Path, PathBuf};

    fn ctx() -> RenderCtx<'static> {
        RenderCtx {
            show_untracked: true,
            now: 0,
            repo_root: Path::new("/repo"),
        }
    }

    fn wt(path: &str, branch: &str, current: bool) -> Worktree {
        let mut w = Worktree::new(PathBuf::from(path));
        w.branch = Some(branch.into());
        w.slug = Some(branch.replace('/', "-"));
        w.is_current = current;
        w.dirty = Some(false);
        w.has_untracked = Some(false);
        w
    }

    #[test]
    fn truncate_and_pad_helpers() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("x", 0), "");
        assert_eq!(pad_right("ab", 5), "ab   ");
        assert_eq!(pad_right("abcdef", 3), "abcdef");
    }

    #[test]
    fn renders_aligned_rows() {
        let worktrees = vec![
            wt("/repo", "main", true),
            wt("/repo/.worktrees/feature-x", "feature/x", false),
        ];
        let table = render_table(&worktrees, &Column::ALL, &ctx(), 120, false);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 2);
        // Current marker on the first row, branch names present.
        assert!(lines[0].starts_with('*'));
        assert!(lines[0].contains("main"));
        assert!(lines[1].contains("feature/x"));
    }

    #[test]
    fn commit_column_truncates_in_narrow_terminal() {
        let mut w = wt("/repo", "main", true);
        w.commit = Some(Commit {
            hash: "abc1234".into(),
            subject: "A very long commit subject that should be truncated to fit".into(),
            author: "A".into(),
            timestamp: "2024-01-15T10:30:00Z".into(),
        });
        let table = render_table(&[w], &[Column::Branch, Column::Commit], &ctx(), 40, false);
        assert!(table.contains('…'));
        // No line exceeds a reasonable width given truncation.
        for line in table.lines() {
            assert!(display_width(line) <= 40, "line too wide: {line:?}");
        }
    }

    #[test]
    fn empty_input_renders_nothing() {
        assert_eq!(render_table(&[], &Column::ALL, &ctx(), 80, false), "");
    }

    #[test]
    fn respects_column_subset() {
        let worktrees = vec![wt("/repo", "main", false)];
        let table = render_table(&worktrees, &[Column::Branch], &ctx(), 80, false);
        assert_eq!(table.trim_end(), "main");
    }

    #[test]
    fn color_wraps_markers_without_changing_visible_width() {
        let current = [wt("/repo", "main", true)]; // status '*'
        let plain = render_table(&current, &Column::ALL, &ctx(), 120, false);
        let colored = render_table(&current, &Column::ALL, &ctx(), 120, true);
        assert!(colored.contains("\x1b["));
        assert!(!plain.contains("\x1b["));
        // Stripping ANSI from the colored output recovers the plain output.
        let strip = |s: &str| {
            let mut out = String::new();
            let mut chars = s.chars();
            while let Some(c) = chars.next() {
                if c == '\x1b' {
                    for n in chars.by_ref() {
                        if n == 'm' {
                            break;
                        }
                    }
                } else {
                    out.push(c);
                }
            }
            out
        };
        assert_eq!(strip(&colored), plain);
    }
}
