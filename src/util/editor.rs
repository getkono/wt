//! Editor resolution (spec §10/§11): the `editor` config key, then `$VISUAL`,
//! then `$EDITOR`.

use crate::cx::Env;
use crate::error::{Error, Result};

/// Resolves the editor command, erroring if none is configured.
pub fn resolve_editor(config_editor: Option<&str>, env: &Env) -> Result<String> {
    if let Some(editor) = config_editor.filter(|e| !e.is_empty()) {
        return Ok(editor.to_string());
    }
    if let Some(visual) = env.get("VISUAL").filter(|v| !v.is_empty()) {
        return Ok(visual.to_string());
    }
    if let Some(editor) = env.get("EDITOR").filter(|e| !e.is_empty()) {
        return Ok(editor.to_string());
    }
    Err(Error::operation(
        "no editor configured; set the `editor` config key, $VISUAL, or $EDITOR",
    ))
}

/// Splits an editor command into argv (honoring quotes).
pub fn editor_argv(command: &str) -> Vec<String> {
    shell_words::split(command).unwrap_or_else(|_| vec![command.to_string()])
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
    fn config_editor_wins() {
        let e = env(&[("VISUAL", "vim"), ("EDITOR", "nano")]);
        assert_eq!(resolve_editor(Some("hx"), &e).unwrap(), "hx");
    }

    #[test]
    fn falls_back_to_visual_then_editor() {
        let e = env(&[("VISUAL", "vim"), ("EDITOR", "nano")]);
        assert_eq!(resolve_editor(None, &e).unwrap(), "vim");
        let e2 = env(&[("EDITOR", "nano")]);
        assert_eq!(resolve_editor(None, &e2).unwrap(), "nano");
    }

    #[test]
    fn empty_values_are_skipped() {
        let e = env(&[("VISUAL", ""), ("EDITOR", "nano")]);
        assert_eq!(resolve_editor(Some(""), &e).unwrap(), "nano");
    }

    #[test]
    fn errors_when_unset() {
        assert!(resolve_editor(None, &env(&[])).is_err());
    }

    #[test]
    fn argv_splits_quoted_command() {
        assert_eq!(editor_argv("code --wait"), vec!["code", "--wait"]);
        assert_eq!(editor_argv("\"my editor\" -n"), vec!["my editor", "-n"]);
    }
}
