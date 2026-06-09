//! Command-line surface (spec §7): the `clap` command tree, global flags,
//! aliases, and dispatch.
//!
//! [`dispatch`] parses `argv`, applies `-C`, validates per-command `--json`
//! support, and routes to the command handlers. Parsing/usage errors map to
//! exit code `2`; `--help`/`--version` render to stdout and exit `0`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use clap_complete::Shell;

use crate::cx::Cx;
use crate::error::{Error, Result};
use crate::output::color::ColorChoice;

/// Top-level parsed command line: global flags plus an optional subcommand
/// (no subcommand launches the TUI).
#[derive(Debug, Parser)]
#[command(
    name = "wt",
    version = crate::version::long_version(),
    about = "Git worktree and GitHub PR manager",
    propagate_version = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Flags accepted by every subcommand.
    #[command(flatten)]
    pub global: GlobalFlags,
    /// The subcommand to run; `None` launches the TUI.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Flags accepted by every subcommand (spec §7 "Global flags").
#[derive(Debug, Args)]
pub struct GlobalFlags {
    /// Emit machine-readable JSON (only where the command supports it).
    #[arg(long, global = true)]
    pub json: bool,
    /// Control ANSI color: `auto` (default), `always`, or `never`.
    #[arg(long, global = true, value_name = "WHEN")]
    pub color: Option<ColorChoice>,
    /// Never page output (useful for scripting).
    #[arg(long = "no-pager", global = true)]
    pub no_pager: bool,
    /// Run as if invoked from `<PATH>` (mirrors `git -C`).
    #[arg(short = 'C', long = "directory", global = true, value_name = "PATH")]
    pub directory: Option<PathBuf>,
    /// Emit additional diagnostics to stderr (stackable, e.g. `-vv`).
    #[arg(short = 'v', long = "verbose", global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

/// The `wt` subcommands (spec §7).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a linked worktree from a branch or base ref.
    New(NewArgs),
    /// Switch the branch checked out in the current worktree (syncs with origin).
    #[command(visible_alias = "co")]
    Checkout(CheckoutArgs),
    /// List worktrees.
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Navigate to a worktree (prints its path).
    #[command(visible_alias = "sw")]
    Switch(SwitchArgs),
    /// Remove a linked worktree.
    #[command(visible_alias = "rm")]
    Remove(RemoveArgs),
    /// Bulk-clean merged or stale worktrees.
    Prune(PruneArgs),
    /// Check out a GitHub PR into its own worktree.
    #[command(args_conflicts_with_subcommands = true)]
    Pr(PrArgs),
    /// Detailed status for one or all worktrees.
    Status(StatusArgs),
    /// Print the absolute path of a matching worktree.
    Path(PathArgs),
    /// Print the repository root (primary worktree or bare repo path).
    Root,
    /// Initialize `wt` for the current repository.
    Init(InitArgs),
    /// Read or modify configuration.
    Config(ConfigArgs),
    /// Print a shell completion script.
    Completions(CompletionsArgs),
    /// Print the shell-integration snippet (includes completion wiring).
    #[command(name = "shell-init")]
    ShellInit(ShellInitArgs),
    /// Launch the TUI explicitly.
    #[command(visible_alias = "tui")]
    Ui,
    /// Hidden dynamic-completion helper used by the generated scripts.
    #[command(name = "__complete", hide = true)]
    Complete(CompleteArgs),
}

/// Arguments for `wt new`.
#[derive(Debug, Args)]
pub struct NewArgs {
    /// Branch to create or check out into a new worktree.
    pub branch: String,
    /// Base ref for a newly created branch (default: the repo's default branch).
    #[arg(long, value_name = "REF")]
    pub from: Option<String>,
    /// Do not switch into the new worktree.
    #[arg(long = "no-switch")]
    pub no_switch: bool,
    /// Skip the post-create hook.
    #[arg(long = "no-hooks")]
    pub no_hooks: bool,
    /// Override the source worktree for the copy step.
    #[arg(long = "copy-from", value_name = "QUERY")]
    pub copy_from: Option<String>,
}

/// Arguments for `wt checkout`.
#[derive(Debug, Args)]
pub struct CheckoutArgs {
    /// Branch to check out in this worktree (local, or remote-only via DWIM).
    pub branch: String,
    /// Do not print the worktree path (no `cd`); print a note to stderr.
    #[arg(long = "no-switch")]
    pub no_switch: bool,
    /// Discard uncommitted changes and switch anyway.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `wt list`.
#[derive(Debug, Args)]
pub struct ListArgs {
    /// Sort field; prefix with `-` for descending (e.g. `-ahead`).
    #[arg(long, value_name = "FIELD")]
    pub sort: Option<String>,
    /// Non-interactive fuzzy filter by branch/slug/path.
    #[arg(long, value_name = "QUERY")]
    pub filter: Option<String>,
}

/// Arguments for `wt switch`.
#[derive(Debug, Args)]
pub struct SwitchArgs {
    /// Worktree query; omit to open the TUI picker.
    pub query: Option<String>,
    /// Force print-only behavior even inside the shell wrapper.
    #[arg(long = "print-path")]
    pub print_path: bool,
}

/// Arguments for `wt remove`.
#[derive(Debug, Args)]
pub struct RemoveArgs {
    /// Worktree query to remove.
    pub query: String,
    /// Remove even when dirty or with unpushed work (work may be lost).
    #[arg(long)]
    pub force: bool,
    /// Always keep the local branch.
    #[arg(long = "keep-branch")]
    pub keep_branch: bool,
    /// Skip the pre-remove hook.
    #[arg(long = "no-hooks")]
    pub no_hooks: bool,
}

/// Arguments for `wt prune`.
#[derive(Debug, Args)]
pub struct PruneArgs {
    /// Include worktrees whose branch is merged into the default branch.
    #[arg(long)]
    pub merged: bool,
    /// Include worktrees whose upstream is gone, and missing worktrees.
    #[arg(long)]
    pub gone: bool,
    /// Report candidates without removing anything.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Remove without confirmation and include dirty worktrees.
    #[arg(long)]
    pub force: bool,
}

/// Arguments for `wt pr`.
#[derive(Debug, Args)]
pub struct PrArgs {
    /// PR number, URL, or head branch; omit to open the picker.
    pub target: Option<String>,
    /// Do not switch into the new worktree.
    #[arg(long = "no-switch")]
    pub no_switch: bool,
    /// Skip the post-create hook.
    #[arg(long = "no-hooks")]
    pub no_hooks: bool,
    /// The `list` sub-form: print open PRs without checking any out.
    #[command(subcommand)]
    pub sub: Option<PrSub>,
}

/// The `pr` sub-forms (`list`, `open`).
#[derive(Debug, Subcommand)]
pub enum PrSub {
    /// List open PRs without checking any out.
    List,
    /// Compose and open (create or update) a PR for the current branch.
    Open(PrOpenArgs),
}

/// Arguments for `wt pr open`.
#[derive(Debug, Args)]
pub struct PrOpenArgs {
    /// PR title. On an interactive terminal it seeds the compose form;
    /// non-interactively (or with `-y`) it is used directly.
    #[arg(long)]
    pub title: Option<String>,
    /// PR body text.
    #[arg(long, conflicts_with = "body_file")]
    pub body: Option<String>,
    /// Read the PR body from a file (use `-` for stdin).
    #[arg(long = "body-file", conflicts_with = "body")]
    pub body_file: Option<String>,
    /// Open the PR as a draft (create only).
    #[arg(long)]
    pub draft: bool,
    /// Draft the title/body with a code agent (Claude), then review before sending.
    #[arg(long)]
    pub ai: bool,
    /// Model for `--ai` drafting: `opus`, `sonnet`, or `haiku` (overrides
    /// `agent.model`; default `sonnet`).
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
    /// Effort for `--ai` drafting: `low`, `medium`, or `high` (overrides
    /// `agent.effort`; default `medium`).
    #[arg(long, value_name = "LEVEL")]
    pub effort: Option<String>,
    /// Skip the compose form and submit non-interactively.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Override the base/trunk branch to target.
    #[arg(long, value_name = "REF")]
    pub base: Option<String>,
    /// When a PR already exists for this branch, update it.
    #[arg(long, conflicts_with = "new")]
    pub update: bool,
    /// When a PR already exists for this branch, create a new one anyway.
    #[arg(long = "new", conflicts_with = "update")]
    pub new: bool,
}

/// Arguments for `wt status`.
#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Worktree query (default: the current worktree).
    pub query: Option<String>,
    /// Report every worktree.
    #[arg(long)]
    pub all: bool,
}

/// Arguments for `wt path`.
#[derive(Debug, Args)]
pub struct PathArgs {
    /// Worktree query.
    pub query: String,
}

/// Arguments for `wt init`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Override the worktree store path template.
    #[arg(long = "path-template", value_name = "TMPL")]
    pub path_template: Option<String>,
}

/// Arguments for `wt config`.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// The configuration action to perform.
    #[command(subcommand)]
    pub action: ConfigAction,
    /// Target the global user config instead of the per-repo file.
    #[arg(long, global = true)]
    pub global: bool,
}

/// The `wt config` actions.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the value of a key.
    Get {
        /// The configuration key.
        key: String,
    },
    /// Set a key to a value.
    Set {
        /// The configuration key.
        key: String,
        /// The new value.
        value: String,
    },
    /// List the effective configuration.
    List,
    /// Open the configuration file in the editor.
    Edit,
}

/// Arguments for `wt completions`.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// The shell to emit a completion script for.
    pub shell: Shell,
}

/// Arguments for `wt shell-init`.
#[derive(Debug, Args)]
pub struct ShellInitArgs {
    /// The shell to emit the integration snippet for.
    pub shell: Shell,
}

/// Arguments for the hidden `wt __complete` helper.
#[derive(Debug, Args)]
pub struct CompleteArgs {
    /// The kind of candidate to list (`worktrees`, `branches`, `pr-numbers`).
    pub kind: String,
    /// The partial token to complete.
    pub partial: Option<String>,
}

impl Cli {
    /// Whether the parsed command accepts `--json` (spec §7 table).
    fn command_supports_json(&self) -> bool {
        match &self.command {
            Some(
                Command::New(_)
                | Command::Checkout(_)
                | Command::List(_)
                | Command::Remove(_)
                | Command::Prune(_)
                | Command::Pr(_)
                | Command::Status(_),
            ) => true,
            Some(Command::Config(c)) => matches!(c.action, ConfigAction::List),
            _ => false,
        }
    }

    /// A short label for the parsed command, for diagnostics.
    fn command_label(&self) -> &'static str {
        match &self.command {
            Some(Command::New(_)) => "new",
            Some(Command::Checkout(_)) => "checkout",
            Some(Command::List(_)) => "list",
            Some(Command::Switch(_)) => "switch",
            Some(Command::Remove(_)) => "remove",
            Some(Command::Prune(_)) => "prune",
            Some(Command::Pr(_)) => "pr",
            Some(Command::Status(_)) => "status",
            Some(Command::Path(_)) => "path",
            Some(Command::Root) => "root",
            Some(Command::Init(_)) => "init",
            Some(Command::Config(_)) => "config",
            Some(Command::Completions(_)) => "completions",
            Some(Command::ShellInit(_)) => "shell-init",
            Some(Command::Ui) | None => "ui",
            Some(Command::Complete(_)) => "__complete",
        }
    }
}

/// Parses `args` (excluding `argv[0]`) and routes to the command handler,
/// returning the process exit code.
pub fn dispatch(args: Vec<String>, cx: &mut Cx) -> Result<u8> {
    let mut argv: Vec<OsString> = Vec::with_capacity(args.len() + 1);
    argv.push(OsString::from("wt"));
    argv.extend(args.into_iter().map(OsString::from));

    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(e) => return report_parse_error(&e, cx),
    };

    if let Some(dir) = cli.global.directory.clone() {
        apply_directory(cx, &dir);
    }
    cx.color_flag = cli.global.color;
    cx.no_pager = cli.global.no_pager;
    cx.verbose = cli.global.verbose;

    if cli.global.json && !cli.command_supports_json() {
        return Err(Error::usage(format!(
            "--json is not supported by `wt {}`",
            cli.command_label()
        )));
    }

    route(cli, cx)
}

/// Renders a `clap` parse error: help/version to stdout (exit `0`), usage
/// errors to stderr (exit `2`).
fn report_parse_error(error: &clap::Error, cx: &mut Cx) -> Result<u8> {
    use clap::error::ErrorKind;
    let rendered = error.render().to_string();
    match error.kind() {
        ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            cx.out.text(&rendered)?;
            Ok(0)
        }
        _ => {
            cx.err.text(&rendered)?;
            Ok(2)
        }
    }
}

/// Resolves `-C` against the current working directory and updates the context.
fn apply_directory(cx: &mut Cx, dir: &Path) {
    cx.cwd = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        cx.cwd.join(dir)
    };
}

/// Routes the parsed command to its handler. Handlers not yet implemented
/// return an "unimplemented" error.
fn route(cli: Cli, cx: &mut Cx) -> Result<u8> {
    let json = cli.global.json;
    match cli.command {
        None | Some(Command::Ui) => crate::commands::switch::launch_picker(cx),
        Some(Command::New(args)) => {
            crate::commands::new::run(cx, &crate::hooks::RealHookRunner, &args, json)
        }
        Some(Command::Checkout(args)) => crate::commands::checkout::run(cx, &args, json),
        Some(Command::List(args)) => crate::commands::list::run(cx, &args, json),
        Some(Command::Switch(args)) => crate::commands::switch::run(cx, &args),
        Some(Command::Remove(args)) => {
            crate::commands::remove::run(cx, &crate::hooks::RealHookRunner, &args, json)
        }
        Some(Command::Prune(args)) => crate::commands::prune::run(cx, &args, json),
        Some(Command::Pr(args)) => {
            crate::commands::pr::run(cx, &crate::hooks::RealHookRunner, &args, json)
        }
        Some(Command::Status(args)) => crate::commands::status_cmd::run(cx, &args, json),
        Some(Command::Path(args)) => crate::commands::path::run(cx, &args),
        Some(Command::Root) => crate::commands::root::run(cx),
        Some(Command::Init(args)) => crate::commands::init::run(cx, &args),
        Some(Command::Config(args)) => crate::commands::config_cmd::run(cx, &args, json),
        Some(Command::Completions(args)) => crate::commands::completions::run(cx, &args),
        Some(Command::ShellInit(args)) => crate::commands::shell_init::run(cx, &args),
        Some(Command::Complete(args)) => crate::commands::complete::run(cx, &args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_cx;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    fn parse(parts: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("wt").chain(parts.iter().copied()))
    }

    #[test]
    fn help_renders_to_stdout_exit_zero() {
        let mut t = test_cx(&[], "/tmp");
        let code = dispatch(argv(&["--help"]), &mut t.cx);
        assert_eq!(code.unwrap(), 0);
        assert!(t.out.contents().contains("Usage"));
        assert!(t.err.contents().is_empty());
    }

    #[test]
    fn version_renders_to_stdout_exit_zero() {
        let mut t = test_cx(&[], "/tmp");
        let code = dispatch(argv(&["--version"]), &mut t.cx).unwrap();
        assert_eq!(code, 0);
        assert!(t.out.contents().contains("wt"));
    }

    #[test]
    fn unknown_command_is_usage_error_exit_two() {
        let mut t = test_cx(&[], "/tmp");
        let code = dispatch(argv(&["bogus"]), &mut t.cx).unwrap();
        assert_eq!(code, 2);
        assert!(!t.err.contents().is_empty());
        assert!(t.out.contents().is_empty());
    }

    #[test]
    fn missing_required_arg_is_usage_error() {
        let mut t = test_cx(&[], "/tmp");
        // `new` requires a branch.
        assert_eq!(dispatch(argv(&["new"]), &mut t.cx).unwrap(), 2);
    }

    #[test]
    fn aliases_resolve() {
        assert!(matches!(
            parse(&["ls"]).unwrap().command,
            Some(Command::List(_))
        ));
        assert!(matches!(
            parse(&["sw", "q"]).unwrap().command,
            Some(Command::Switch(_))
        ));
        assert!(matches!(
            parse(&["rm", "q"]).unwrap().command,
            Some(Command::Remove(_))
        ));
        assert!(matches!(
            parse(&["co", "branch"]).unwrap().command,
            Some(Command::Checkout(_))
        ));
        assert!(matches!(
            parse(&["tui"]).unwrap().command,
            Some(Command::Ui)
        ));
    }

    #[test]
    fn checkout_parses_with_flags() {
        let cli = parse(&["checkout", "feature/x", "--no-switch", "--force"]).unwrap();
        match cli.command {
            Some(Command::Checkout(a)) => {
                assert_eq!(a.branch, "feature/x");
                assert!(a.no_switch);
                assert!(a.force);
            }
            _ => panic!("expected checkout"),
        }
        // `checkout` requires a branch.
        let mut t = test_cx(&[], "/tmp");
        assert_eq!(dispatch(argv(&["checkout"]), &mut t.cx).unwrap(), 2);
    }

    #[test]
    fn no_subcommand_is_tui() {
        assert!(parse(&[]).unwrap().command.is_none());
    }

    #[test]
    fn pr_forms_parse_distinctly() {
        // `pr list` -> the list sub-form.
        let cli = parse(&["pr", "list"]).unwrap();
        match cli.command {
            Some(Command::Pr(a)) => {
                assert!(a.target.is_none());
                assert!(matches!(a.sub, Some(PrSub::List)));
            }
            _ => panic!("expected pr"),
        }
        // `pr 123` -> a checkout target.
        let cli = parse(&["pr", "123"]).unwrap();
        match cli.command {
            Some(Command::Pr(a)) => {
                assert_eq!(a.target.as_deref(), Some("123"));
                assert!(a.sub.is_none());
            }
            _ => panic!("expected pr"),
        }
        // `pr` -> picker (no target, no sub).
        let cli = parse(&["pr"]).unwrap();
        match cli.command {
            Some(Command::Pr(a)) => {
                assert!(a.target.is_none() && a.sub.is_none());
            }
            _ => panic!("expected pr"),
        }
        // `pr open --title X --draft --ai --model opus --effort high` -> the open
        // sub-form with its flags, including the model/effort overrides.
        let cli = parse(&[
            "pr", "open", "--title", "X", "--draft", "--ai", "--model", "opus", "--effort", "high",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Pr(a)) => match a.sub {
                Some(PrSub::Open(o)) => {
                    assert_eq!(o.title.as_deref(), Some("X"));
                    assert!(o.draft);
                    assert!(o.ai);
                    assert_eq!(o.model.as_deref(), Some("opus"));
                    assert_eq!(o.effort.as_deref(), Some("high"));
                    assert!(!o.update && !o.new);
                }
                _ => panic!("expected pr open"),
            },
            _ => panic!("expected pr"),
        }
        // `--update` and `--new` are mutually exclusive.
        assert!(parse(&["pr", "open", "--update", "--new"]).is_err());
        // `--body` and `--body-file` are mutually exclusive.
        assert!(parse(&["pr", "open", "--body", "b", "--body-file", "f"]).is_err());
    }

    #[test]
    fn global_flags_parse_before_and_after_subcommand() {
        assert!(parse(&["--json", "list"]).unwrap().global.json);
        assert!(parse(&["list", "--json"]).unwrap().global.json);
        let cli = parse(&["-C", "/repo", "status"]).unwrap();
        assert_eq!(cli.global.directory.as_deref(), Some(Path::new("/repo")));
        assert_eq!(parse(&["-vv", "list"]).unwrap().global.verbose, 2);
        assert_eq!(
            parse(&["--color", "never", "list"]).unwrap().global.color,
            Some(ColorChoice::Never)
        );
    }

    #[test]
    fn json_rejected_for_unsupported_commands() {
        for cmd in [
            vec!["switch", "q"],
            vec!["path", "q"],
            vec!["root"],
            vec!["init"],
            vec!["completions", "bash"],
            vec!["shell-init", "bash"],
            vec!["ui"],
        ] {
            let mut t = test_cx(&[], "/tmp");
            let mut a = vec!["--json"];
            a.extend(cmd.iter().copied());
            // Go through `run` so the error is mapped to an exit code and
            // reported on stderr.
            let code = crate::run(argv(&a), &mut t.cx);
            assert_eq!(code, 2, "expected --json rejected for {cmd:?}");
            assert!(t.err.contents().contains("--json is not supported"));
        }
    }

    #[test]
    fn json_accepted_for_supported_commands() {
        // These pass json validation and reach their handler. Run from a
        // non-repo dir so repo-scoped handlers fail with NotInRepo and stubs
        // with Operation; either way the error is not a usage (--json) error.
        for cmd in [
            vec!["list"],
            vec!["status"],
            vec!["new", "b"],
            vec!["checkout", "b"],
            vec!["remove", "q"],
            vec!["prune", "--merged"],
            vec!["pr", "list"],
            vec!["config", "list"],
        ] {
            let mut t = test_cx(&[], "/tmp");
            let mut a = vec!["--json"];
            a.extend(cmd.iter().copied());
            let err = dispatch(argv(&a), &mut t.cx).unwrap_err();
            assert!(
                !matches!(err, Error::Usage(_)),
                "expected --json accepted for {cmd:?}, got usage error {err:?}"
            );
        }
    }

    #[test]
    fn config_get_rejects_json_but_list_accepts() {
        let mut t = test_cx(&[], "/tmp");
        assert_eq!(
            crate::run(argv(&["--json", "config", "get", "k"]), &mut t.cx),
            2
        );
        // `config list` accepts --json (reaches the handler, which fails with
        // NotInRepo from a non-repo dir — not a usage/--json rejection).
        let mut t2 = test_cx(&[], "/tmp");
        let err = dispatch(argv(&["--json", "config", "list"]), &mut t2.cx).unwrap_err();
        assert!(!matches!(err, Error::Usage(_)));
    }

    #[test]
    fn directory_flag_updates_cwd() {
        let mut t = test_cx(&[], "/start");
        // Relative path is joined onto the current cwd.
        let _ = dispatch(argv(&["-C", "sub", "root"]), &mut t.cx);
        assert_eq!(t.cx.cwd, PathBuf::from("/start/sub"));
        // Absolute path replaces it.
        let mut t = test_cx(&[], "/start");
        let _ = dispatch(argv(&["-C", "/abs", "root"]), &mut t.cx);
        assert_eq!(t.cx.cwd, PathBuf::from("/abs"));
    }

    #[test]
    fn repo_scoped_commands_fail_outside_a_repo() {
        // From a non-repo dir, switch/ui (TUI) and the no-subcommand launch all
        // fail at repository discovery (NotInRepo, exit 1) before any terminal I/O.
        for parts in [vec!["switch"], vec!["ui"], vec![]] {
            let mut t = test_cx(&[], "/tmp");
            let err = dispatch(argv(&parts), &mut t.cx).unwrap_err();
            assert!(matches!(err, Error::NotInRepo), "for {parts:?}");
        }
    }
}
