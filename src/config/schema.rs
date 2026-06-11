//! The resolved [`Config`] and the per-layer [`ConfigLayer`], plus the merge
//! semantics (spec §11).

use ratatui::style::Color;

use crate::agent::{AgentModel, Effort};
use crate::cx::Env;
use crate::keys::{KeyAction, KeyChord, Keymap};
use crate::model::Column;
use crate::output::color::{ColorChoice, resolve_color};
use crate::template::DEFAULT_TEMPLATE;
use crate::tui::theme::{Palette, ThemePreset};

/// When to initialize git submodules after a worktree is created or a branch is
/// checked out (`[submodules] init`, issue #50). Opt-in: the default never
/// touches submodules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SubmoduleInit {
    /// Never initialize submodules automatically (the default).
    #[default]
    Never,
    /// Always run `git submodule update --init --recursive` when uninitialized
    /// submodules are present.
    Always,
}

impl SubmoduleInit {
    /// Parses a `submodules.init` value (`never`, `always`).
    pub fn parse(value: &str) -> Option<SubmoduleInit> {
        match value {
            "never" => Some(SubmoduleInit::Never),
            "always" => Some(SubmoduleInit::Always),
            _ => None,
        }
    }
}

/// The fully-resolved configuration after merging all layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Worktree-store path template (spec §6).
    pub path_template: String,
    /// Base ref for `new` when a branch is created; `None` resolves the repo's
    /// default branch at runtime.
    pub default_base: Option<String>,
    /// Glob patterns to copy into new worktrees (spec §8).
    pub copy: Vec<String>,
    /// Shell command run after worktree creation.
    pub hooks_post_create: Option<String>,
    /// Shell command run before worktree removal.
    pub hooks_pre_remove: Option<String>,
    /// Editor command; `None` falls back to `$VISUAL`/`$EDITOR`.
    pub editor: Option<String>,
    /// Delete a wt-created branch on `remove` if fully merged.
    pub remove_delete_merged_branch: bool,
    /// Whether untracked files count as dirty for remove/prune guards.
    pub remove_untracked_blocks: bool,
    /// Remote used for PR fetches.
    pub pr_default_remote: String,
    /// When to auto-initialize git submodules on create/checkout (issue #50).
    pub submodules_init: SubmoduleInit,
    /// Default model for the AI PR auto-fill (`wt pr open --ai`); overridable
    /// per-invocation by `--model` or the TUI's `Ctrl-M` key.
    pub agent_model: AgentModel,
    /// Default effort for the AI PR auto-fill; overridable by `--effort` or the
    /// TUI's `Ctrl-E` key.
    pub agent_effort: Effort,
    /// Show `?` in the dirty column for untracked files.
    pub list_show_untracked: bool,
    /// Ordered list of columns to display in `wt list`.
    pub list_columns: Vec<Column>,
    /// Enable Nerd Font glyphs in the TUI.
    pub ui_nerd_fonts: bool,
    /// Enable mouse support in the TUI.
    pub ui_mouse: bool,
    /// Color output setting (reconciled with `--color`/`NO_COLOR`).
    pub ui_color: ColorChoice,
    /// Built-in theme preset (the base TUI palette).
    pub ui_theme: ThemePreset,
    /// Per-color overrides layered on top of the preset (`[ui.theme]`).
    pub theme_overrides: ThemeOverrides,
    /// Accumulated `ui.keybindings` overrides (applied over the defaults).
    pub keybinding_overrides: Vec<(KeyAction, KeyChord)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            path_template: DEFAULT_TEMPLATE.to_string(),
            default_base: None,
            copy: Vec::new(),
            hooks_post_create: None,
            hooks_pre_remove: None,
            editor: None,
            remove_delete_merged_branch: true,
            remove_untracked_blocks: false,
            pr_default_remote: "origin".to_string(),
            submodules_init: SubmoduleInit::default(),
            agent_model: AgentModel::default(),
            agent_effort: Effort::default(),
            list_show_untracked: true,
            list_columns: Column::ALL.to_vec(),
            ui_nerd_fonts: false,
            ui_mouse: true,
            ui_color: ColorChoice::Auto,
            ui_theme: ThemePreset::default(),
            theme_overrides: ThemeOverrides::default(),
            keybinding_overrides: Vec::new(),
        }
    }
}

impl Config {
    /// Applies a parsed layer on top of this config (spec §11 merge semantics):
    /// scalars replace, arrays (`copy`, `list.columns`) replace wholesale,
    /// `ui.keybindings` deep-merges per action, and the `[ui.theme]` colors
    /// deep-merge per slot (the `preset` is a scalar). Overrides accumulate in
    /// apply order, so a later layer wins.
    pub fn apply(&mut self, layer: ConfigLayer) {
        if let Some(v) = layer.path_template {
            self.path_template = v;
        }
        if let Some(v) = layer.default_base {
            self.default_base = Some(v);
        }
        if let Some(v) = layer.copy {
            self.copy = v;
        }
        if let Some(v) = layer.editor {
            self.editor = Some(v);
        }
        if let Some(v) = layer.hooks_post_create {
            self.hooks_post_create = Some(v);
        }
        if let Some(v) = layer.hooks_pre_remove {
            self.hooks_pre_remove = Some(v);
        }
        if let Some(v) = layer.remove_delete_merged_branch {
            self.remove_delete_merged_branch = v;
        }
        if let Some(v) = layer.remove_untracked_blocks {
            self.remove_untracked_blocks = v;
        }
        if let Some(v) = layer.pr_default_remote {
            self.pr_default_remote = v;
        }
        if let Some(v) = layer.submodules_init {
            self.submodules_init = v;
        }
        if let Some(v) = layer.agent_model {
            self.agent_model = v;
        }
        if let Some(v) = layer.agent_effort {
            self.agent_effort = v;
        }
        if let Some(v) = layer.list_show_untracked {
            self.list_show_untracked = v;
        }
        if let Some(v) = layer.list_columns {
            self.list_columns = v;
        }
        if let Some(v) = layer.ui_nerd_fonts {
            self.ui_nerd_fonts = v;
        }
        if let Some(v) = layer.ui_mouse {
            self.ui_mouse = v;
        }
        if let Some(v) = layer.ui_color {
            self.ui_color = v;
        }
        if let Some(v) = layer.ui_theme {
            self.ui_theme = v;
        }
        self.theme_overrides.merge(layer.theme_overrides);
        self.keybinding_overrides.extend(layer.ui_keybindings);
    }

    /// Resolves the effective TUI [`Palette`]: the selected preset's base palette
    /// with any `[ui.theme]` per-color overrides applied on top.
    pub fn palette(&self) -> Palette {
        let mut palette = self.ui_theme.palette();
        self.theme_overrides.apply_to(&mut palette);
        palette
    }

    /// Builds the effective TUI keymap: the defaults with the configured
    /// overrides applied in order.
    pub fn keymap(&self) -> Keymap {
        let mut keymap = Keymap::defaults();
        for (action, chord) in &self.keybinding_overrides {
            keymap.rebind(*action, *chord);
        }
        keymap
    }

    /// Resolves whether to emit color, reconciling the `--color` flag, the
    /// `NO_COLOR` env var, and `ui.color` (spec §11 precedence).
    pub fn color_enabled(&self, flag: Option<ColorChoice>, env: &Env, stdout_is_tty: bool) -> bool {
        resolve_color(
            flag,
            env.is_set_nonempty("NO_COLOR"),
            Some(self.ui_color),
            stdout_is_tty,
        )
    }
}

/// One configuration layer (a single file's settings, or flags); every field is
/// optional and only present keys override lower layers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigLayer {
    /// `path_template`.
    pub path_template: Option<String>,
    /// `default_base`.
    pub default_base: Option<String>,
    /// `copy`.
    pub copy: Option<Vec<String>>,
    /// `editor`.
    pub editor: Option<String>,
    /// `hooks.post_create`.
    pub hooks_post_create: Option<String>,
    /// `hooks.pre_remove`.
    pub hooks_pre_remove: Option<String>,
    /// `remove.delete_merged_branch`.
    pub remove_delete_merged_branch: Option<bool>,
    /// `remove.untracked_blocks`.
    pub remove_untracked_blocks: Option<bool>,
    /// `pr.default_remote`.
    pub pr_default_remote: Option<String>,
    /// `submodules.init`.
    pub submodules_init: Option<SubmoduleInit>,
    /// `agent.model`.
    pub agent_model: Option<AgentModel>,
    /// `agent.effort`.
    pub agent_effort: Option<Effort>,
    /// `list.show_untracked`.
    pub list_show_untracked: Option<bool>,
    /// `list.columns`.
    pub list_columns: Option<Vec<Column>>,
    /// `ui.nerd_fonts`.
    pub ui_nerd_fonts: Option<bool>,
    /// `ui.mouse`.
    pub ui_mouse: Option<bool>,
    /// `ui.color`.
    pub ui_color: Option<ColorChoice>,
    /// `ui.theme.preset`.
    pub ui_theme: Option<ThemePreset>,
    /// `[ui.theme]` per-color overrides present in this layer.
    pub theme_overrides: ThemeOverrides,
    /// `ui.keybindings` (action → chord) overrides.
    pub ui_keybindings: Vec<(KeyAction, KeyChord)>,
}

/// Per-color overrides for the TUI palette (`[ui.theme]`). Each field mirrors a
/// [`Palette`] slot; `None` leaves the preset's color untouched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThemeOverrides {
    /// `ui.theme.accent`.
    pub accent: Option<Color>,
    /// `ui.theme.green`.
    pub green: Option<Color>,
    /// `ui.theme.red`.
    pub red: Option<Color>,
    /// `ui.theme.yellow`.
    pub yellow: Option<Color>,
    /// `ui.theme.orange`.
    pub orange: Option<Color>,
    /// `ui.theme.cyan`.
    pub cyan: Option<Color>,
    /// `ui.theme.magenta`.
    pub magenta: Option<Color>,
    /// `ui.theme.gray`.
    pub gray: Option<Color>,
    /// `ui.theme.selection_bg`.
    pub selection_bg: Option<Color>,
    /// `ui.theme.chip_fg`.
    pub chip_fg: Option<Color>,
}

impl ThemeOverrides {
    /// Merges another layer's overrides on top of these (set slots win).
    pub fn merge(&mut self, other: ThemeOverrides) {
        self.accent = other.accent.or(self.accent);
        self.green = other.green.or(self.green);
        self.red = other.red.or(self.red);
        self.yellow = other.yellow.or(self.yellow);
        self.orange = other.orange.or(self.orange);
        self.cyan = other.cyan.or(self.cyan);
        self.magenta = other.magenta.or(self.magenta);
        self.gray = other.gray.or(self.gray);
        self.selection_bg = other.selection_bg.or(self.selection_bg);
        self.chip_fg = other.chip_fg.or(self.chip_fg);
    }

    /// Applies the set overrides onto a base [`Palette`].
    fn apply_to(&self, palette: &mut Palette) {
        if let Some(c) = self.accent {
            palette.accent = c;
        }
        if let Some(c) = self.green {
            palette.green = c;
        }
        if let Some(c) = self.red {
            palette.red = c;
        }
        if let Some(c) = self.yellow {
            palette.yellow = c;
        }
        if let Some(c) = self.orange {
            palette.orange = c;
        }
        if let Some(c) = self.cyan {
            palette.cyan = c;
        }
        if let Some(c) = self.magenta {
            palette.magenta = c;
        }
        if let Some(c) = self.gray {
            palette.gray = c;
        }
        if let Some(c) = self.selection_bg {
            palette.selection_bg = c;
        }
        if let Some(c) = self.chip_fg {
            palette.chip_fg = c;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.path_template, DEFAULT_TEMPLATE);
        assert!(c.default_base.is_none());
        assert!(c.copy.is_empty());
        assert!(c.remove_delete_merged_branch);
        assert!(!c.remove_untracked_blocks);
        assert_eq!(c.pr_default_remote, "origin");
        assert_eq!(c.submodules_init, SubmoduleInit::Never);
        assert_eq!(c.agent_model, AgentModel::Sonnet);
        assert_eq!(c.agent_effort, Effort::Medium);
        assert!(c.list_show_untracked);
        assert_eq!(c.list_columns, Column::ALL.to_vec());
        assert!(!c.ui_nerd_fonts);
        assert!(c.ui_mouse);
        assert_eq!(c.ui_color, ColorChoice::Auto);
    }

    #[test]
    fn scalars_replace_on_apply() {
        let mut c = Config::default();
        c.apply(ConfigLayer {
            pr_default_remote: Some("upstream".into()),
            ui_mouse: Some(false),
            ..Default::default()
        });
        assert_eq!(c.pr_default_remote, "upstream");
        assert!(!c.ui_mouse);
        // Untouched fields keep their defaults.
        assert!(c.list_show_untracked);
    }

    #[test]
    fn arrays_replace_wholesale() {
        let mut c = Config::default();
        c.apply(ConfigLayer {
            copy: Some(vec![".env".into()]),
            list_columns: Some(vec![Column::Branch, Column::Pr]),
            ..Default::default()
        });
        assert_eq!(c.copy, vec![".env".to_string()]);
        assert_eq!(c.list_columns, vec![Column::Branch, Column::Pr]);
        // A second layer replaces, not concatenates.
        c.apply(ConfigLayer {
            copy: Some(vec![".envrc".into()]),
            ..Default::default()
        });
        assert_eq!(c.copy, vec![".envrc".to_string()]);
    }

    #[test]
    fn apply_sets_every_scalar_and_optional_field() {
        let mut c = Config::default();
        c.apply(ConfigLayer {
            path_template: Some("{home}/{branch_slug}".into()),
            default_base: Some("trunk".into()),
            editor: Some("hx".into()),
            hooks_post_create: Some("setup".into()),
            hooks_pre_remove: Some("teardown".into()),
            remove_delete_merged_branch: Some(false),
            remove_untracked_blocks: Some(true),
            submodules_init: Some(SubmoduleInit::Always),
            agent_model: Some(AgentModel::Haiku),
            agent_effort: Some(Effort::Low),
            list_show_untracked: Some(false),
            ui_nerd_fonts: Some(true),
            ui_color: Some(ColorChoice::Never),
            ..Default::default()
        });
        assert_eq!(c.path_template, "{home}/{branch_slug}");
        assert_eq!(c.default_base.as_deref(), Some("trunk"));
        assert_eq!(c.editor.as_deref(), Some("hx"));
        assert_eq!(c.hooks_post_create.as_deref(), Some("setup"));
        assert_eq!(c.hooks_pre_remove.as_deref(), Some("teardown"));
        assert!(!c.remove_delete_merged_branch);
        assert!(c.remove_untracked_blocks);
        assert_eq!(c.submodules_init, SubmoduleInit::Always);
        assert_eq!(c.agent_model, AgentModel::Haiku);
        assert_eq!(c.agent_effort, Effort::Low);
        assert!(!c.list_show_untracked);
        assert!(c.ui_nerd_fonts);
        assert_eq!(c.ui_color, ColorChoice::Never);
    }

    #[test]
    fn color_enabled_follows_precedence() {
        use crate::output::color::ColorChoice;
        let mut c = Config::default();
        let no_env = Env::from_map(std::collections::HashMap::new());
        // Default ui.color=auto -> follows stdout TTY.
        assert!(c.color_enabled(None, &no_env, true));
        assert!(!c.color_enabled(None, &no_env, false));
        // ui.color=never overrides auto.
        c.ui_color = ColorChoice::Never;
        assert!(!c.color_enabled(None, &no_env, true));
        // --color always beats config.
        assert!(c.color_enabled(Some(ColorChoice::Always), &no_env, false));
        // NO_COLOR beats config 'always'.
        c.ui_color = ColorChoice::Always;
        let no_color = Env::from_map(
            [("NO_COLOR".to_string(), "1".to_string())]
                .into_iter()
                .collect(),
        );
        assert!(!c.color_enabled(None, &no_color, true));
    }

    #[test]
    fn keybindings_deep_merge_per_action() {
        let mut c = Config::default();
        // Global layer rebinds navigate-up.
        c.apply(ConfigLayer {
            ui_keybindings: vec![(KeyAction::NavigateUp, KeyChord::key(KeyCode::Char('w')))],
            ..Default::default()
        });
        // Per-repo layer rebinds navigate-up again, plus quit.
        c.apply(ConfigLayer {
            ui_keybindings: vec![
                (KeyAction::NavigateUp, KeyChord::key(KeyCode::Char('e'))),
                (KeyAction::Quit, KeyChord::key(KeyCode::Char('x'))),
            ],
            ..Default::default()
        });
        let km = c.keymap();
        // Per-repo wins for navigate-up.
        assert_eq!(
            km.action_for(KeyChord::key(KeyCode::Char('e'))),
            Some(KeyAction::NavigateUp)
        );
        assert_eq!(km.action_for(KeyChord::key(KeyCode::Char('w'))), None);
        // Quit rebound, but unrelated actions keep their defaults.
        assert_eq!(
            km.action_for(KeyChord::key(KeyCode::Char('x'))),
            Some(KeyAction::Quit)
        );
        assert_eq!(
            km.action_for(KeyChord::key(KeyCode::Char('n'))),
            Some(KeyAction::New)
        );
    }

    #[test]
    fn theme_defaults_to_one_dark() {
        let c = Config::default();
        assert_eq!(c.ui_theme, ThemePreset::OneDark);
        assert_eq!(c.theme_overrides, ThemeOverrides::default());
        assert_eq!(c.palette(), Palette::one_dark());
    }

    #[test]
    fn theme_preset_and_overrides_apply_and_merge() {
        let mut c = Config::default();
        // Global layer: solarized preset + an accent override.
        c.apply(ConfigLayer {
            ui_theme: Some(ThemePreset::Solarized),
            theme_overrides: ThemeOverrides {
                accent: Some(Color::Rgb(1, 1, 1)),
                ..Default::default()
            },
            ..Default::default()
        });
        // Per-repo layer: override red only; preset and accent untouched.
        c.apply(ConfigLayer {
            theme_overrides: ThemeOverrides {
                red: Some(Color::Rgb(2, 2, 2)),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(c.ui_theme, ThemePreset::Solarized);
        let p = c.palette();
        // Both overrides survive (deep-merge per slot).
        assert_eq!(p.accent, Color::Rgb(1, 1, 1));
        assert_eq!(p.red, Color::Rgb(2, 2, 2));
        // A non-overridden slot keeps the solarized base.
        assert_eq!(p.green, Palette::solarized().green);
    }

    #[test]
    fn later_theme_override_wins_for_same_slot() {
        let mut o = ThemeOverrides {
            accent: Some(Color::Rgb(1, 1, 1)),
            ..Default::default()
        };
        o.merge(ThemeOverrides {
            accent: Some(Color::Rgb(9, 9, 9)),
            ..Default::default()
        });
        assert_eq!(o.accent, Some(Color::Rgb(9, 9, 9)));
    }
}
