# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are automated from [Conventional Commits](https://www.conventionalcommits.org)
by [release-plz](https://release-plz.dev); new sections below are generated on each
release. See [RELEASING.md](RELEASING.md) for the process.

## [Unreleased]

## [1.5.0](https://github.com/getkono/wt/compare/v1.4.0...v1.5.0) - 2026-07-10

### Added

- *(checkout,pr)* support --start
- *(shell-init)* cd into the worktree after --start via $WT_CD_FILE
- *(new)* add --start to run a command in the new worktree
- *(hooks)* add run_start to execute a command in a worktree
- *(commands)* auto-answer prompts under --yes
- *(cli)* add a global -y/--yes flag

### Other

- document --yes, --start, and WT_CD_FILE
- *(commands)* funnel nav commands through finish_worktree
- *(pr)* drop `pr open -y` in favor of the global flag

## [1.4.0](https://github.com/getkono/wt/compare/v1.3.0...v1.4.0) - 2026-07-02

### Added

- *(tui)* richly explain the blocked exit

### Fixed

- *(tui)* block premature exit while background jobs run

### Other

- *(tui)* route the event-loop exit through App::exit_now

## [1.3.0](https://github.com/getkono/wt/compare/v1.2.0...v1.3.0) - 2026-07-01

### Added

- *(tui)* run background jobs concurrently with per-row spinners

## [1.2.0](https://github.com/getkono/wt/compare/v1.1.0...v1.2.0) - 2026-06-28

### Added

- *(tui)* enable sync on worktree-less branch rows
- *(sync)* sync worktree-less branches by moving the ref

## [1.1.0](https://github.com/getkono/wt/releases/tag/v1.1.0) - 2026-06-27

### Added

- `wt drop` removes the current worktree.

### Fixed

- `shell-init`: run `wt` in the elvish navigation-capture path.
- TUI: require a real terminal before entering raw mode, and restore the
  terminal when setup fails after raw mode was already enabled.

## [1.0.0](https://github.com/getkono/wt/releases/tag/v1.0.0) - 2026-06-25

Initial public release of `wt`, a single-binary CLI + TUI for managing Git
worktrees and their GitHub pull requests, now MIT-licensed and distributed via
the [getkono/homebrew-tap](https://github.com/getkono/homebrew-tap) Homebrew tap.

### Added

- Worktree lifecycle commands: `new`, `switch`/`co`, `go`/`sw`, `list`/`ls`,
  `remove`/`rm`, and `prune` for bulk-cleaning merged or stale worktrees.
- GitHub PR integration: `pr` checks out a pull request into its own worktree,
  backed by the `sendit` helper library.
- Repository helpers: `status`, `path`, `root`, `init`, and `config`.
- Sync command: `sync` pulls then pushes the current (or selected) worktree's branch.
- A terminal UI (default when no subcommand is given, or explicit `tui`) for
  browsing, creating, switching, and removing worktrees interactively.
- Shell integration via `shell-init` (bash, zsh, fish, PowerShell) for real
  directory navigation plus dynamic tab completion; standalone `completion`
  scripts are also available.
- Global flags for machine-readable `--json` output, `--color` control,
  `--no-pager`, and `-C <PATH>` (mirrors `git -C`).
- Rich `--version` output embedding the build commit, profile, toolchain, and
  timestamp.
