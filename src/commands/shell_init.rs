//! `wt shell-init <shell>` — print the shell-integration snippet (spec §5/§9).
//!
//! The snippet defines a `wt` shell function that intercepts the navigation
//! subcommands (`switch`/`sw`, `new`, `pr`, `ui`/`tui`, and no subcommand),
//! captures their path-only stdout, and `cd`s into it on success. Invocations
//! that print to stdout but should not be `cd`'d into — `--json`,
//! `--print-path`, and the clap help/version flags (`-h`/`--help`/`-V`/
//! `--version`) — bypass the capture and run straight through. It also wires
//! up completion, including the dynamic `wt __complete` helper.
//!
//! The `cd` is additionally gated on the captured stdout being a *real
//! directory*: nav commands always emit an existing worktree path, while
//! `wt pr open` succeeds by printing a PR *URL*. Without the guard the wrapper
//! would `cd` into that URL and fail with "no such file or directory" — so any
//! non-directory stdout is reprinted instead (issue #40).

use clap_complete::Shell;

use crate::cli::ShellInitArgs;
use crate::cx::Cx;
use crate::error::Result;

/// Prints the integration snippet for the requested shell.
pub(crate) fn run(cx: &mut Cx, args: &ShellInitArgs) -> Result<u8> {
    let snippet = match args.shell {
        Shell::Bash => BASH,
        Shell::Zsh => ZSH,
        Shell::Fish => FISH,
        Shell::PowerShell => POWERSHELL,
        Shell::Elvish => ELVISH,
        // `Shell` is non-exhaustive; future shells get the POSIX snippet.
        _ => BASH,
    };
    cx.out.text(snippet)?;
    Ok(0)
}

const BASH: &str = r#"# wt shell integration (bash) — source this from your ~/.bashrc
wt() {
  case "${1-}" in
    switch|sw|checkout|co|new|pr|ui|tui|"")
      local __wt_arg
      for __wt_arg in "$@"; do
        case "$__wt_arg" in
          --json|--print-path|-h|--help|-V|--version) command wt "$@"; return $? ;;
        esac
      done
      local __wt_out __wt_code
      __wt_out="$(command wt "$@")"; __wt_code=$?
      # Only cd into a real directory: `wt pr open` succeeds by printing a PR
      # URL, which must be reprinted, not navigated into (issue #40).
      if [ "$__wt_code" -eq 0 ] && [ -d "$__wt_out" ]; then
        builtin cd -- "$__wt_out"
      else
        [ -n "$__wt_out" ] && printf '%s\n' "$__wt_out"
        return "$__wt_code"
      fi
      ;;
    *) command wt "$@" ;;
  esac
}

_wt_complete() {
  local cur prev cmd
  cur="${COMP_WORDS[COMP_CWORD]}"
  prev="${COMP_WORDS[COMP_CWORD-1]}"
  cmd="${COMP_WORDS[1]-}"
  case "$cmd" in
    switch|sw|remove|rm|status|path)
      if [ "$COMP_CWORD" -ge 2 ]; then
        COMPREPLY=($(command wt __complete worktrees "$cur" 2>/dev/null)); return
      fi ;;
    new)
      if [ "$prev" = "--from" ] || [ "$COMP_CWORD" -eq 2 ]; then
        COMPREPLY=($(command wt __complete branches "$cur" 2>/dev/null)); return
      fi ;;
    checkout|co)
      if [ "$COMP_CWORD" -eq 2 ]; then
        COMPREPLY=($(command wt __complete all-branches "$cur" 2>/dev/null)); return
      fi ;;
    pr)
      if [ "$COMP_CWORD" -eq 2 ]; then
        COMPREPLY=($(command wt __complete pr-numbers "$cur" 2>/dev/null)); return
      fi ;;
  esac
  if [ "$COMP_CWORD" -eq 1 ]; then
    COMPREPLY=($(compgen -W "new checkout co list ls switch sw remove rm prune pr status path root init config completions shell-init ui tui" -- "$cur"))
  fi
}
complete -F _wt_complete wt
"#;

const ZSH: &str = r#"# wt shell integration (zsh) — source this from your ~/.zshrc
wt() {
  case "${1-}" in
    switch|sw|checkout|co|new|pr|ui|tui|"")
      local __wt_arg
      for __wt_arg in "$@"; do
        case "$__wt_arg" in
          --json|--print-path|-h|--help|-V|--version) command wt "$@"; return $? ;;
        esac
      done
      local __wt_out __wt_code
      __wt_out="$(command wt "$@")"; __wt_code=$?
      # Only cd into a real directory: `wt pr open` succeeds by printing a PR
      # URL, which must be reprinted, not navigated into (issue #40).
      if [[ $__wt_code -eq 0 && -d "$__wt_out" ]]; then
        builtin cd -- "$__wt_out"
      else
        [[ -n "$__wt_out" ]] && print -r -- "$__wt_out"
        return $__wt_code
      fi
      ;;
    *) command wt "$@" ;;
  esac
}

_wt() {
  local cmd="${words[2]-}"
  case "$cmd" in
    switch|sw|remove|rm|status|path)
      compadd -- ${(f)"$(command wt __complete worktrees "${words[CURRENT]}" 2>/dev/null)"}; return ;;
    new)
      compadd -- ${(f)"$(command wt __complete branches "${words[CURRENT]}" 2>/dev/null)"}; return ;;
    checkout|co)
      compadd -- ${(f)"$(command wt __complete all-branches "${words[CURRENT]}" 2>/dev/null)"}; return ;;
    pr)
      compadd -- ${(f)"$(command wt __complete pr-numbers "${words[CURRENT]}" 2>/dev/null)"}; return ;;
  esac
  compadd -- new checkout co list ls switch sw remove rm prune pr status path root init config completions shell-init ui tui
}
compdef _wt wt
"#;

const FISH: &str = r#"# wt shell integration (fish) — source this from your config.fish
function wt
    set -l cmd $argv[1]
    if test (count $argv) -eq 0; or contains -- "$cmd" switch sw checkout co new pr ui tui
        if contains -- --json $argv; or contains -- --print-path $argv; or contains -- -h $argv; or contains -- --help $argv; or contains -- -V $argv; or contains -- --version $argv
            command wt $argv
            return $status
        end
        set -l __wt_out (command wt $argv)
        set -l __wt_code $status
        # Only cd into a real directory: `wt pr open` succeeds by printing a PR
        # URL, which must be reprinted, not navigated into (issue #40).
        if test $__wt_code -eq 0; and test -d "$__wt_out"
            cd $__wt_out
        else
            test -n "$__wt_out"; and printf '%s\n' $__wt_out
            return $__wt_code
        end
    else
        command wt $argv
    end
end

complete -c wt -f
complete -c wt -n '__fish_seen_subcommand_from switch sw remove rm status path' \
    -a '(command wt __complete worktrees 2>/dev/null)'
complete -c wt -n '__fish_seen_subcommand_from new' \
    -a '(command wt __complete branches 2>/dev/null)'
complete -c wt -n '__fish_seen_subcommand_from checkout co' \
    -a '(command wt __complete all-branches 2>/dev/null)'
complete -c wt -n '__fish_seen_subcommand_from pr' \
    -a '(command wt __complete pr-numbers 2>/dev/null)'
complete -c wt -n '__fish_use_subcommand' \
    -a 'new checkout co list switch remove prune pr status path root init config completions shell-init ui tui'
"#;

const POWERSHELL: &str = r#"# wt shell integration (PowerShell) — add to your $PROFILE
function wt {
    $nav = @('switch','sw','checkout','co','new','pr','ui','tui')
    $exe = (Get-Command wt -CommandType Application | Select-Object -First 1).Source
    if ($args.Count -eq 0 -or $nav -contains $args[0]) {
        if ($args -contains '--json' -or $args -contains '--print-path' -or $args -contains '-h' -or $args -contains '--help' -or $args -contains '-V' -or $args -contains '--version') { & $exe @args; return }
        $out = & $exe @args
        # Only cd into a real directory: `wt pr open` succeeds by printing a PR
        # URL, which must be reprinted, not navigated into (issue #40).
        if ($LASTEXITCODE -eq 0 -and $out -and (Test-Path -LiteralPath "$out" -PathType Container)) { Set-Location -LiteralPath "$out" }
        elseif ($out) { Write-Output $out }
    } else {
        & $exe @args
    }
}

Register-ArgumentCompleter -CommandName wt -Native -ScriptBlock {
    param($wordToComplete, $commandAst, $cursorPosition)
    $exe = (Get-Command wt -CommandType Application | Select-Object -First 1).Source
    $elements = $commandAst.CommandElements
    $sub = if ($elements.Count -ge 2) { $elements[1].ToString() } else { '' }
    switch -Regex ($sub) {
        '^(switch|sw|remove|rm|status|path)$' { & $exe __complete worktrees $wordToComplete }
        '^new$' { & $exe __complete branches $wordToComplete }
        '^(checkout|co)$' { & $exe __complete all-branches $wordToComplete }
        '^pr$'  { & $exe __complete pr-numbers $wordToComplete }
        default { 'new','checkout','co','list','switch','remove','prune','pr','status','path','root','init','config','completions','shell-init','ui','tui' | Where-Object { $_ -like "$wordToComplete*" } }
    }
}
"#;

const ELVISH: &str = r#"# wt shell integration (elvish) — source this from your rc.elv
fn wt {|@a|
    var nav = [switch sw checkout co new pr ui tui]
    if (or (== (count $a) 0) (and (> (count $a) 0) (has-value $nav $a[0]))) {
        if (or (has-value $a --json) (has-value $a --print-path) (has-value $a -h) (has-value $a --help) (has-value $a -V) (has-value $a --version)) {
            (external wt) $@a
            return
        }
        var out = (external wt $@a | slurp | str:trim-space (one))
        # Only cd into a real directory: `wt pr open` succeeds by printing a PR
        # URL. cd raises on a non-directory, so echo it instead (issue #40).
        if (and (== $edit:command-exit-status 0) (not-eq $out "")) {
            try {
                cd $out
            } catch _ {
                echo $out
            }
        } elif (not-eq $out "") {
            echo $out
        }
    } else {
        (external wt) $@a
    }
}
"#;

#[cfg(test)]
mod tests {
    use crate::cli::ShellInitArgs;
    use clap_complete::Shell;

    fn snippet(shell: Shell) -> String {
        let mut t = crate::testutil::test_cx(&[], "/tmp");
        let code = super::run(&mut t.cx, &ShellInitArgs { shell }).unwrap();
        assert_eq!(code, 0);
        t.out.contents()
    }

    #[test]
    fn every_shell_defines_a_wt_wrapper() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let s = snippet(shell);
            assert!(s.contains("wt"), "no wt mention for {shell:?}");
        }
    }

    #[test]
    fn posix_wrappers_handle_cd_json_and_completion() {
        for shell in [Shell::Bash, Shell::Zsh] {
            let s = snippet(shell);
            assert!(s.contains("cd"), "no cd for {shell:?}");
            assert!(s.contains("--json"), "no --json guard for {shell:?}");
            assert!(
                s.contains("__complete worktrees"),
                "no dynamic completion for {shell:?}"
            );
            assert!(s.contains("switch"));
            // `--print-path` forces print-only inside the wrapper (spec §5).
            assert!(
                s.contains("--print-path"),
                "no --print-path passthrough for {shell:?}"
            );
        }
    }

    #[test]
    fn wrappers_only_cd_into_real_directories() {
        // Regression for #40: `wt pr open` succeeds by printing a PR *URL* to
        // stdout. The wrapper must gate its `cd` on the output being an actual
        // directory, otherwise it `cd`s into the URL ("no such file or
        // directory"). Every shell guards the navigation differently, so assert
        // the directory check is present in each.
        for shell in [Shell::Bash, Shell::Zsh] {
            let s = snippet(shell);
            assert!(
                s.contains(r#"-d "$__wt_out""#),
                "{shell:?} cd is not gated on a directory test"
            );
        }
        assert!(
            snippet(Shell::Fish).contains(r#"test -d "$__wt_out""#),
            "fish cd is not gated on a directory test"
        );
        assert!(
            snippet(Shell::PowerShell).contains("-PathType Container"),
            "powershell cd is not gated on a container test"
        );
        // Elvish has no portable directory predicate, so it guards `cd` in a
        // try/catch and echoes the value when navigation fails.
        let elvish = snippet(Shell::Elvish);
        assert!(elvish.contains("try {"), "elvish cd is not guarded");
        assert!(elvish.contains("catch"), "elvish has no cd fallback");
    }

    #[test]
    fn every_wrapper_passes_through_print_path() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            assert!(
                snippet(shell).contains("--print-path"),
                "{shell:?} wrapper ignores --print-path"
            );
        }
    }

    #[test]
    fn every_wrapper_passes_through_help_and_version() {
        // Regression for #21: `wt pr -h` prints help to stdout and exits 0, so
        // without a guard the wrapper would `cd` into the help text. Each
        // wrapper must force a passthrough on the help/version flags.
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let s = snippet(shell);
            assert!(
                s.contains("--help"),
                "{shell:?} wrapper does not guard --help"
            );
            assert!(
                s.contains("--version"),
                "{shell:?} wrapper does not guard --version"
            );
        }
    }

    #[test]
    fn every_wrapper_handles_checkout() {
        // The checkout command emits a path (nav contract), so every wrapper must
        // treat `checkout`/`co` as cd-eligible and offer it as a subcommand.
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::PowerShell,
            Shell::Elvish,
        ] {
            let s = snippet(shell);
            assert!(
                s.contains("checkout"),
                "{shell:?} wrapper does not mention checkout"
            );
        }
    }

    #[test]
    fn checkout_completes_local_and_remote_branches() {
        // `wt checkout` switches to a local *or* remote-only branch (DWIM), so its
        // completion must offer both via the `all-branches` kind, while `new`
        // keeps the local-only `branches` kind (issue #32).
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
            let s = snippet(shell);
            assert!(
                s.contains("__complete all-branches"),
                "{shell:?} checkout does not complete remote branches"
            );
            assert!(
                s.contains("__complete branches"),
                "{shell:?} dropped the local-only branches completion"
            );
        }
    }

    #[test]
    fn fish_wrapper_has_cd_and_completion() {
        let s = snippet(Shell::Fish);
        assert!(s.contains("function wt"));
        assert!(s.contains("cd "));
        assert!(s.contains("__complete worktrees"));
    }

    #[test]
    fn powershell_wrapper_uses_set_location() {
        let s = snippet(Shell::PowerShell);
        assert!(s.contains("Set-Location"));
        assert!(s.contains("--json"));
        assert!(s.contains("__complete"));
    }

    #[test]
    fn elvish_wrapper_defines_fn() {
        let s = snippet(Shell::Elvish);
        assert!(s.contains("fn wt"));
        assert!(s.contains("cd "));
    }
}
