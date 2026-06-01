//! Worktree-store path-template rendering (spec §6).
//!
//! New worktrees are placed according to a configurable template with the
//! variables `{repo_parent}`, `{repo}`, `{repo_root}`, `{branch}`,
//! `{branch_slug}`, and `{home}`. [`render`] substitutes them; [`ensure_outside_git`]
//! rejects a rendered path that would land inside the `.git` directory.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The default worktree-store template (spec §6 "Sibling").
pub const DEFAULT_TEMPLATE: &str = "{repo_parent}/{repo}.worktrees/{branch_slug}";

/// The values substituted into a path template. For a bare repository these
/// resolve against the bare repo's own directory (spec §6).
#[derive(Debug, Clone)]
pub struct TemplateVars {
    /// Directory containing the repo root.
    pub repo_parent: PathBuf,
    /// Repo directory name.
    pub repo: String,
    /// Repo root (or bare repo path).
    pub repo_root: PathBuf,
    /// Raw branch name.
    pub branch: String,
    /// Filesystem-safe branch slug.
    pub branch_slug: String,
    /// The user's home directory.
    pub home: PathBuf,
}

/// Renders `template`, substituting the [`TemplateVars`]. An unknown `{var}` or
/// an unterminated `{` is a configuration error.
pub fn render(template: &str, vars: &TemplateVars) -> Result<PathBuf> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| template_error(template, "unterminated '{' in template"))?;
        let name = &after[..close];
        out.push_str(&substitute(name, vars).ok_or_else(|| {
            template_error(template, &format!("unknown template variable {{{name}}}"))
        })?);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(PathBuf::from(out))
}

/// Returns the substitution for a variable name, or `None` if unknown.
fn substitute(name: &str, vars: &TemplateVars) -> Option<String> {
    Some(match name {
        "repo_parent" => vars.repo_parent.to_string_lossy().into_owned(),
        "repo" => vars.repo.clone(),
        "repo_root" => vars.repo_root.to_string_lossy().into_owned(),
        "branch" => vars.branch.clone(),
        "branch_slug" => vars.branch_slug.clone(),
        "home" => vars.home.to_string_lossy().into_owned(),
        _ => return None,
    })
}

/// Builds a config error for a bad `path_template`.
fn template_error(template: &str, reason: &str) -> Error {
    Error::Config {
        file: "path_template".into(),
        key: template.into(),
        reason: reason.into(),
    }
}

/// Rejects a rendered worktree path that lies inside the repository's `.git`
/// directory (spec §6).
pub fn ensure_outside_git(rendered: &Path, git_dir: &Path) -> Result<()> {
    if rendered.starts_with(git_dir) {
        return Err(Error::Config {
            file: "path_template".into(),
            key: "path_template".into(),
            reason: format!(
                "template renders a worktree inside the git directory: {}",
                rendered.display()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> TemplateVars {
        TemplateVars {
            repo_parent: PathBuf::from("/home/u/code"),
            repo: "proj".into(),
            repo_root: PathBuf::from("/home/u/code/proj"),
            branch: "feature/login".into(),
            branch_slug: "feature-login".into(),
            home: PathBuf::from("/home/u"),
        }
    }

    #[test]
    fn renders_default_sibling_template() {
        let p = render(DEFAULT_TEMPLATE, &vars()).unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/u/code/proj.worktrees/feature-login")
        );
    }

    #[test]
    fn renders_subdir_and_central_presets() {
        let sub = render("{repo_root}/.worktrees/{branch_slug}", &vars()).unwrap();
        assert_eq!(
            sub,
            PathBuf::from("/home/u/code/proj/.worktrees/feature-login")
        );
        let central = render("{home}/worktrees/{repo}/{branch_slug}", &vars()).unwrap();
        assert_eq!(
            central,
            PathBuf::from("/home/u/worktrees/proj/feature-login")
        );
    }

    #[test]
    fn repo_token_does_not_clobber_repo_parent_or_root() {
        let p = render("{repo_parent}/{repo}/{repo_root}/{branch}", &vars()).unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/u/code/proj//home/u/code/proj/feature/login")
        );
    }

    #[test]
    fn unknown_variable_is_config_error() {
        let err = render("{repo}/{bogus}", &vars()).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn unterminated_brace_is_config_error() {
        let err = render("{repo}/{branch", &vars()).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
        assert!(err.to_string().contains("unterminated"));
    }

    #[test]
    fn literal_text_without_variables() {
        assert_eq!(
            render("/tmp/fixed", &vars()).unwrap(),
            PathBuf::from("/tmp/fixed")
        );
    }

    #[test]
    fn ensure_outside_git_rejects_inside_and_allows_outside() {
        let git_dir = Path::new("/home/u/code/proj/.git");
        let inside = Path::new("/home/u/code/proj/.git/worktrees/x");
        let outside = Path::new("/home/u/code/proj.worktrees/x");
        assert!(ensure_outside_git(inside, git_dir).is_err());
        assert!(ensure_outside_git(outside, git_dir).is_ok());
        // A sibling whose name merely starts with the git dir name is allowed.
        let sibling = Path::new("/home/u/code/proj/.gitignore-dir/x");
        assert!(ensure_outside_git(sibling, git_dir).is_ok());
    }
}
