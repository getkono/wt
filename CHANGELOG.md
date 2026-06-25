# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Releases are automated from [Conventional Commits](https://www.conventionalcommits.org)
by [release-plz](https://release-plz.dev); new sections below are generated on each
release. See [RELEASING.md](RELEASING.md) for the process.

## [Unreleased]

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
