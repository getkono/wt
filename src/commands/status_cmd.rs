//! `wt status [<query>]` — detailed status for one or all worktrees (spec §7).

use crate::cli::StatusArgs;
use crate::commands::{Resolution, open_session, resolve_query};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::git::status_of;
use crate::output::render::status_block;
use crate::worktree_service::build_worktrees;

/// Renders the status block(s) to stdout, or newline-delimited JSON with
/// `--json`. Default target is the current worktree; `--all` reports every one.
pub fn run(cx: &mut Cx, args: &StatusArgs, json: bool) -> Result<u8> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let worktrees = build_worktrees(&session.repo, git.as_ref())?;

    let selected: Vec<usize> = if args.all {
        (0..worktrees.len()).collect()
    } else if let Some(query) = &args.query {
        match resolve_query(cx, &worktrees, query) {
            Resolution::Found(index) => vec![index],
            Resolution::Ambiguous => return Ok(3),
            Resolution::NotFound => {
                return Err(Error::NotFound {
                    query: query.clone(),
                });
            }
        }
    } else {
        match worktrees.iter().position(|w| w.is_current) {
            Some(index) => vec![index],
            None => return Err(Error::NoCurrentWorktree),
        }
    };

    if json {
        for &index in &selected {
            cx.out.line(&worktrees[index].to_json_line()?)?;
        }
        return Ok(0);
    }

    let mut output = String::new();
    for (n, &index) in selected.iter().enumerate() {
        if n > 0 {
            output.push('\n');
        }
        let worktree = &worktrees[index];
        let entries = if worktree.is_missing {
            Vec::new()
        } else {
            status_of(git.as_ref(), &worktree.path)
                .map(|s| s.entries)
                .unwrap_or_default()
        };
        output.push_str(&status_block(worktree, &entries));
    }
    crate::output::pager::page(cx, &output)?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::cli::StatusArgs;
    use crate::testutil::TestRepo;

    fn args(query: Option<&str>, all: bool) -> StatusArgs {
        StatusArgs {
            query: query.map(str::to_string),
            all,
        }
    }

    #[test]
    fn current_worktree_status() {
        let repo = TestRepo::init();
        repo.write("README.md", "changed\n");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &args(None, false), false).unwrap();
        assert_eq!(code, 0);
        let out = t.out.contents();
        assert!(out.contains("branch:   main"));
        assert!(out.contains("dirty:"));
        assert!(out.contains("README.md"));
    }

    #[test]
    fn all_worktrees_status() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args(None, true), false).unwrap();
        let out = t.out.contents();
        assert!(out.contains("branch:   main"));
        assert!(out.contains("branch:   feature/x"));
    }

    #[test]
    fn json_output_is_newline_delimited() {
        let repo = TestRepo::init();
        repo.add_worktree("feature/x", "../wt-x");
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args(None, true), true).unwrap();
        let out = t.out.contents();
        assert_eq!(out.lines().count(), 2);
        for line in out.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["schema_version"], serde_json::json!(1));
        }
    }
}
