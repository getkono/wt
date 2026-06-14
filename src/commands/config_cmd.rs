//! `wt config <get|set|list|edit>` — read or modify configuration (spec §11).

use std::path::Path;
use std::process::Command;

use toml_edit::{DocumentMut, Item};

use crate::cli::{ConfigAction, ConfigArgs};
use crate::commands::open_session;
use crate::config::{self, Config, repo_config_path};
use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::util::editor::{editor_argv, resolve_editor};

/// All configurable keys, in display order.
const KEYS: &[&str] = &[
    "path_template",
    "default_base",
    "copy",
    "editor",
    "hooks.post_create",
    "hooks.pre_remove",
    "remove.delete_merged_branch",
    "remove.untracked_blocks",
    "pr.default_remote",
    "agent.model",
    "agent.effort",
    "list.show_untracked",
    "list.columns",
    "ui.nerd_fonts",
    "ui.mouse",
    "ui.color",
    "ui.theme.preset",
    "ui.theme.accent",
    "ui.theme.green",
    "ui.theme.red",
    "ui.theme.yellow",
    "ui.theme.orange",
    "ui.theme.cyan",
    "ui.theme.magenta",
    "ui.theme.gray",
    "ui.theme.selection_bg",
    "ui.theme.chip_fg",
];

/// The value type of a settable key.
enum KeyType {
    Str,
    Bool,
    StrArray,
}

/// Dispatches the config action.
pub fn run(cx: &mut Cx, args: &ConfigArgs, json: bool) -> Result<u8> {
    // Determine the config to read and the file to write.
    let (config, file) = if args.global {
        let file = config::global_config_path(&cx.env)
            .ok_or_else(|| Error::operation("cannot determine the global config path"))?;
        (config::load(None, &cx.env)?, file)
    } else {
        let git = cx.git.clone();
        let session = open_session(cx, git.as_ref())?;
        (
            session.config.clone(),
            repo_config_path(&session.primary_root),
        )
    };

    match &args.action {
        ConfigAction::Get { key } => get(cx, &config, key),
        ConfigAction::Set { key, value } => set(cx, &file, key, value),
        ConfigAction::List => list(cx, &config, json),
        ConfigAction::Edit => edit(cx, &config, &file),
    }
}

/// `config get <key>` — print the effective value (empty if unset).
fn get(cx: &mut Cx, config: &Config, key: &str) -> Result<u8> {
    let value = config_value(config, key)?;
    cx.out.line(value.as_deref().unwrap_or(""))?;
    Ok(0)
}

/// `config list` — print all effective keys/values (table or JSON).
fn list(cx: &mut Cx, config: &Config, json: bool) -> Result<u8> {
    if json {
        let mut map = serde_json::Map::new();
        for key in KEYS {
            map.insert((*key).to_string(), config_json_value(config, key));
        }
        cx.out
            .line(&serde_json::to_string(&serde_json::Value::Object(map))?)?;
        return Ok(0);
    }
    for key in KEYS {
        let value = config_value(config, key)?.unwrap_or_default();
        cx.out.line(&format!("{key} = {value}"))?;
    }
    Ok(0)
}

/// `config set <key> <value>` — write the key to the target file, validating.
fn set(cx: &mut Cx, file: &Path, key: &str, value: &str) -> Result<u8> {
    let item = match key_type(key)
        .ok_or_else(|| Error::usage(format!("unknown or non-settable config key: {key}")))?
    {
        KeyType::Str => toml_edit::value(value),
        KeyType::Bool => {
            let parsed = value
                .parse::<bool>()
                .map_err(|_| Error::usage(format!("{key} expects true or false")))?;
            toml_edit::value(parsed)
        }
        KeyType::StrArray => {
            let mut array = toml_edit::Array::new();
            for part in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                array.push(part);
            }
            toml_edit::value(array)
        }
    };

    let text = std::fs::read_to_string(file).unwrap_or_default();
    let mut doc = text.parse::<DocumentMut>().map_err(|e| Error::Config {
        file: file.display().to_string(),
        key: String::new(),
        reason: format!("invalid TOML: {e}"),
    })?;
    set_dotted(&mut doc, key, item)?;

    // Validate the resulting document before writing.
    let rendered = doc.to_string();
    config::parse_layer(&rendered, &file.display().to_string())?;

    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(file, rendered)?;
    cx.err.line(&format!("set {key} = {value}"))?;
    Ok(0)
}

/// `config edit` — open the config file in the editor.
fn edit(cx: &mut Cx, config: &Config, file: &Path) -> Result<u8> {
    let editor = resolve_editor(config.editor.as_deref(), &cx.env)?;
    let argv = editor_argv(&editor);
    let Some((program, rest)) = argv.split_first() else {
        return Err(Error::operation("empty editor command"));
    };
    if !file.exists() {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(file, "")?;
    }
    let status = Command::new(program)
        .args(rest)
        .arg(file)
        .status()
        .map_err(|e| Error::operation(format!("failed to launch editor: {e}")))?;
    if !status.success() {
        return Err(Error::operation("editor exited with an error"));
    }
    Ok(0)
}

/// Sets a dotted `key` in the document, creating intermediate tables.
fn set_dotted(doc: &mut DocumentMut, key: &str, item: Item) -> Result<()> {
    // The leaf is the final dotted component; anything before it names the
    // intermediate tables. `rsplit_once` keeps this total — an undotted key is
    // its own leaf with no parent tables — so there is no empty-key edge case
    // (and thus no panic where `split_last` previously could not fail).
    let (prefix, last) = match key.rsplit_once('.') {
        Some((parents, leaf)) => (parents.split('.').collect::<Vec<&str>>(), leaf),
        None => (Vec::new(), key),
    };
    let mut table = doc.as_table_mut();
    for part in prefix {
        let entry = table
            .entry(part)
            .or_insert_with(|| Item::Table(toml_edit::Table::new()));
        table = entry.as_table_mut().ok_or_else(|| Error::Config {
            file: "config".into(),
            key: key.to_string(),
            reason: format!("{part} is not a table"),
        })?;
    }
    table.insert(last, item);
    Ok(())
}

/// The type of a settable key, or `None` if unknown/non-settable.
fn key_type(key: &str) -> Option<KeyType> {
    Some(match key {
        "path_template"
        | "default_base"
        | "editor"
        | "hooks.post_create"
        | "hooks.pre_remove"
        | "pr.default_remote"
        | "agent.model"
        | "agent.effort"
        | "ui.color"
        | "ui.theme.preset"
        | "ui.theme.accent"
        | "ui.theme.green"
        | "ui.theme.red"
        | "ui.theme.yellow"
        | "ui.theme.orange"
        | "ui.theme.cyan"
        | "ui.theme.magenta"
        | "ui.theme.gray"
        | "ui.theme.selection_bg"
        | "ui.theme.chip_fg" => KeyType::Str,
        "remove.delete_merged_branch"
        | "remove.untracked_blocks"
        | "list.show_untracked"
        | "ui.nerd_fonts"
        | "ui.mouse" => KeyType::Bool,
        "copy" | "list.columns" => KeyType::StrArray,
        _ => return None,
    })
}

/// The effective string value of a key (`None` if unset), or an error for an
/// unknown key.
fn config_value(config: &Config, key: &str) -> Result<Option<String>> {
    Ok(match key {
        "path_template" => Some(config.path_template.clone()),
        "default_base" => config.default_base.clone(),
        "copy" => Some(config.copy.join(", ")),
        "editor" => config.editor.clone(),
        "hooks.post_create" => config.hooks_post_create.clone(),
        "hooks.pre_remove" => config.hooks_pre_remove.clone(),
        "remove.delete_merged_branch" => Some(config.remove_delete_merged_branch.to_string()),
        "remove.untracked_blocks" => Some(config.remove_untracked_blocks.to_string()),
        "pr.default_remote" => Some(config.pr_default_remote.clone()),
        "agent.model" => Some(config.agent_model.id().to_string()),
        "agent.effort" => Some(config.agent_effort.id().to_string()),
        "list.show_untracked" => Some(config.list_show_untracked.to_string()),
        "list.columns" => Some(
            config
                .list_columns
                .iter()
                .map(|c| c.identifier())
                .collect::<Vec<_>>()
                .join(", "),
        ),
        "ui.nerd_fonts" => Some(config.ui_nerd_fonts.to_string()),
        "ui.mouse" => Some(config.ui_mouse.to_string()),
        "ui.color" => Some(color_str(config.ui_color).to_string()),
        // The preset always resolves (default one-dark); per-color overrides are
        // raw — unset reads back empty (like `default_base`).
        "ui.theme.preset" => Some(config.ui_theme.id().to_string()),
        "ui.theme.accent" => config.theme_overrides.accent.map(|c| c.to_string()),
        "ui.theme.green" => config.theme_overrides.green.map(|c| c.to_string()),
        "ui.theme.red" => config.theme_overrides.red.map(|c| c.to_string()),
        "ui.theme.yellow" => config.theme_overrides.yellow.map(|c| c.to_string()),
        "ui.theme.orange" => config.theme_overrides.orange.map(|c| c.to_string()),
        "ui.theme.cyan" => config.theme_overrides.cyan.map(|c| c.to_string()),
        "ui.theme.magenta" => config.theme_overrides.magenta.map(|c| c.to_string()),
        "ui.theme.gray" => config.theme_overrides.gray.map(|c| c.to_string()),
        "ui.theme.selection_bg" => config.theme_overrides.selection_bg.map(|c| c.to_string()),
        "ui.theme.chip_fg" => config.theme_overrides.chip_fg.map(|c| c.to_string()),
        _ => return Err(Error::usage(format!("unknown config key: {key}"))),
    })
}

/// The typed JSON value of a key (for `config list --json`).
fn config_json_value(config: &Config, key: &str) -> serde_json::Value {
    use serde_json::Value;
    match key {
        "copy" => Value::from(config.copy.clone()),
        "list.columns" => Value::from(
            config
                .list_columns
                .iter()
                .map(|c| c.identifier())
                .collect::<Vec<_>>(),
        ),
        "remove.delete_merged_branch" => Value::from(config.remove_delete_merged_branch),
        "remove.untracked_blocks" => Value::from(config.remove_untracked_blocks),
        "list.show_untracked" => Value::from(config.list_show_untracked),
        "ui.nerd_fonts" => Value::from(config.ui_nerd_fonts),
        "ui.mouse" => Value::from(config.ui_mouse),
        _ => match config_value(config, key) {
            Ok(Some(v)) => Value::from(v),
            _ => Value::Null,
        },
    }
}

/// The string form of a color choice.
fn color_str(color: crate::output::color::ColorChoice) -> &'static str {
    use crate::output::color::ColorChoice;
    match color {
        ColorChoice::Auto => "auto",
        ColorChoice::Always => "always",
        ColorChoice::Never => "never",
    }
}

#[cfg(test)]
mod tests {
    use crate::cli::{ConfigAction, ConfigArgs};
    use crate::testutil::TestRepo;

    fn run(
        repo: &TestRepo,
        env: &[(&str, &str)],
        action: ConfigAction,
        global: bool,
        json: bool,
    ) -> (u8, String, String) {
        let mut t = crate::testutil::test_cx(env, repo.root().to_str().unwrap());
        let code = super::run(&mut t.cx, &ConfigArgs { action, global }, json).unwrap();
        (code, t.out.contents(), t.err.contents())
    }

    #[test]
    fn get_default_value() {
        let repo = TestRepo::init();
        let (code, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "pr.default_remote".into(),
            },
            false,
            false,
        );
        assert_eq!(code, 0);
        assert_eq!(out.trim(), "origin");
    }

    #[test]
    fn get_unknown_key_is_usage_error() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(
            &mut t.cx,
            &ConfigArgs {
                action: ConfigAction::Get {
                    key: "bogus".into(),
                },
                global: false,
            },
            false,
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn set_then_get_roundtrip() {
        let repo = TestRepo::init();
        run(
            &repo,
            &[],
            ConfigAction::Set {
                key: "pr.default_remote".into(),
                value: "upstream".into(),
            },
            false,
            false,
        );
        let (_, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "pr.default_remote".into(),
            },
            false,
            false,
        );
        assert_eq!(out.trim(), "upstream");
        // The file is valid TOML with the nested table.
        let content = std::fs::read_to_string(repo.root().join(".wt.toml")).unwrap();
        assert!(content.contains("default_remote = \"upstream\""));
    }

    #[test]
    fn agent_model_and_effort_roundtrip_and_validate() {
        let repo = TestRepo::init();
        // Default value is the resolved Sonnet/medium.
        let (_, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "agent.model".into(),
            },
            false,
            false,
        );
        assert_eq!(out.trim(), "sonnet");
        // Set + get a valid model.
        run(
            &repo,
            &[],
            ConfigAction::Set {
                key: "agent.model".into(),
                value: "opus".into(),
            },
            false,
            false,
        );
        let (_, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "agent.model".into(),
            },
            false,
            false,
        );
        assert_eq!(out.trim(), "opus");
        // An invalid value is rejected at write time (validation re-parses).
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(
            &mut t.cx,
            &ConfigArgs {
                action: ConfigAction::Set {
                    key: "agent.effort".into(),
                    value: "max".into(),
                },
                global: false,
            },
            false,
        )
        .unwrap_err();
        assert!(matches!(err, crate::error::Error::Config { .. }));
    }

    #[test]
    fn set_bool_and_array() {
        let repo = TestRepo::init();
        run(
            &repo,
            &[],
            ConfigAction::Set {
                key: "ui.mouse".into(),
                value: "false".into(),
            },
            false,
            false,
        );
        run(
            &repo,
            &[],
            ConfigAction::Set {
                key: "copy".into(),
                value: ".env, .envrc".into(),
            },
            false,
            false,
        );
        let (_, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get { key: "copy".into() },
            false,
            false,
        );
        assert_eq!(out.trim(), ".env, .envrc");
        let (_, mouse, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "ui.mouse".into(),
            },
            false,
            false,
        );
        assert_eq!(mouse.trim(), "false");
    }

    #[test]
    fn set_rejects_invalid_value() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(
            &mut t.cx,
            &ConfigArgs {
                action: ConfigAction::Set {
                    key: "ui.color".into(),
                    value: "rainbow".into(),
                },
                global: false,
            },
            false,
        )
        .unwrap_err();
        // Validation (parse_layer) rejects the bad color value.
        assert!(matches!(err, crate::error::Error::Config { .. }));
        // Nothing written.
        assert!(!repo.root().join(".wt.toml").exists());
    }

    #[test]
    fn list_outputs_all_keys() {
        let repo = TestRepo::init();
        let (_, out, _) = run(&repo, &[], ConfigAction::List, false, false);
        assert!(out.contains("path_template = "));
        assert!(out.contains("ui.mouse = true"));
        assert!(out.contains("pr.default_remote = origin"));
    }

    #[test]
    fn list_json() {
        let repo = TestRepo::init();
        let (_, out, _) = run(&repo, &[], ConfigAction::List, false, true);
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["ui.mouse"], serde_json::json!(true));
        assert_eq!(v["pr.default_remote"], serde_json::json!("origin"));
        assert!(v["copy"].is_array());
    }

    #[test]
    fn edit_launches_editor() {
        let repo = TestRepo::init();
        // `true` ignores its argument and exits 0, standing in for an editor.
        let (code, _, _) = run(
            &repo,
            &[("EDITOR", "true")],
            ConfigAction::Edit,
            false,
            false,
        );
        assert_eq!(code, 0);
        // The file was created for editing.
        assert!(repo.root().join(".wt.toml").exists());
    }

    #[test]
    fn edit_without_editor_errors() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(
            &mut t.cx,
            &ConfigArgs {
                action: ConfigAction::Edit,
                global: false,
            },
            false,
        )
        .unwrap_err();
        assert!(err.to_string().contains("no editor"));
    }

    #[test]
    fn theme_preset_roundtrip_and_default() {
        let repo = TestRepo::init();
        // The default preset is one-dark.
        let (_, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "ui.theme.preset".into(),
            },
            false,
            false,
        );
        assert_eq!(out.trim(), "one-dark");
        run(
            &repo,
            &[],
            ConfigAction::Set {
                key: "ui.theme.preset".into(),
                value: "solarized".into(),
            },
            false,
            false,
        );
        let (_, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "ui.theme.preset".into(),
            },
            false,
            false,
        );
        assert_eq!(out.trim(), "solarized");
    }

    #[test]
    fn theme_color_override_roundtrip_and_unset_is_empty() {
        let repo = TestRepo::init();
        // An unset override reads back empty (raw-value semantics).
        let (_, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "ui.theme.accent".into(),
            },
            false,
            false,
        );
        assert_eq!(out.trim(), "");
        // A hex color round-trips (canonicalized to uppercase by ratatui).
        run(
            &repo,
            &[],
            ConfigAction::Set {
                key: "ui.theme.accent".into(),
                value: "#ff8800".into(),
            },
            false,
            false,
        );
        let (_, out, _) = run(
            &repo,
            &[],
            ConfigAction::Get {
                key: "ui.theme.accent".into(),
            },
            false,
            false,
        );
        assert_eq!(out.trim(), "#FF8800");
        // The value is written under the nested [ui.theme] table.
        let content = std::fs::read_to_string(repo.root().join(".wt.toml")).unwrap();
        assert!(content.contains("accent = \"#ff8800\""));
    }

    #[test]
    fn theme_set_rejects_invalid_color() {
        let repo = TestRepo::init();
        let mut t = crate::testutil::test_cx(&[], repo.root().to_str().unwrap());
        let err = super::run(
            &mut t.cx,
            &ConfigArgs {
                action: ConfigAction::Set {
                    key: "ui.theme.accent".into(),
                    value: "notacolor".into(),
                },
                global: false,
            },
            false,
        )
        .unwrap_err();
        assert!(matches!(err, crate::error::Error::Config { .. }));
        // Validation runs before the write, so nothing is created.
        assert!(!repo.root().join(".wt.toml").exists());
    }

    #[test]
    fn list_includes_theme_keys() {
        let repo = TestRepo::init();
        let (_, out, _) = run(&repo, &[], ConfigAction::List, false, false);
        assert!(out.contains("ui.theme.preset = one-dark"));
        assert!(out.contains("ui.theme.accent = "));
    }

    #[test]
    fn set_dotted_writes_nested_and_top_level_keys() {
        use toml_edit::{DocumentMut, value};
        let mut doc = DocumentMut::new();
        // A dotted key creates the intermediate table; an undotted key is written
        // at the top level.
        super::set_dotted(&mut doc, "hooks.post_create", value("echo hi")).unwrap();
        super::set_dotted(&mut doc, "editor", value("vim")).unwrap();
        assert_eq!(doc["hooks"]["post_create"].as_str(), Some("echo hi"));
        assert_eq!(doc["editor"].as_str(), Some("vim"));
    }
}
