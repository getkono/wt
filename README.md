# wt

`wt` is a single-binary CLI + TUI for managing Git worktrees and their GitHub
pull requests: create a branch and worktree in one step, jump between them, check
out PRs into isolated directories, and clean up when work merges. Git is the
source of truth — worktrees you create or remove with plain `git` show up
automatically.

## Getting Started

### 1. Install (from source)

`wt` is not published anywhere; building from source is the only way to install it.
You need the [Rust toolchain](https://rustup.rs) (rustup), `git` ≥ 2.20 on your
`PATH`, and — only for PR commands — the [`gh` CLI](https://cli.github.com).

```bash
just install        # cargo install --path .  → installs `wt` to ~/.cargo/bin
```

Make sure `~/.cargo/bin` is on your `PATH`. Then enable shell integration
(below) — that single step also gives you the best tab completion.

### 2. Enable shell integration (required for navigation)

A program can't change its parent shell's working directory, so on its own `wt`
can only *print* where to go. The shell wrapper closes that gap: it captures the
path and `cd`s you in. Source it from your shell rc once:

```bash
# ~/.zshrc or ~/.bashrc
eval "$(wt shell-init zsh)"      # use `bash` for bash

# fish (~/.config/fish/config.fish)
wt shell-init fish | source

# PowerShell ($PROFILE)
wt shell-init powershell | Out-String | Invoke-Expression
```

**Without it, `switch`, `new`, `pr`, and the TUI just print a path instead of
moving you.** Supported shells: bash, zsh, fish, powershell, elvish. On anything
else, `wt switch --print-path` lets you build your own `cd` alias.

This is also the recommended way to get tab completion. The `shell-init` snippet
installs *dynamic* completions that suggest live values — real worktree names,
branches, and PR numbers (via `wt __complete`) — not just the static command and
flag list. Because you need to source it for navigation anyway, it's the single
step that sets up everything; there's no separate completions install. (A static,
values-unaware script is still available via `wt completions <shell>` if you want
to manage it yourself.)

### 3. Authenticate `gh` (only for PR commands)

```bash
gh auth login
```

Everything except `wt pr` works fully offline. If `gh` is missing or
unauthenticated, only the PR commands fail (with an actionable message); the rest
keep working.

### 4. Open it

Run `wt` with no arguments in any repository to launch the TUI dashboard, then
press `?` for the full keymap — creating, switching, removing, checking out PRs,
sorting, and filtering are all discoverable from there. For example:

```bash
wt new feature/login   # create the branch + worktree and switch into it
wt switch              # fuzzy-pick a worktree to jump to
```

Run `wt --help` (or `wt <command> --help`) for the complete command surface.

## Key features to know

These are the things worth knowing up front; the rest is discoverable from
`--help` and the TUI.

- **See every branch, not just worktrees.** The TUI lists your worktrees first,
  then — dimmed beneath them — any local branch that has no worktree, each with how
  far it is ahead/behind its base. Select one and press `Enter` to create a
  worktree for it and switch in (it asks first). A branch left behind after you
  remove its worktree stays visible here instead of vanishing.
- **Pick options on pop-up fields.** TUI fields with known choices offer an
  inline dropdown instead of blind typing. The new-worktree branch/base fields
  suggest existing local **and** remote branches to fork from or check out — type
  to filter, `↑/↓` to pick, `Enter` to accept, or just type a brand-new name. The
  PR compose form's model and effort fields list their choices the same way.
- **Where worktrees are created.** New worktrees follow a configurable path
  template. The default keeps them beside the repo, out of it, and prefixes each
  worktree directory with the repo name so it's obvious which repo you're in:
  `{repo_parent}/{repo}.worktrees/{repo}-{branch_slug}`. Change it with
  `wt config set path_template …`. Common alternatives: a subdir inside the repo,
  `{repo_root}/.worktrees/{branch_slug}` (add it to `.gitignore`), or a central
  store, `{home}/worktrees/{repo}/{branch_slug}`. Worktrees you made by hand
  anywhere are still listed and managed.
- **Auto-copy ignored files into new worktrees.** Git-ignored files like `.env`
  don't follow a new worktree. List glob patterns under `copy` to bring them along
  on `wt new`, e.g. `copy = [".env", ".env.local"]`.
- **Run a command after creating a worktree.** `hooks.post_create` (e.g.
  `npm install`, `direnv allow`) runs inside the new worktree; `hooks.pre_remove`
  runs before removal. Hooks receive `WT_WORKTREE_PATH`, `WT_BRANCH`,
  `WT_REPO_ROOT`, and friends in their environment.
- **Configuration lives in two places.** A per-repo `.wt.toml` at the repo root and
  a global user config, managed with `wt config get|set|list|edit` (`--global` for
  the user config); precedence is flags > repo > global. `wt init` is an optional
  convenience that scaffolds a starter `.wt.toml` and, for a subdir store, offers
  to add it to `.gitignore`.
- **Theme the TUI.** Pick a built-in palette and tweak individual colors under
  `[ui.theme]`: `preset` selects the base (`one-dark` (default) or `solarized`),
  and the named slots (`accent`, `green`, `red`, `yellow`, `orange`, `cyan`,
  `magenta`, `gray`, `selection_bg`, `chip_fg`) override it. Colors are `#rrggbb`
  hex, a named color (e.g. `cyan`, `light-blue`), or a 0–255 ANSI index. Like every
  setting, themes merge across layers (a global base palette, per-repo accents), e.g.

  ```toml
  [ui.theme]
  preset = "solarized"
  accent = "#ff8800"
  ```
- **Removal protects your work.** `wt remove` and `wt prune` refuse to drop a
  worktree with uncommitted or unpushed changes unless you pass `--force`.
- **Bulk-clean stale branches.** `wt prune --merged` removes worktrees whose branch
  is merged into the default branch, and `wt prune --gone` removes worktrees whose
  upstream was deleted (plus any missing worktrees). Both also delete matching
  **local branches that no longer have a worktree** — so a repo left with a pile of
  merged feature branches gets cleaned up too. Preview with `--dry-run`. A `--gone`
  branch that isn't also merged may hold unmerged commits, so it is skipped unless
  you pass `--force`. The current and default branches are never touched.

## Development

### Prerequisites

- [Rust (rustup)](https://rustup.rs) — toolchain (pinned via `rust-toolchain.toml`)
- [just](https://github.com/casey/just) — command runner
- [Lefthook](https://github.com/evilmartians/lefthook) — git hooks manager
- [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) — code coverage tool

| Command             | Description                          |
| ------------------- | ------------------------------------ |
| `cargo run`         | Run the application                  |
| `just install`      | Build and install `wt` to ~/.cargo/bin |
| `just test`         | Run tests                            |
| `just format`       | Format code                          |
| `just lint`         | Lint with Clippy (warnings as errors)|
| `just lint-fix`     | Lint and auto-fix                    |
| `just coverage`     | Run tests with coverage (min 80%)    |

After cloning, run `lefthook install` once to activate the git hooks.

### Tech Stack

- **Runtime:** Rust (edition 2024)
- **Formatter:** rustfmt
- **Linter:** Clippy
- **Task runner:** just
- **Key Dependencies:** tokio, eyre + color-eyre, tracing + tracing-subscriber, thiserror

### Architecture

The logic lives in the library crate (`src/lib.rs`) so it is unit-testable and
measured by coverage. The binary (`src/main.rs`) is a thin entry point that
wires up error reporting and tracing, then delegates to the library; it is
excluded from coverage.

### Git Hooks

This project uses [Lefthook](https://github.com/evilmartians/lefthook).
Pre-commit hooks auto-fix formatting and linting on staged Rust files.
Pre-push hooks run format checks, Clippy, tests, and the coverage gate.

### CI/CD

GitHub Actions runs format checks, linting, tests, and coverage on pushes to
`main` and pull requests.

### Code Coverage

This project uses [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov)
for LLVM-based code coverage. CI enforces a minimum of 80% line coverage and
uploads the report as a CI artifact.

```bash
just coverage
```

## License

Proprietary — all rights reserved. See [LICENSE](LICENSE) for details.
