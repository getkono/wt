# wt

`wt` is a single-binary CLI + TUI for managing Git worktrees and their GitHub
pull requests: create a branch and worktree in one step, jump between them, check
out PRs into isolated directories, and clean up when work merges. Git is the
source of truth — worktrees you create or remove with plain `git` show up
automatically.

## Getting Started

### 1. Install

#### Homebrew (recommended)

```bash
brew install getkono/tap/wt
```

This pulls a prebuilt binary from the [getkono/homebrew-tap](https://github.com/getkono/homebrew-tap)
tap (macOS arm64/x86_64 and Linux arm64/x86_64).

#### Cargo (crates.io)

The crate is published as [`kono-wt`](https://crates.io/crates/kono-wt) (the bare
`wt` name was already taken); the installed binary is still `wt`.

```bash
cargo install kono-wt                               # installs `wt` to ~/.cargo/bin
```

#### From source

You need the [Rust toolchain](https://rustup.rs) (rustup), `git` ≥ 2.20 on your
`PATH`, and — only for PR commands — the [`gh` CLI](https://cli.github.com).

```bash
cargo install --git https://github.com/getkono/wt   # latest from master
# or, from a checkout:
cargo install --path .                              # installs `wt` to ~/.cargo/bin
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

### 3. Authenticate `gh` (for PR and issue commands)

```bash
gh auth login
```

Everything except `wt pr` and `wt issue` works fully offline. If `gh` is missing
or unauthenticated, only those commands fail (with an actionable message); the
rest keep working.

### 4. Open it

Run `wt` with no arguments in any repository to launch the TUI dashboard, then
press `?` for the full keymap — creating, switching, removing, checking out PRs,
sorting, and filtering are all discoverable from there. For example:

```bash
wt new feature/login   # create the branch + worktree and switch into it
wt issue 123           # branch + worktree for a GitHub issue, name proposed for you
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
- **Start from a GitHub issue.** `wt issue 123` fetches the issue's title, body,
  labels, type and milestone, proposes a conventional `type/123-slug` branch and a
  short implementation brief, lets you edit both, then creates the worktree and
  records the link. `wt list` can show it with the opt-in `issue` column
  (`wt config set list.columns status,dirty,branch,issue,path`), and running
  `wt issue 123` again reuses the branch it already made.

  Generation is best-effort and never blocks you: if the agent is missing, hangs,
  or returns nonsense, `wt` falls back to a deterministic name built from the
  issue's own labels and title, tells you it did, and carries on. Set which model
  it uses under `[agent.generation]`, or per-run with `--model`/`--effort`.

  `wt` stops at the prepared worktree — it does not run a coding agent for you.
  Handing the work to an agent is [karet](https://github.com/getkono/karet)'s job.
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
- **Worktrees of submodule-heavy repos, without the re-clone.** A linked
  worktree does not share the superproject's submodule object stores: git puts
  its submodule git directories under `worktrees/<id>/modules/`, not the shared
  `.git/modules/`, so `git submodule update --init --recursive` in a new
  worktree clones every submodule over the network again. On a repo with a lot
  of submodules that is the entire cost of making a worktree. `wt` clones them
  from the object stores already on your disk instead, which git hardlinks — no
  network, near-zero disk. It is on by default (`[submodules] seed = "auto"`,
  or `--no-seed-submodules` for one run) and cannot change the result: the
  stock `git submodule update --init --recursive` still runs afterwards and
  decides the outcome, so seeding only ever removes work from it.

  For the working tree itself there is a second, opt-in step. On a
  copy-on-write filesystem (btrfs, XFS with `reflink=1`, APFS, ReFS), set
  `[create] reflink = "auto"` (or pass `--reflink`) and a new worktree's files
  are cloned from an existing one by sharing extents rather than being written
  out — including the ignored build output you would otherwise rebuild. On one
  241 MiB repo that was 22 MiB consumed instead of 268 MiB. It applies only
  when a worktree is already at the same commit and the filesystem supports it,
  and quietly falls back to a normal checkout otherwise. It is off by default
  because carrying ignored files across is a bigger change than seeding. The
  source's *untracked* files stay where they are — the new worktree comes up
  clean, and `copy` still decides which non-tracked files travel.

  Both leave `submodule.fetchJobs` to git if you want parallel fetches.
- **Run a command after creating a worktree.** `hooks.post_create` (e.g.
  `npm install`, `direnv allow`) runs inside the new worktree; `hooks.pre_remove`
  runs before removal. Hooks receive `WT_WORKTREE_PATH`, `WT_BRANCH`,
  `WT_REPO_ROOT`, and friends in their environment.
- **Hand the new worktree straight to a tool.** `--start <command>` runs a command
  inside the worktree once it is fully set up — after the copy step, the
  `post_create` hook, and submodule init — and leaves your shell there afterwards:

  ```bash
  wt new feat/login -y --start "claude --permission-mode plan"
  wt pr 42 --start "claude"          # check a PR out and review it
  ```

  It works on `wt new`, `wt checkout`, and `wt pr`. The command gets a real
  terminal, so interactive tools work, and `wt` exits with the command's status.
  It sees the same `WT_*` variables hooks do (`$WT_BRANCH`, `$WT_PR_NUMBER`, …) —
  `wt` does not interpolate `{branch}`-style placeholders, which would collide
  with shell braces. The `cd` afterwards needs the shell integration from step 2;
  re-run `wt shell-init <shell>` if you set it up before `--start` existed.
- **Say yes to everything.** `-y`/`--yes` is a global flag that answers every
  confirmation prompt, so `wt` can run unattended. It is not `--force`: the
  safety guards on `remove`, `drop`, and `prune` — dirty worktrees, unpushed
  commits, unmerged branches — still hold, and still need `--force` to override.
- **Configuration lives in two places.** A per-repo `.wt.toml` at the repo root and
  a global user config, managed with `wt config get|set|list|edit` (`--global` for
  the user config); precedence is flags > repo > global. `wt init` is an optional
  convenience that scaffolds a starter `.wt.toml` and, for a subdir store, offers
  to add it to `.gitignore`.
- **Pick the generation model.** The short, structured generation steps `wt`
  owns — the `wt pr open --ai` draft and the `wt issue` branch/brief proposal —
  read one profile:

  ```toml
  [agent.generation]
  provider = "claude"
  model = "sonnet"   # opus | sonnet | haiku
  effort = "medium"  # low | medium | high
  ```

  The older flat `agent.model` / `agent.effort` keys still work and mean the same
  thing. `[agent.work]` is deliberately not a `wt` setting: running a coding agent
  on the work belongs to [karet](https://github.com/getkono/karet).
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
- **Drop the worktree you're in.** `wt drop` removes the worktree containing the
  current directory (from any depth), keeps the branch, and `cd`s you back to the
  main worktree. It refuses the primary worktree and honors the same `--force`
  guard.
- **Bulk-clean stale branches.** `wt prune --merged` removes worktrees whose branch
  is merged into the default branch, and `wt prune --gone` removes worktrees whose
  upstream was deleted (plus any missing worktrees). Both also delete matching
  **local branches that no longer have a worktree** — so a repo left with a pile of
  merged feature branches gets cleaned up too. Preview with `--dry-run`. A `--gone`
  branch that isn't also merged may hold unmerged commits, so it is skipped unless
  you pass `--force`. The current and default branches are never touched.

## Using wt as a library

Everything the CLI and TUI do sits on a worktree engine that is usable on its
own. [karet](https://github.com/getkono/karet) drives it directly; the contract
below is what it depends on.

### Consuming it

```toml
[dependencies]
kono-wt = { version = "1", default-features = false }
```

The package is `kono-wt`, but the library target is named `wt`, so the import
path is `wt::…` either way (the same rename that leaves the installed binary
called `wt`).

`default-features = false` drops the application surface — argument parsing, the
TUI, the PR compose flow, the agent integration — and with it `clap`,
`clap_complete`, `ratatui`, `crossterm`, `nucleo-matcher`, `futures-util`,
`tokio`, `sendit`, `color-eyre`, `eyre` and `tracing-subscriber`. What remains is
the engine: the worktree service, config, git, branch naming, path templating and
the typed error enum. Turn features back on individually (`cli`, `tui`, `pr`,
`agent`) if you want part of the application surface too.

### The worktree service

`wt::worktree::Workspace` is the entry point. Discover a repository, then
enumerate, create and remove worktrees:

```rust
use std::path::Path;

use wt::git::RealGit;
use wt::hooks::RealHookRunner;
use wt::worktree::{CreateOptions, Workspace};
use wt::{Env, install_signal_handlers};

fn main() -> Result<(), wt::Error> {
    install_signal_handlers();

    let env = Env::from_real();
    let ws = Workspace::discover(Path::new("."), &env, &RealGit)?;

    // Detached worktrees have no branch, so `branch` is an `Option`.
    for worktree in ws.list(&RealGit)? {
        let branch = worktree.branch.as_deref().unwrap_or("(detached)");
        println!("{branch}\t{}", worktree.path.display());
    }

    let created = ws.create(
        &RealGit,
        &RealHookRunner,
        &CreateOptions {
            branch: "feat/login".into(),
            ..CreateOptions::default()
        },
    )?;
    println!("{}", created.path.display());
    Ok(())
}
```

**The service never prompts and never writes to stdout or stderr.** That is the
property that makes it embeddable: everything a user might need to see comes back
as data on the outcome structs — hook results (`HookOutcome`), what the copy step
did, how submodule initialization went, and whether a removal was forced past the
dirty/unpushed guards. The caller decides how, or whether, to present any of it.
Failures are typed variants of `wt::Error`, not messages.

`Workspace::create` is idempotent: an existing worktree at the configured target
comes back with `reused: true` rather than an error.

### Where worktrees live

Layout is repository configuration, not a convention:

```rust
use wt::template::{self, DEFAULT_TEMPLATE, TemplateVars};
```

`DEFAULT_TEMPLATE` is `{repo_parent}/{repo}.worktrees/{repo}-{branch_slug}`, but
a repository's `.wt.toml` may set `path_template` to anything, using
`{repo_parent}`, `{repo}`, `{repo_root}`, `{branch}`, `{branch_slug}` and
`{home}`.

**Resolve paths through this library — never reimplement the template.** Two
tools that guess independently will disagree about where a repository's worktrees
are, and the user is the one who finds out. `Workspace::create` already renders
through `template::render` and reports the resulting path, which is the simplest
way to stay consistent; `template::render` itself is there for resolving a path
before creating anything.

### The metadata contract

`wt` records per-branch state in the repository's git config under
`wt.<branch>.*`. Read it with `Workspace::read_meta` and write it with
`Workspace::write_meta`:

| Key | Meaning |
| --- | --- |
| `baseRef` | The ref the branch was created from |
| `createdByWt` | `wt` created the branch, so `wt` may delete it |
| `prNumber`, `prState`, `prTitle`, `prUrl` | The originating PR, cached so listing works offline |
| `issueNumber`, `issueTitle`, `issueUrl` | The linked GitHub issue |
| `issueBrief` | The implementation brief `wt issue` generated |

Reads map a missing key to `None` and ignore unknown keys, so *adding* a key
never breaks an older reader, and an embedder can keep its own state in its own
config namespace without `wt` disturbing it. `MetaUpdate` writes only its `Some`
fields, so refreshing one key cannot clobber the rest.

Changing what an existing key *means* is the case that needs coordination, and
`wt.schema` gates it. A repository with no `wt.schema` is version 1;
`wt::worktree::SCHEMA_VERSION` is what this build understands. A repository
stamped **higher** than that is refused with `Error::SchemaTooNew` rather than
read with the wrong meanings — surface it as "upgrade the tool", not as a
corrupt repository. `Workspace::discover` performs this check, and the mutating
operations repeat it.

### Locking

Mutations are serialized across every `wt` process and embedder sharing a
repository by an advisory lock — a `wt-mutation.lock` marker in the common git
directory, waited on for up to 10 seconds before failing with
`Error::LockUnavailable`.

`create`, `remove`, `write_meta` and `clear_meta` take it internally, so **do not
hold a lock across a call to them** — it is not reentrant, and doing so waits out
the full timeout and then fails. Take one yourself, via `Workspace::lock`, only to
make a longer read-check-write sequence over `wt.*` metadata atomic; drop it
before calling back into the service.

Hooks deliberately run *outside* the lock, so a `post_create` or `pre_remove`
hook that re-enters `wt` cannot deadlock against the operation that invoked it.

Call `wt::install_signal_handlers()` once, early. The lock is released on drop,
which a terminating signal skips — stranding the marker so the next mutation
waits out its whole timeout. The handlers clean it up and re-raise. (`SIGHUP` is
not covered.)

### Generation and work

`wt` owns the short, structured generation steps it needs for its own proposals —
the `wt pr open --ai` draft and the `wt issue` branch/brief — configured under
`[agent.generation]`. It deliberately owns nothing else: running a coding agent on
the work belongs to the embedder, which is why `[agent.work]` is rejected rather
than accepted and ignored.

`wt issue` reflects the same split. It creates the worktree, records the issue
link and persists the brief, then stops. Handing the work to an agent is the
embedder's step, and the persisted `issueBrief` means it need not pay to
regenerate what `wt` already produced.

## Development

### Prerequisites

- [Rust (rustup)](https://rustup.rs) — toolchain (pinned via `rust-toolchain.toml`)
- [mise](https://mise.jdx.dev) — tool manager + task runner
- [hk](https://hk.jdx.dev) — git hooks manager

Run `mise install` once to fetch the pinned dev tools (`hk`, `pkl`,
`cargo-llvm-cov`, `cargo-mutants`).

| Command             | Description                            |
| ------------------- | -------------------------------------- |
| `mise tasks`        | List available tasks                   |
| `cargo run`         | Run the application                    |
| `mise run install`  | Build and install `wt` to ~/.cargo/bin |
| `mise run test`     | Run tests                              |
| `mise run format`   | Format code                            |
| `mise run lint`     | Lint with Clippy (warnings as errors)  |
| `mise run lint-fix` | Lint and auto-fix                      |
| `mise run coverage` | Run tests with coverage (min 80%)      |

After cloning, run `mise install` to fetch the dev tools, then `hk install`
once to activate the git hooks.

### Tech Stack

- **Runtime:** Rust (edition 2024)
- **Formatter:** rustfmt
- **Linter:** Clippy
- **Task runner:** mise
- **Git hooks:** hk
- **Key Dependencies:** tokio, eyre + color-eyre, tracing + tracing-subscriber, thiserror

### Architecture

The logic lives in the library crate (`src/lib.rs`) so it is unit-testable and
measured by coverage. The binary (`src/main.rs`) is a thin entry point that
wires up error reporting and tracing, then delegates to the library; it is
excluded from coverage.

### Git Hooks

This project uses [hk](https://hk.jdx.dev), configured in `hk.pkl`.
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
mise run coverage
```

## License

MIT — see [LICENSE](LICENSE) for details.
