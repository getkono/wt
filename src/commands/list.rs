//! `wt list` — list worktrees as a table or newline-delimited JSON (spec §7).

use std::collections::HashSet;

use crate::cli::ListArgs;
use crate::commands::open_session;
use crate::cx::Cx;
use crate::error::Result;
use crate::model::{SortSpec, Worktree};
use crate::output::render::RenderCtx;
use crate::output::table::render_table;
use crate::time::now_unix;
use crate::util::fuzzy;
use crate::worktree_service::{build_worktrees, sort_worktrees};

/// Fallback table width when stdout is not a terminal.
const DEFAULT_WIDTH: usize = 100;

/// Lists worktrees, applying `--sort` and `--filter`, then renders a table (or
/// JSON with `--json`).
pub fn run(cx: &mut Cx, args: &ListArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let mut worktrees = build_worktrees(&session.repo, git.as_ref())?;

    let spec = match &args.sort {
        Some(field) => SortSpec::parse(field)?,
        None => SortSpec::default(),
    };
    sort_worktrees(&mut worktrees, spec);

    if let Some(filter) = &args.filter {
        let haystacks: Vec<String> = worktrees.iter().map(haystack).collect();
        let matching: HashSet<usize> = fuzzy::filter_indices(&haystacks, filter)
            .into_iter()
            .collect();
        let mut index = 0;
        worktrees.retain(|_| {
            let keep = matching.contains(&index);
            index += 1;
            keep
        });
    }

    if json {
        for worktree in &worktrees {
            cx.out.line(&worktree.to_json_line()?)?;
        }
        return Ok(0);
    }

    let ctx = RenderCtx {
        show_untracked: session.config.list_show_untracked,
        now: now_unix(),
        repo_root: &session.primary_root,
    };
    let table = render_table(
        &worktrees,
        &session.config.list_columns,
        &ctx,
        terminal_width(cx),
    );
    cx.out.text(&table)?;
    Ok(0)
}

/// The fuzzy-filter haystack for a worktree: branch + slug + path.
fn haystack(worktree: &Worktree) -> String {
    format!(
        "{} {} {}",
        worktree.branch.as_deref().unwrap_or(""),
        worktree.slug.as_deref().unwrap_or(""),
        worktree.path.display()
    )
}

/// The output width: the terminal width when stdout is a TTY, else a default.
fn terminal_width(cx: &Cx) -> usize {
    if cx.out.is_tty() {
        crossterm::terminal::size()
            .map(|(w, _)| usize::from(w))
            .unwrap_or(DEFAULT_WIDTH)
    } else {
        DEFAULT_WIDTH
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::ListArgs;
    use crate::testutil::TestRepo;

    fn args(sort: Option<&str>, filter: Option<&str>) -> ListArgs {
        ListArgs {
            sort: sort.map(str::to_string),
            filter: filter.map(str::to_string),
        }
    }

    #[test]
    fn lists_worktrees_as_table() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(None, None), false).unwrap();
        assert_eq!(code, 0);
        let out = t.out.contents();
        assert!(out.contains("main"));
        assert!(out.contains("feature/x"));
        // The current worktree carries the `*` marker.
        assert!(out.lines().any(|l| l.starts_with('*')));
    }

    #[test]
    fn json_output_one_object_per_line() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args(None, None), true).unwrap();
        let out = t.out.contents();
        assert_eq!(out.lines().count(), 2);
        for line in out.lines() {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn filter_selects_subset() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/login", "../wt-login");
        repo.add_worktree("hotfix/crash", "../wt-crash");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args(None, Some("login")), true).unwrap();
        let out = t.out.contents();
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("feature/login"));
    }

    #[test]
    fn sort_by_branch_orders_rows() {
        let repo = TestRepo::init();
        repo.add_worktree("aaa", "../wt-a");
        repo.add_worktree("zzz", "../wt-z");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args(Some("branch"), None), true).unwrap();
        let out = t.out.contents();
        let branches: Vec<String> = out
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["branch"].as_str().unwrap_or("").to_string()
            })
            .collect();
        assert_eq!(branches, vec!["aaa", "main", "zzz"]);
    }

    #[test]
    fn invalid_sort_is_usage_error() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(&mut t.cx, &args(Some("bogus"), None), false).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
