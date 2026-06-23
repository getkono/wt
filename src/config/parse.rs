//! Parsing and validation of a single config file into a [`ConfigLayer`]
//! (spec §11). Validation runs on every invocation: unknown keys, invalid
//! `list.columns` identifiers, and bad `ui.keybindings` action names or key
//! strings are rejected with a precise `{file, key, reason}` error.

use ratatui::style::Color;
use toml::Value;

use crate::agent::{AgentModel, Effort};
use crate::config::schema::{ConfigLayer, SubmoduleInit};
use crate::error::{Error, Result};
use crate::keys::{KeyAction, KeyChord};
use crate::model::Column;
use crate::output::color::ColorChoice;
use crate::tui::theme::ThemePreset;

/// Builds a configuration error with file/key/reason context.
fn cfg_err(file: &str, key: &str, reason: impl Into<String>) -> Error {
    Error::Config {
        file: file.to_string(),
        key: key.to_string(),
        reason: reason.into(),
    }
}

/// Reads a string value, or errors with the expected type.
fn as_string(file: &str, key: &str, value: &Value) -> Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| cfg_err(file, key, "expected a string"))
}

/// Reads a boolean value, or errors with the expected type.
fn as_bool(file: &str, key: &str, value: &Value) -> Result<bool> {
    value
        .as_bool()
        .ok_or_else(|| cfg_err(file, key, "expected a boolean"))
}

/// Reads an array of strings, or errors with the expected type.
fn as_string_array(file: &str, key: &str, value: &Value) -> Result<Vec<String>> {
    let array = value
        .as_array()
        .ok_or_else(|| cfg_err(file, key, "expected an array of strings"))?;
    array
        .iter()
        .map(|item| as_string(file, key, item))
        .collect()
}

/// Reads a sub-table, or errors with the expected type.
fn as_table<'a>(file: &str, key: &str, value: &'a Value) -> Result<&'a toml::Table> {
    value
        .as_table()
        .ok_or_else(|| cfg_err(file, key, "expected a table"))
}

/// Parses and validates a config file's text into a [`ConfigLayer`].
pub fn parse_layer(text: &str, file: &str) -> Result<ConfigLayer> {
    let value: Value =
        toml::from_str(text).map_err(|e| cfg_err(file, "", format!("invalid TOML: {e}")))?;
    let table = as_table(file, "", &value)?;
    let mut layer = ConfigLayer::default();
    for (key, val) in table {
        match key.as_str() {
            "path_template" => layer.path_template = Some(as_string(file, key, val)?),
            "default_base" => layer.default_base = Some(as_string(file, key, val)?),
            "copy" => layer.copy = Some(as_string_array(file, key, val)?),
            "editor" => layer.editor = Some(as_string(file, key, val)?),
            "hooks" => parse_hooks(file, val, &mut layer)?,
            "remove" => parse_remove(file, val, &mut layer)?,
            "pr" => parse_pr(file, val, &mut layer)?,
            "submodules" => parse_submodules(file, val, &mut layer)?,
            "agent" => parse_agent(file, val, &mut layer)?,
            "list" => parse_list(file, val, &mut layer)?,
            "ui" => parse_ui(file, val, &mut layer)?,
            other => return Err(cfg_err(file, other, "unknown configuration key")),
        }
    }
    Ok(layer)
}

/// Parses the `[hooks]` table.
fn parse_hooks(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    for (sub, val) in as_table(file, "hooks", value)? {
        let key = format!("hooks.{sub}");
        match sub.as_str() {
            "post_create" => layer.hooks_post_create = Some(as_string(file, &key, val)?),
            "pre_remove" => layer.hooks_pre_remove = Some(as_string(file, &key, val)?),
            _ => return Err(cfg_err(file, &key, "unknown configuration key")),
        }
    }
    Ok(())
}

/// Parses the `[remove]` table.
fn parse_remove(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    for (sub, val) in as_table(file, "remove", value)? {
        let key = format!("remove.{sub}");
        match sub.as_str() {
            "delete_merged_branch" => {
                layer.remove_delete_merged_branch = Some(as_bool(file, &key, val)?);
            }
            "untracked_blocks" => {
                layer.remove_untracked_blocks = Some(as_bool(file, &key, val)?);
            }
            _ => return Err(cfg_err(file, &key, "unknown configuration key")),
        }
    }
    Ok(())
}

/// Parses the `[pr]` table.
fn parse_pr(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    for (sub, val) in as_table(file, "pr", value)? {
        let key = format!("pr.{sub}");
        match sub.as_str() {
            "default_remote" => layer.pr_default_remote = Some(as_string(file, &key, val)?),
            _ => return Err(cfg_err(file, &key, "unknown configuration key")),
        }
    }
    Ok(())
}

/// Parses the `[submodules]` table, validating the `init` policy (issue #50).
fn parse_submodules(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    for (sub, val) in as_table(file, "submodules", value)? {
        let key = format!("submodules.{sub}");
        match sub.as_str() {
            "init" => {
                let text = as_string(file, &key, val)?;
                let policy = SubmoduleInit::parse(&text)
                    .ok_or_else(|| cfg_err(file, &key, "expected one of: prompt, never, always"))?;
                layer.submodules_init = Some(policy);
            }
            _ => return Err(cfg_err(file, &key, "unknown configuration key")),
        }
    }
    Ok(())
}

/// Parses the `[agent]` table, validating the model and effort identifiers.
fn parse_agent(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    for (sub, val) in as_table(file, "agent", value)? {
        let key = format!("agent.{sub}");
        match sub.as_str() {
            "model" => {
                let text = as_string(file, &key, val)?;
                let model = AgentModel::parse(&text)
                    .ok_or_else(|| cfg_err(file, &key, "expected one of: opus, sonnet, haiku"))?;
                layer.agent_model = Some(model);
            }
            "effort" => {
                let text = as_string(file, &key, val)?;
                let effort = Effort::parse(&text)
                    .ok_or_else(|| cfg_err(file, &key, "expected one of: low, medium, high"))?;
                layer.agent_effort = Some(effort);
            }
            _ => return Err(cfg_err(file, &key, "unknown configuration key")),
        }
    }
    Ok(())
}

/// Parses the `[list]` table, validating column identifiers.
fn parse_list(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    for (sub, val) in as_table(file, "list", value)? {
        let key = format!("list.{sub}");
        match sub.as_str() {
            "show_untracked" => layer.list_show_untracked = Some(as_bool(file, &key, val)?),
            "columns" => {
                let names = as_string_array(file, &key, val)?;
                let mut columns = Vec::with_capacity(names.len());
                for name in names {
                    let column = Column::parse(&name).ok_or_else(|| {
                        cfg_err(file, &key, format!("unknown column identifier: {name:?}"))
                    })?;
                    columns.push(column);
                }
                layer.list_columns = Some(columns);
            }
            _ => return Err(cfg_err(file, &key, "unknown configuration key")),
        }
    }
    Ok(())
}

/// Parses the `[ui]` table, including `ui.color` and `ui.keybindings`.
fn parse_ui(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    for (sub, val) in as_table(file, "ui", value)? {
        let key = format!("ui.{sub}");
        match sub.as_str() {
            "nerd_fonts" => layer.ui_nerd_fonts = Some(as_bool(file, &key, val)?),
            "mouse" => layer.ui_mouse = Some(as_bool(file, &key, val)?),
            "color" => {
                let text = as_string(file, &key, val)?;
                let choice = ColorChoice::parse(&text)
                    .ok_or_else(|| cfg_err(file, &key, "expected one of: auto, always, never"))?;
                layer.ui_color = Some(choice);
            }
            "theme" => parse_theme(file, val, layer)?,
            "keybindings" => parse_keybindings(file, val, layer)?,
            _ => return Err(cfg_err(file, &key, "unknown configuration key")),
        }
    }
    Ok(())
}

/// Parses `ui.theme`: either a string shorthand selecting a preset
/// (`theme = "solarized"`) or a `[ui.theme]` table with a `preset` key and
/// per-color overrides.
fn parse_theme(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    // String shorthand selects the base preset.
    if let Some(name) = value.as_str() {
        let preset = ThemePreset::parse(name)
            .ok_or_else(|| cfg_err(file, "ui.theme", "expected one of: one-dark, solarized"))?;
        layer.ui_theme = Some(preset);
        return Ok(());
    }
    let o = &mut layer.theme_overrides;
    for (sub, val) in as_table(file, "ui.theme", value)? {
        let key = format!("ui.theme.{sub}");
        match sub.as_str() {
            "preset" => {
                let text = as_string(file, &key, val)?;
                let preset = ThemePreset::parse(&text)
                    .ok_or_else(|| cfg_err(file, &key, "expected one of: one-dark, solarized"))?;
                layer.ui_theme = Some(preset);
            }
            "accent" => o.accent = Some(parse_color(file, &key, val)?),
            "green" => o.green = Some(parse_color(file, &key, val)?),
            "red" => o.red = Some(parse_color(file, &key, val)?),
            "yellow" => o.yellow = Some(parse_color(file, &key, val)?),
            "orange" => o.orange = Some(parse_color(file, &key, val)?),
            "cyan" => o.cyan = Some(parse_color(file, &key, val)?),
            "magenta" => o.magenta = Some(parse_color(file, &key, val)?),
            "gray" => o.gray = Some(parse_color(file, &key, val)?),
            "selection_bg" => o.selection_bg = Some(parse_color(file, &key, val)?),
            "chip_fg" => o.chip_fg = Some(parse_color(file, &key, val)?),
            _ => return Err(cfg_err(file, &key, "unknown configuration key")),
        }
    }
    Ok(())
}

/// Parses a color string: `#rrggbb` hex, a named color (e.g. `cyan`,
/// `light-blue`), or a 0–255 ANSI index, via ratatui's [`Color`] parser.
fn parse_color(file: &str, key: &str, value: &Value) -> Result<Color> {
    let text = as_string(file, key, value)?;
    text.parse::<Color>()
        .map_err(|_| cfg_err(file, key, format!("invalid color: {text:?}")))
}

/// Parses the `[ui.keybindings]` table (action name → key string).
fn parse_keybindings(file: &str, value: &Value, layer: &mut ConfigLayer) -> Result<()> {
    for (action_name, val) in as_table(file, "ui.keybindings", value)? {
        let key = format!("ui.keybindings.{action_name}");
        let action = KeyAction::parse(action_name)
            .ok_or_else(|| cfg_err(file, &key, "unknown keybinding action"))?;
        let key_string = as_string(file, &key, val)?;
        let chord = KeyChord::parse(&key_string)
            .ok_or_else(|| cfg_err(file, &key, format!("invalid key string: {key_string:?}")))?;
        layer.ui_keybindings.push((action, chord));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    fn parse(text: &str) -> Result<ConfigLayer> {
        parse_layer(text, "test.toml")
    }

    fn config_reason(err: Error) -> (String, String) {
        match err {
            Error::Config { key, reason, .. } => (key, reason),
            other => panic!("expected config error, got {other:?}"),
        }
    }

    #[test]
    fn parses_a_full_valid_file() {
        let text = r#"
            path_template = "{home}/wt/{branch_slug}"
            default_base = "develop"
            copy = [".env", ".envrc"]
            editor = "nvim"

            [hooks]
            post_create = "direnv allow"
            pre_remove = "echo bye"

            [remove]
            delete_merged_branch = false
            untracked_blocks = true

            [pr]
            default_remote = "upstream"

            [submodules]
            init = "always"

            [agent]
            model = "opus"
            effort = "high"

            [list]
            show_untracked = false
            columns = ["branch", "pr"]

            [ui]
            nerd_fonts = true
            mouse = false
            color = "always"

            [ui.keybindings]
            quit = "ctrl+c"
            navigate-up = "w"
        "#;
        let layer = parse(text).unwrap();
        assert_eq!(
            layer.path_template.as_deref(),
            Some("{home}/wt/{branch_slug}")
        );
        assert_eq!(layer.default_base.as_deref(), Some("develop"));
        assert_eq!(layer.copy, Some(vec![".env".into(), ".envrc".into()]));
        assert_eq!(layer.editor.as_deref(), Some("nvim"));
        assert_eq!(layer.hooks_post_create.as_deref(), Some("direnv allow"));
        assert_eq!(layer.hooks_pre_remove.as_deref(), Some("echo bye"));
        assert_eq!(layer.remove_delete_merged_branch, Some(false));
        assert_eq!(layer.remove_untracked_blocks, Some(true));
        assert_eq!(layer.pr_default_remote.as_deref(), Some("upstream"));
        assert_eq!(layer.submodules_init, Some(SubmoduleInit::Always));
        assert_eq!(layer.agent_model, Some(AgentModel::Opus));
        assert_eq!(layer.agent_effort, Some(Effort::High));
        assert_eq!(layer.list_show_untracked, Some(false));
        assert_eq!(layer.list_columns, Some(vec![Column::Branch, Column::Pr]));
        assert_eq!(layer.ui_nerd_fonts, Some(true));
        assert_eq!(layer.ui_mouse, Some(false));
        assert_eq!(layer.ui_color, Some(ColorChoice::Always));
        assert_eq!(
            layer.ui_keybindings,
            vec![
                (KeyAction::NavigateUp, KeyChord::key(KeyCode::Char('w'))),
                (KeyAction::Quit, KeyChord::ctrl('c')),
            ]
        );
    }

    #[test]
    fn empty_file_is_empty_layer() {
        assert_eq!(parse("").unwrap(), ConfigLayer::default());
    }

    #[test]
    fn unknown_top_level_key_rejected() {
        let (key, reason) = config_reason(parse("bogus = 1").unwrap_err());
        assert_eq!(key, "bogus");
        assert!(reason.contains("unknown"));
    }

    #[test]
    fn unknown_nested_key_rejected_with_dotted_path() {
        let (key, _) = config_reason(parse("[ui]\nwiggle = true").unwrap_err());
        assert_eq!(key, "ui.wiggle");
        let (key, _) = config_reason(parse("[hooks]\nmid = \"x\"").unwrap_err());
        assert_eq!(key, "hooks.mid");
    }

    #[test]
    fn type_mismatches_rejected() {
        assert!(parse("path_template = 5").is_err());
        assert!(parse("[ui]\nmouse = \"yes\"").is_err());
        assert!(parse("copy = \"single\"").is_err());
        let (key, reason) = config_reason(parse("[remove]\nuntracked_blocks = 1").unwrap_err());
        assert_eq!(key, "remove.untracked_blocks");
        assert!(reason.contains("boolean"));
    }

    #[test]
    fn invalid_column_identifier_rejected() {
        let (key, reason) =
            config_reason(parse("[list]\ncolumns = [\"branch\", \"bogus\"]").unwrap_err());
        assert_eq!(key, "list.columns");
        assert!(reason.contains("bogus"));
    }

    #[test]
    fn invalid_color_rejected() {
        let (key, _) = config_reason(parse("[ui]\ncolor = \"rainbow\"").unwrap_err());
        assert_eq!(key, "ui.color");
    }

    #[test]
    fn invalid_agent_model_and_effort_rejected() {
        let (key, reason) = config_reason(parse("[agent]\nmodel = \"gpt\"").unwrap_err());
        assert_eq!(key, "agent.model");
        assert!(reason.contains("opus, sonnet, haiku"));
        let (key, reason) = config_reason(parse("[agent]\neffort = \"max\"").unwrap_err());
        assert_eq!(key, "agent.effort");
        assert!(reason.contains("low, medium, high"));
        let (key, _) = config_reason(parse("[agent]\nwiggle = true").unwrap_err());
        assert_eq!(key, "agent.wiggle");
    }

    #[test]
    fn submodules_init_parses_and_validates() {
        assert_eq!(
            parse("[submodules]\ninit = \"never\"")
                .unwrap()
                .submodules_init,
            Some(SubmoduleInit::Never)
        );
        assert_eq!(
            parse("[submodules]\ninit = \"prompt\"")
                .unwrap()
                .submodules_init,
            Some(SubmoduleInit::Prompt)
        );
        let (key, reason) = config_reason(parse("[submodules]\ninit = \"sometimes\"").unwrap_err());
        assert_eq!(key, "submodules.init");
        assert!(reason.contains("prompt, never, always"));
        let (key, _) = config_reason(parse("[submodules]\nwiggle = true").unwrap_err());
        assert_eq!(key, "submodules.wiggle");
    }

    #[test]
    fn invalid_keybinding_action_and_key_rejected() {
        let (key, reason) = config_reason(parse("[ui.keybindings]\nfly = \"x\"").unwrap_err());
        assert_eq!(key, "ui.keybindings.fly");
        assert!(reason.contains("unknown keybinding action"));
        let (key, reason) =
            config_reason(parse("[ui.keybindings]\nquit = \"nope+z\"").unwrap_err());
        assert_eq!(key, "ui.keybindings.quit");
        assert!(reason.contains("invalid key string"));
    }

    #[test]
    fn malformed_toml_is_config_error() {
        let (_, reason) = config_reason(parse("this is not = = toml").unwrap_err());
        assert!(reason.contains("invalid TOML"));
    }

    #[test]
    fn parses_theme_table_with_preset_and_overrides() {
        let layer =
            parse("[ui.theme]\npreset = \"solarized\"\naccent = \"#ff8800\"\nred = \"red\"")
                .unwrap();
        assert_eq!(layer.ui_theme, Some(ThemePreset::Solarized));
        assert_eq!(
            layer.theme_overrides.accent,
            Some(Color::Rgb(0xff, 0x88, 0x00))
        );
        assert_eq!(layer.theme_overrides.red, Some(Color::Red));
        // Untouched slots stay None.
        assert_eq!(layer.theme_overrides.green, None);
    }

    #[test]
    fn parses_theme_string_shorthand() {
        let layer = parse("[ui]\ntheme = \"solarized\"").unwrap();
        assert_eq!(layer.ui_theme, Some(ThemePreset::Solarized));
        assert_eq!(layer.theme_overrides, Default::default());
    }

    #[test]
    fn invalid_theme_preset_rejected() {
        let (key, reason) = config_reason(parse("[ui.theme]\npreset = \"dracula\"").unwrap_err());
        assert_eq!(key, "ui.theme.preset");
        assert!(reason.contains("one-dark, solarized"));
        // The string shorthand validates the preset too.
        let (key, _) = config_reason(parse("[ui]\ntheme = \"dracula\"").unwrap_err());
        assert_eq!(key, "ui.theme");
    }

    #[test]
    fn invalid_theme_color_rejected() {
        let (key, reason) = config_reason(parse("[ui.theme]\naccent = \"notacolor\"").unwrap_err());
        assert_eq!(key, "ui.theme.accent");
        assert!(reason.contains("invalid color"));
    }

    #[test]
    fn unknown_theme_key_rejected() {
        let (key, _) = config_reason(parse("[ui.theme]\nsparkle = \"#fff\"").unwrap_err());
        assert_eq!(key, "ui.theme.sparkle");
    }
}
