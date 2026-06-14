//! `wt completions <shell>` — print a static completion script (spec §9).

use clap::CommandFactory;

use crate::cli::{Cli, CompletionsArgs};
use crate::cx::Cx;
use crate::error::Result;

/// Generates and prints a `clap`-based completion script for the given shell
/// (subcommands, flags, and enumerated values).
pub(crate) fn run(cx: &mut Cx, args: &CompletionsArgs) -> Result<u8> {
    let mut command = Cli::command();
    let mut buffer: Vec<u8> = Vec::new();
    clap_complete::generate(args.shell, &mut command, "wt", &mut buffer);
    cx.out.text(&String::from_utf8_lossy(&buffer))?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::cli::CompletionsArgs;
    use clap_complete::Shell;

    fn run_for(shell: Shell) -> String {
        let mut t = crate::testutil::test_cx(&[], "/tmp");
        let code = super::run(&mut t.cx, &CompletionsArgs { shell }).unwrap();
        assert_eq!(code, 0);
        t.out.contents()
    }

    #[test]
    fn generates_for_all_shells() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let script = run_for(shell);
            assert!(!script.is_empty(), "empty script for {shell:?}");
            assert!(script.contains("wt"), "missing bin name for {shell:?}");
        }
    }

    #[test]
    fn bash_script_mentions_subcommands() {
        let script = run_for(Shell::Bash);
        assert!(script.contains("switch"));
        assert!(script.contains("status"));
    }
}
