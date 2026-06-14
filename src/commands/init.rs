//! `wt init` — initialize per-repo configuration (spec §7/§11). Idempotent.

use std::path::Path;

use crate::cli::InitArgs;
use crate::commands::{confirm, open_session};
use crate::config::repo_config_path;
use crate::cx::Cx;
use crate::error::Result;

/// Writes a `.wt.toml` at the repo root (if absent) and, for a subdir store
/// layout, offers to add the store directory to `.gitignore`.
pub(crate) fn run(cx: &mut Cx, args: &InitArgs) -> Result<u8> {
    let git = cx.git.clone();
    let session = open_session(cx, git.as_ref())?;
    let config_path = repo_config_path(&session.primary_root);

    if config_path.exists() {
        cx.err.line(&format!(
            "{} already exists; leaving it unchanged",
            config_path.display()
        ))?;
    } else {
        std::fs::write(
            &config_path,
            default_contents(args.path_template.as_deref()),
        )?;
        cx.err.line(&format!("wrote {}", config_path.display()))?;
    }

    // For a subdir layout (template inside the repo), offer a .gitignore entry.
    if let Some(template) = args.path_template.as_deref()
        && let Some(store_dir) = subdir_store(template)
    {
        offer_gitignore(cx, &session.primary_root, &store_dir)?;
    }

    Ok(0)
}

/// The default `.wt.toml` contents.
fn default_contents(path_template: Option<&str>) -> String {
    let mut out =
        String::from("# wt per-repo configuration. See the spec or `wt config --help`.\n");
    match path_template {
        Some(template) => out.push_str(&format!("path_template = \"{template}\"\n")),
        None => out.push_str(
            "# path_template = \"{repo_parent}/{repo}.worktrees/{repo}-{branch_slug}\"\n\
             # copy = [\".env\"]\n",
        ),
    }
    out
}

/// If the template places worktrees inside the repo (`{repo_root}/...`), returns
/// the literal store directory (e.g. `.worktrees/`).
fn subdir_store(template: &str) -> Option<String> {
    let rest = template.strip_prefix("{repo_root}/")?;
    let literal: String = rest.chars().take_while(|&c| c != '{').collect();
    let dir = literal.trim_end_matches('/');
    if dir.is_empty() {
        None
    } else {
        Some(format!("{dir}/"))
    }
}

/// Offers to append `entry` to the repo's `.gitignore`.
fn offer_gitignore(cx: &mut Cx, root: &Path, entry: &str) -> Result<()> {
    let gitignore = root.join(".gitignore");
    let existing = std::fs::read_to_string(&gitignore).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim() == entry.trim_end_matches('/') || l.trim() == entry)
    {
        return Ok(());
    }
    if confirm(cx, &format!("Add `{entry}` to .gitignore? [y/N] "))? {
        let mut content = existing;
        if !content.is_empty() && !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(entry);
        content.push('\n');
        std::fs::write(&gitignore, content)?;
        cx.err.line(&format!("added `{entry}` to .gitignore"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::cli::InitArgs;
    use crate::testutil::{CannedInput, TestRepo};

    fn args(template: Option<&str>) -> InitArgs {
        InitArgs {
            path_template: template.map(str::to_string),
        }
    }

    #[test]
    fn writes_default_config_idempotently() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args(None)).unwrap();
        let path = repo.root().join(".wt.toml");
        assert!(path.exists());
        let first = std::fs::read_to_string(&path).unwrap();

        // Running again leaves it unchanged.
        let mut t2 = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t2.cx, &args(None)).unwrap();
        assert!(t2.err.contents().contains("already exists"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    }

    #[test]
    fn writes_given_path_template() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        super::run(&mut t.cx, &args(Some("{home}/wt/{branch_slug}"))).unwrap();
        let content = std::fs::read_to_string(repo.root().join(".wt.toml")).unwrap();
        assert!(content.contains("path_template = \"{home}/wt/{branch_slug}\""));
    }

    #[test]
    fn subdir_layout_offers_gitignore() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        t.cx.input = Box::new(CannedInput::new(&["y"]));
        super::run(
            &mut t.cx,
            &args(Some("{repo_root}/.worktrees/{branch_slug}")),
        )
        .unwrap();
        let gitignore = std::fs::read_to_string(repo.root().join(".gitignore")).unwrap();
        assert!(gitignore.contains(".worktrees/"));
    }

    #[test]
    fn subdir_detection() {
        assert_eq!(
            super::subdir_store("{repo_root}/.worktrees/{branch_slug}"),
            Some(".worktrees/".to_string())
        );
        assert_eq!(
            super::subdir_store("{repo_parent}/{repo}.worktrees/{repo}-{branch_slug}"),
            None
        );
    }
}
