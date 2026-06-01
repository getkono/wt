//! Configuration loading and merging (spec §11).
//!
//! Two file layers are merged over the built-in defaults: the global
//! `config.toml` (under the platform config dir or `$XDG_CONFIG_HOME/wt`) and
//! the per-repo `.wt.toml` at the repo root. Per-repo overrides global; both are
//! parsed and validated on every invocation ([`load`]).

mod parse;
mod schema;
pub mod wtconfig;

use std::path::{Path, PathBuf};

use directories::ProjectDirs;

use crate::cx::Env;
use crate::error::{Error, Result};

pub use parse::parse_layer;
pub use schema::{Config, ConfigLayer};
pub use wtconfig::WtMeta;

/// The path to the global `config.toml`, honoring `$XDG_CONFIG_HOME` and falling
/// back to the platform config directory. `None` only if no home directory can
/// be determined.
pub fn global_config_path(env: &Env) -> Option<PathBuf> {
    if let Some(xdg) = env.get("XDG_CONFIG_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(xdg).join("wt").join("config.toml"));
    }
    ProjectDirs::from("", "", "wt").map(|dirs| dirs.config_dir().join("config.toml"))
}

/// The path to the per-repo `.wt.toml` at `repo_root`.
pub fn repo_config_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".wt.toml")
}

/// Reads a file's text, returning `None` if it does not exist.
fn read_if_exists(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::Config {
            file: path.display().to_string(),
            key: String::new(),
            reason: format!("cannot read: {e}"),
        }),
    }
}

/// Loads and merges the global and per-repo configuration over the defaults.
/// `repo_root` is `None` when not inside a repository (only the global layer is
/// considered).
pub fn load(repo_root: Option<&Path>, env: &Env) -> Result<Config> {
    let mut config = Config::default();
    if let Some(global) = global_config_path(env)
        && let Some(text) = read_if_exists(&global)?
    {
        config.apply(parse_layer(&text, &global.display().to_string())?);
    }
    if let Some(root) = repo_root {
        let per_repo = repo_config_path(root);
        if let Some(text) = read_if_exists(&per_repo)? {
            config.apply(parse_layer(&text, &per_repo.display().to_string())?);
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<HashMap<_, _>>(),
        )
    }

    #[test]
    fn global_path_uses_xdg_when_set() {
        let e = env(&[("XDG_CONFIG_HOME", "/cfg")]);
        assert_eq!(
            global_config_path(&e),
            Some(PathBuf::from("/cfg/wt/config.toml"))
        );
    }

    #[test]
    fn global_path_falls_back_to_platform_dir() {
        // With XDG unset, the platform config dir is used (present on macOS/Linux).
        assert!(global_config_path(&env(&[])).is_some());
    }

    #[test]
    fn load_without_files_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let e = env(&[("XDG_CONFIG_HOME", dir.path().to_str().unwrap())]);
        let config = load(Some(dir.path()), &e).unwrap();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn per_repo_overrides_global() {
        let global_dir = tempfile::tempdir().unwrap();
        let repo_dir = tempfile::tempdir().unwrap();
        // Global config: set remote + disable mouse.
        let global_wt = global_dir.path().join("wt");
        std::fs::create_dir_all(&global_wt).unwrap();
        std::fs::write(
            global_wt.join("config.toml"),
            "pr.default_remote = \"global-remote\"\n[ui]\nmouse = false\n",
        )
        .unwrap();
        // Per-repo: override only the remote.
        std::fs::write(
            repo_config_path(repo_dir.path()),
            "[pr]\ndefault_remote = \"repo-remote\"\n",
        )
        .unwrap();

        let e = env(&[("XDG_CONFIG_HOME", global_dir.path().to_str().unwrap())]);
        let config = load(Some(repo_dir.path()), &e).unwrap();
        assert_eq!(config.pr_default_remote, "repo-remote"); // per-repo wins
        assert!(!config.ui_mouse); // inherited from global
    }

    #[test]
    fn load_propagates_validation_errors() {
        let repo_dir = tempfile::tempdir().unwrap();
        std::fs::write(repo_config_path(repo_dir.path()), "bogus_key = 1\n").unwrap();
        let e = env(&[("XDG_CONFIG_HOME", "/nonexistent-xyz")]);
        let err = load(Some(repo_dir.path()), &e).unwrap_err();
        assert!(matches!(err, Error::Config { .. }));
    }

    #[test]
    fn load_without_repo_uses_global_only() {
        let global_dir = tempfile::tempdir().unwrap();
        let global_wt = global_dir.path().join("wt");
        std::fs::create_dir_all(&global_wt).unwrap();
        std::fs::write(global_wt.join("config.toml"), "default_base = \"trunk\"\n").unwrap();
        let e = env(&[("XDG_CONFIG_HOME", global_dir.path().to_str().unwrap())]);
        let config = load(None, &e).unwrap();
        assert_eq!(config.default_base.as_deref(), Some("trunk"));
    }
}
