# `wt` — Git Worktree Manager

**Version:** 1.0 (specification)
**Status:** Requirements specification — defines *what* `wt` must do, not *how* to implement it.

---

## 1. Overview

`wt` is a single-binary CLI + TUI for managing Git worktrees and their associated
GitHub pull requests. It removes the friction of the raw `git worktree` workflow:
deciding where worktrees live, creating a branch and worktree in one step, jumping
between them, seeing their status at a glance, checking out PRs into isolated
directories, and cleaning up when work merges.

The product is for developers who keep several lines of work in flight at once
(feature branches, PR reviews, long builds, hotfixes) and want each in its own
checkout without stashing or re-cloning.

### Design pillars

1. **Git is the source of truth.** `wt` never maintains a parallel database of which
   worktrees exist. State is *derived* from Git on every invocation, so worktrees
   created or removed with plain `git` (or another tool) are always visible and
   manageable, and `wt`'s view can never desync.
2. **Right tool for each job** (see §4): read operations use `gitoxide`; worktree
   mutations and network operations shell out to `git`; pull-request operations
   shell out to `gh`.
3. **Fast and offline by default.** Listing, status, and switching never touch the
   network. Only explicit PR and fetch operations do.
4. **Composable.** Machine-readable output where it matters (`--json`, path-only
   output for navigation) so `wt` slots into scripts and shell functions.

---

## 2. Goals and Non-Goals

### Goals
- One-step creation of a worktree + branch from any base ref.
- Instant navigation between worktrees from the shell (`cd` into them).
- A live TUI dashboard of all worktrees with status, ahead/behind, and PR info.
- First-class GitHub PR checkout into dedicated worktrees, via `gh`.
- Safe removal and bulk cleanup of merged/stale worktrees.
- Shell completion (static + dynamic) for Bash, Zsh, Fish, PowerShell, Elvish.
- Per-repo and global configuration, including auto-copying of ignored files and
  post-create hooks.

### Non-Goals
- `wt` is **not** a Git porcelain replacement. It does not commit, merge, rebase,
  push, or resolve conflicts. Those remain the user's job via `git`.
- No hosting-provider abstraction beyond GitHub in 1.0 (GitHub via `gh` only).
- No daemon or background process.
- No management of submodules' internal worktrees.

---

## 3. Core Concepts and Terminology

- **Repository** — the Git repository `wt` is operating on, discovered from the
  current directory upward. May be a normal repo (with a primary worktree) or a
  **bare** repo used as a worktree hub. Both must be supported.
- **Primary worktree** — the original checkout (the one holding `.git`, or the
  bare repo's conceptual root). Never removable by `wt`.
- **Linked worktree** — any additional worktree. These are what `wt` creates and
  manages.
- **Worktree store** — the directory layout under which `wt` places new linked
  worktrees (§6). Worktrees outside the store are still listed and manageable;
  the store only governs where *newly created* ones go.
- **Branch slug** — a filesystem-safe rendering of a branch name used for directory
  names (e.g. `feature/login` → `feature-login`). The real branch name is always
  preserved in Git; the slug only affects the directory path.

---

## 4. Architecture: the `git` / `gh` / `gitoxide` boundary

This boundary is a hard requirement, chosen to match each library's maturity.

**Use `gitoxide` (`gix`) for all read operations:**
- Repository discovery and root resolution; detecting bare vs. normal repos.
- Enumerating worktrees and the branch/commit each has checked out.
- Listing local and remote branches; resolving refs and revspecs.
- Reading commit metadata for display (subject, author, timestamp, short hash).
- Computing ahead/behind counts via commit-graph traversal.
- Detecting working-tree dirtiness (status).
- Reading and writing repository config under the `wt.*` namespace, and reading
  Git config needed to resolve the default branch and remotes.

**Use the `git` CLI (subprocess) for state-mutating and network operations**, because
`gitoxide`'s support for these is not yet stable:
- `git worktree add | remove | move | prune | list` (worktree lifecycle).
- Branch creation when coupled to checkout (`git worktree add -b …`).
- Any network operation: `fetch`, `pull`, `push`.

**Use the `gh` CLI (subprocess) for all GitHub pull-request operations:**
- Listing PRs, viewing PR metadata/status, resolving a PR to its head ref, and
  fetching PR branches (including from forks). `gh` provides authentication.

**Rule of thumb for implementers:** if an operation only *reads* repository data,
it should go through `gix`; if it *changes* the working tree, refs over the network,
or touches GitHub, it shells out. The exact `gix` capability set evolves between
releases — implementers should verify the current crate status and may fall back to
`git` subprocess + porcelain parsing for any read that is not yet stable in the
pinned `gix` version (status is the most likely such case). Subprocess invocations
must capture stderr and surface `git`/`gh` errors verbatim in `wt`'s error output.

**Dependency preconditions:** `git` (≥ 2.20, for stable `worktree` semantics) must be
on `PATH`. `gh` is required only for PR commands; if absent or unauthenticated, PR
commands fail with an actionable message and all other commands continue to work.

---

## 5. Shell Integration (the `cd` problem)

A child process cannot change its parent shell's working directory. Navigation is
therefore the one feature that requires shell integration, and the contract below
is a hard requirement.

- **Output discipline:** for navigation commands, `wt` prints **only the resolved
  absolute path** to **stdout**, and nothing else. All human-facing UI, prompts,
  the TUI, logs, and errors go to **stderr**. This lets a shell wrapper safely
  `cd "$(...)"`.
- **`wt shell-init <shell>`** prints a shell snippet the user sources from their
  rc file. The snippet defines a `wt` shell function that:
  - intercepts navigation subcommands (`switch`, the no-arg TUI launch) and the
    selection result, capturing stdout and running `cd` into it on success;
  - passes every other subcommand straight through to the binary unchanged;
  - wires up completion (§9).
- The underlying binary must remain fully usable without the wrapper; only the
  automatic `cd` is lost. A `wt switch --print-path` form must exist so users on
  unsupported shells can build their own `cd` alias.
- Supported shells for `shell-init`: Bash, Zsh, Fish, PowerShell, Elvish.

---

## 6. Worktree Store and Path Layout

New worktrees are placed according to a configurable **path template**. The default
keeps worktrees adjacent to the repo but out of it:

```
default: {repo_parent}/{repo}.worktrees/{branch_slug}
```

Available template variables: `{repo_parent}` (dir containing the repo root),
`{repo}` (repo directory name), `{repo_root}`, `{branch}` (raw), `{branch_slug}`,
`{home}`. Implementers must reject templates that would place a worktree inside the
`.git` directory.

Common presets the docs should call out:
- **Sibling (default):** `{repo_parent}/{repo}.worktrees/{branch_slug}`
- **Subdir:** `{repo_root}/.worktrees/{branch_slug}` (recommend adding to `.gitignore`)
- **Central:** `{home}/worktrees/{repo}/{branch_slug}`

Requirements:
- Creating a worktree must create intermediate directories as needed.
- A collision (target path exists) is an error unless the path already *is* the
  worktree for that branch (idempotent no-op, reported as such).
- The store layout governs only creation. Listing, status, switch, and remove must
  work for worktrees at any location, including the primary worktree and ones made
  by hand.

---

## 7. CLI Command Surface

General conventions: every command accepts `--json` for machine-readable output
(except where it makes no sense), `-h/--help`, and respects config + flags (flags
win). Commands that resolve a worktree accept a **query** that matches by branch
name, slug, directory name, or unambiguous prefix; an ambiguous query lists the
candidates and exits non-zero (or, if interactive, opens the picker).

### `wt new <branch> [--from <ref>] [--no-switch] [--no-hooks]`
Create a linked worktree.
- If `<branch>` exists locally: create a worktree checking it out.
- If it does not exist: create it from `--from` (default: the repo's default branch,
  resolved via remote `HEAD`/config; fall back to current `HEAD`).
- Refuse if the branch is already checked out in another worktree (Git enforces
  this; surface a clear message naming that worktree).
- After creation: run the copy step (§8) and post-create hook unless suppressed,
  then print the new worktree's path to stdout so the shell wrapper switches into
  it (unless `--no-switch`).

### `wt list` (alias `ls`)
List worktrees. Default human output is a compact table: branch, slug/dir, dirty
marker, ahead/behind vs. upstream, last-commit summary, and PR number/state if
known. `--json` emits the full structured record per worktree.

### `wt switch [<query>]` (alias `sw`)
Navigate to a worktree. With a query, resolve and print its path. With no query,
open the TUI picker (§10) and print the chosen path. This is a navigation command:
path-only on stdout, UI on stderr. `--print-path` forces print-only even inside the
shell wrapper.

### `wt remove <query> [--force] [--keep-branch]` (alias `rm`)
Remove a linked worktree.
- Refuse if the working tree is dirty or has unpushed work, unless `--force`.
- Never removes the primary worktree.
- By default also deletes the local branch **only if** it is fully merged and was
  created by `wt`; otherwise the branch is kept. `--keep-branch` always keeps it;
  `--force` permits deleting an unmerged branch.
- Run the pre-remove hook (§8) before deletion.

### `wt prune [--merged] [--gone] [--dry-run] [--force]`
Bulk cleanup. Candidates:
- `--merged`: worktrees whose branch is merged into the default branch.
- `--gone`: worktrees whose upstream branch no longer exists on the remote.
- Also reconciles Git's worktree admin metadata (equivalent to `git worktree prune`).
Always shows what will be removed and asks for confirmation unless `--force`;
`--dry-run` only reports. Dirty worktrees are skipped unless `--force`.

### `wt pr <number | url | branch>` (alias for checkout)
Check out a GitHub PR into its own worktree via `gh`.
- Resolve the PR (including fork PRs) to a head ref, fetch it, and create a worktree
  for it under the store. Record the PR number in `wt.*` config for that worktree
  so `list`/TUI can show PR state.
- With no argument, open an interactive PR list (via `gh`) to choose from.
- `wt pr list` prints open PRs (table or `--json`); selecting one checks it out.
- Switches into the new worktree on success (same stdout path contract).

### `wt status [<query>]`
Detailed status for one worktree (default: current) or, with `--all`, every
worktree: dirty files summary, ahead/behind, upstream, and PR state.

### `wt path <query>`
Print the absolute path of a matching worktree to stdout (scripting helper; no `cd`).

### `wt root`
Print the repository root (primary worktree / bare repo path).

### `wt init [--path-template <tmpl>]`
Initialize `wt` for the current repo: write per-repo config defaults and, for the
subdir layout, offer to add the store directory to `.gitignore`. Idempotent.

### `wt config <get|set|list|edit> [key] [value]`
Read or modify configuration (§11). `--global` targets the user config; default is
per-repo.

### `wt completions <shell>`
Print a completion script for the given shell (§9).

### `wt shell-init <shell>`
Print the shell-integration snippet, which includes the completion wiring (§5, §9).

### `wt ui` (alias `tui`)
Launch the TUI explicitly (also the behavior of `wt` with no subcommand).

---

## 8. Copy Rules and Hooks

Newly created worktrees start from a clean checkout and therefore lack
Git-ignored local files (`.env`, local config, build caches) that the user often
needs. `wt` addresses this:

- **Copy patterns:** a configurable list of glob patterns. On `new`/`pr`, matching
  files/dirs are copied from a **source worktree** (default: the worktree the user
  ran the command from, falling back to the primary worktree) into the new one.
  Patterns matching tracked files are ignored (no need to copy those). Copying must
  never overwrite existing files in the target.
- **Hooks:** optional shell commands run with the new worktree as CWD and useful
  context in the environment (e.g. `WT_WORKTREE_PATH`, `WT_BRANCH`, `WT_REPO_ROOT`,
  `WT_BASE_REF`, and for PRs `WT_PR_NUMBER`):
  - `post_create` — after creation + copy (e.g. install deps, `direnv allow`).
  - `pre_remove` — before a worktree is removed.
  A non-zero `post_create` exit is reported as a warning; the worktree is still
  created. `--no-hooks` skips hooks for a single command.

---

## 9. Completion

Completion is a first-class requirement, both static and dynamic.

- **Static scripts** via `wt completions <shell>` for Bash, Zsh, Fish, PowerShell,
  and Elvish: complete subcommands, flags, and enumerated values (e.g. shell names).
- **Dynamic value completion** for context-dependent arguments:
  - `switch`, `remove`, `status`, `path` → complete existing **worktree** names
    (branch / slug / dir).
  - `new --from`, and `new <branch>` → complete existing **branch** names.
  - `pr` → complete open PR numbers when `gh` is available (best-effort; must not
    block or error the shell if `gh` is missing).
- Dynamic completion must be fast (sourced from `gix` reads, not the network) and
  must degrade silently if invoked outside a repository.
- `wt shell-init` installs completion automatically so users get it from one line
  in their rc file; `wt completions` remains available for manual setup.

---

## 10. TUI

Launched by `wt` (no args) or `wt ui`. It is a live dashboard and action center.

**Layout**
- A list of all worktrees, each row showing: branch, dirty indicator,
  ahead/behind, last-commit summary + relative time, and PR number/state if known.
- A detail/preview pane for the selected worktree (path, upstream, fuller status,
  recent commits, PR title/state).
- A status/help line showing key bindings and current filter.

**Behavior**
- Status, ahead/behind, and PR data load **asynchronously** so the UI is
  interactive immediately and never blocks on slow repos or `gh`; rows fill in as
  data arrives, with a clear loading indicator.
- Mouse optional; keyboard-first. Suggested default bindings (configurable):
  - `↑/↓` or `j/k` navigate; `Enter` switch (selects worktree → prints path → shell
    `cd`s, then TUI exits).
  - `/` filter (fuzzy match on branch/slug/path); `Esc` clear.
  - `n` new worktree (prompt for branch + base); `d` remove (with confirm + dirty
    guard); `p` PR picker / checkout; `o` open in `$EDITOR` or configured editor;
    `r` refresh; `?` help; `q` quit without switching.
- Destructive actions always confirm and honor the dirty/merge guards from §7.
- The TUI must restore the terminal cleanly on exit, including on error or signal.

---

## 11. Configuration

Two layers, merged with **flags > per-repo > global > built-in defaults**:
- **Global:** `$XDG_CONFIG_HOME/wt/config.toml` (and the platform-appropriate
  equivalent on macOS/Windows).
- **Per-repo:** a `wt` config file at the repo root, committed or not at the user's
  discretion. Per-repo settings override global.
- Per-worktree metadata that `wt` itself records (e.g. originating PR number,
  "created by wt") lives in Git config under the `wt.*` namespace, not in these
  files — keeping it tied to Git's own state.

**Configurable keys (names illustrative):**
- `path_template` — worktree store template (§6).
- `default_base` — base ref for `new` when branch is created (default: resolved
  default branch).
- `copy` — list of glob patterns to copy into new worktrees (§8).
- `hooks.post_create`, `hooks.pre_remove` — commands (§8).
- `editor` — command used by TUI "open" (falls back to `$VISUAL`/`$EDITOR`).
- `remove.delete_merged_branch` — whether `remove` deletes merged wt-created
  branches by default (default: true).
- `pr.default_remote` — remote to use for PR fetches (default: `origin`).
- `ui.*` — TUI preferences (default columns, key bindings, color theme honoring
  `NO_COLOR`).

`wt config edit` opens the appropriate file; invalid config produces a precise
error (file, key, reason) and never silently ignores keys.

---

## 12. Safety, Errors, and Edge Cases

These behaviors are required, not optional polish:

- **Not in a repo:** any repo-scoped command exits non-zero with a clear message;
  completion degrades silently.
- **Branch already checked out elsewhere:** report which worktree holds it; do not
  attempt a duplicate checkout (Git forbids it).
- **Dirty worktree on remove/prune:** refuse without `--force`, and on `--force`
  state plainly that uncommitted work may be lost.
- **Path collision on create:** error unless it is the same branch's existing
  worktree (then idempotent no-op).
- **Stale admin metadata:** `prune` (and a best-effort check on `list`) reconciles
  Git's internal worktree records so manually-deleted directories don't linger.
- **`gh` missing/unauthenticated:** only PR commands fail, with a message pointing
  to `gh auth login`; everything else works.
- **Detached HEAD / no default branch:** fall back to current `HEAD` as base and
  warn.
- **Subprocess failures:** surface the underlying `git`/`gh` stderr; do not swallow.
- **Exit codes:** `0` success; `1` user/operation error; `2` usage/argument error;
  reserve a distinct code for "ambiguous query / nothing selected" so scripts and
  the shell wrapper can avoid a spurious `cd`.
- **Concurrency:** rely on Git's own locking for worktree mutations; detect and
  report lock contention rather than corrupting state.

---

## 13. Non-Functional Requirements

- **Single self-contained binary**, no runtime dependencies beyond `git` (always)
  and `gh` (PR commands only).
- **Platforms:** Linux and macOS are first-class; Windows is supported on a
  best-effort basis (path handling, shell snippets for PowerShell).
- **Performance:** `list` and the initial TUI paint on a repo with dozens of
  worktrees complete near-instantly by using `gix` reads and avoiding the network;
  no per-worktree subprocess fan-out for read-only listing.
- **Startup:** no network access on any command except explicit fetch/PR operations.
- **Output:** human output is concise and respects `NO_COLOR` and non-TTY stdout
  (auto-plain). `--json` output is stable and documented.
- **Reliability:** all mutations are atomic from the user's perspective — on failure
  partway through `new` (e.g. hook fails), the worktree state is left consistent and
  the failure is reported; nothing is half-created without notice.
- **Testability:** behavior is verifiable against real temporary repositories;
  subprocess boundaries (`git`, `gh`) are isolatable for testing.

---

## 14. Acceptance Criteria (summary)

A conforming implementation must let a user, from inside any Git repository:
1. Run one source line in their rc file (`wt shell-init`) and thereafter `cd` into
   worktrees via `wt switch`/the TUI.
2. `wt new feature/x` → get a branch + worktree under the configured store, with
   ignored files copied and a post-create hook run, landing in that directory.
3. `wt pr 123` → get that PR (even from a fork) checked out in its own worktree.
4. `wt list` / the TUI → see every worktree (including hand-made ones) with dirty
   state, ahead/behind, and PR status, loaded without blocking.
5. `wt remove`/`wt prune` → safely clean up, with dirty-work and merge guards.
6. Receive working completion for subcommands, worktree names, branch names, and
   PR numbers in their shell.
7. Observe that worktrees created/removed with plain `git` are always reflected,
   confirming Git remains the single source of truth.
