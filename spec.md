# `wt` — Git Worktree Manager

**Version:** 1.1 (specification)
**Status:** Requirements specification — defines *what* `wt` must do, not *how* to implement it.

> **Changelog — 1.1:** consistency pass. Closed audit findings: unified TUI
> keybinding action names (§10/§11), named the per-repo config file `.wt.toml`,
> enumerated `list.columns` identifiers and `pr.state` values, defined exit code `3`
> (ambiguous query / nothing selected), fixed `status --all --json` to be
> newline-delimited, specified JSON values for detached-HEAD and missing worktrees,
> defined hook shell/env/failure semantics, paging, color precedence, the copy source,
> `prune --gone` offline behavior, and the `new`/`pr`/`switch` `--json` vs path-only
> contract.

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
  preserved in Git; the slug only affects the directory path. Normalization rules:
  (1) replace `/` and `\` with `-`; (2) replace any run of characters outside
  `[a-zA-Z0-9.-]` with `-`; (3) collapse consecutive `-` into one; (4) strip
  leading/trailing `-`; (5) if the result is empty, fall back to the short commit
  hash of the base ref.
- **Missing worktree** — a worktree whose Git admin record exists but whose
  directory has been deleted externally. Distinct from a stale record (`git worktree
  prune` cleans those): the worktree is *known* but its path is gone. `wt` surfaces
  missing worktrees with a distinct visual marker and handles them gracefully in
  `remove` and `prune` (see §12).
- **Upstream branch** — the remote tracking branch configured for a local branch
  (e.g. `origin/feature/x`). Used for ahead/behind display and "gone" detection.
- **Base ref** — the ref a branch was created from, recorded in the `wt.*` Git
  config namespace at creation time. Used for "fully merged" checks in `remove` and
  for display in `wt status`. Distinct from the upstream branch.

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

- **Navigation commands** are the subset that exist to move the shell: `switch`,
  `new`, `pr`, and any TUI launch (`wt` with no subcommand, `wt ui`, `wt tui`). For
  these, `wt` prints **only the resolved absolute path** to **stdout**, and nothing
  else. All human-facing UI, prompts, the TUI, logs, and errors go to **stderr**. This
  lets a shell wrapper safely `cd "$(...)"`.
- **Data commands** (`list`, `status`, `path`, `root`, `config`, …) also write their
  result to stdout, but the wrapper does **not** `cd` into it — `path` and `root` are
  scripting helpers, not navigation. The wrapper distinguishes the two sets by
  subcommand name.
- **`--json` suppresses `cd`.** When a navigation command is invoked with `--json`
  (only `new`/`pr` accept it; see §7), it emits a JSON object to stdout instead of a
  bare path, and the wrapper must not `cd`. `switch`, `path`, and `root` do not accept
  `--json`.
- **`wt shell-init <shell>`** prints a shell snippet the user sources from their
  rc file. The snippet defines a `wt` shell function that:
  - intercepts the navigation subcommands above, capturing stdout and running `cd`
    into it on success (exit code `0` with a non-empty path; see §12 — exit `3` or an
    empty path means "nothing selected", so the wrapper must not `cd`);
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
default: {repo_parent}/{repo}.worktrees/{repo}-{branch_slug}
```

Available template variables: `{repo_parent}` (dir containing the repo root),
`{repo}` (repo directory name), `{repo_root}`, `{branch}` (raw), `{branch_slug}`,
`{home}`. For a **bare** repo these resolve against the bare repository's own
directory: `{repo_root}` is the bare repo path, `{repo}` its directory name, and
`{repo_parent}` its containing directory.

Common presets the docs should call out:
- **Sibling (default):** `{repo_parent}/{repo}.worktrees/{repo}-{branch_slug}`
- **Subdir:** `{repo_root}/.worktrees/{branch_slug}` (recommend adding to `.gitignore`)
- **Central:** `{home}/worktrees/{repo}/{branch_slug}`

Requirements:
- Creating a worktree must create intermediate directories as needed.
- **Collision handling** when the rendered target path already exists:
  - It already *is* the worktree for this same branch → idempotent no-op, reported as
    such (exit `0`).
  - It is another branch's worktree, or a plain (non-worktree) directory/file → first
    attempt to disambiguate by appending `-<short-hash>` (the short commit hash of the
    base ref) to the `{branch_slug}` segment; if that path is *also* occupied, fail
    with an error naming the conflicting path.
  - This also covers two distinct branch names that normalize to the same slug (e.g.
    `feature/login` and `feature_login`): the second one created gets the
    `-<short-hash>` suffix.
- Reject templates that would place a worktree inside the `.git` directory.
- The store layout governs only creation. Listing, status, switch, and remove must
  work for worktrees at any location, including the primary worktree and ones made
  by hand.

---

## 7. CLI Command Surface

General conventions: most commands accept `--json` for machine-readable output,
`-h/--help`, and respect config + flags (flags win).

**`--json` support is per command:**

| `--json` supported | `--json` rejected (errors, or no effect) |
|--------------------|-------------------------------------------|
| `list`, `status`, `pr list`, `prune` (reports the candidate/removed set), `config list`, `new`, `pr`, `remove` | `switch`, `path`, `root`, `completions`, `shell-init`, `ui`/`tui`, `init` |

`new`, `pr`, and `remove` emit a single result object (the affected worktree's
`list`-row schema, plus a `removed: true` flag for `remove`) rather than human text;
for `new`/`pr` this replaces the bare path on stdout and tells the shell wrapper not to
`cd` (§5).

**Query resolution.** Commands that resolve a worktree accept a **query** matched in
this precedence order, first non-empty tier wins:
1. exact branch name, then exact slug, then exact directory name;
2. unambiguous prefix across branch / slug / directory name.

If a single tier yields more than one match the query is **ambiguous**: in an
interactive context (stderr is a TTY) `wt` opens the picker (§10); otherwise it lists
the candidates on stderr and exits with code `3` (§12). A query that matches nothing
exits `1`.

### Global flags

These flags are accepted by every subcommand:

| Flag | Description |
|------|-------------|
| `--json` | Machine-readable output (JSON). Stable schema; see per-command docs. |
| `--color <auto\|always\|never>` | Control ANSI color. Default `auto` (color when stdout is a TTY). Respects `NO_COLOR` env var. |
| `--no-pager` | Never page output (useful for scripting). |
| `-C <path>` | Run as if invoked from `<path>` (mirrors `git -C`). |
| `-v` / `--verbose` | Emit additional diagnostic output to stderr. Stackable (`-vv`). |

### `wt new <branch> [--from <ref>] [--no-switch] [--no-hooks] [--copy-from <query>]`
Create a linked worktree.
- If `<branch>` exists locally: create a worktree checking it out.
- If it does not exist: create it from `--from` (default: the repo's default branch,
  resolved via remote `HEAD`/config; fall back to current `HEAD`).
- Refuse if the branch is already checked out in another worktree (Git enforces
  this; surface a clear message naming that worktree).
- After creation: run the copy step (§8) and post-create hook unless suppressed,
  then print the new worktree's path to stdout so the shell wrapper switches into
  it (unless `--no-switch`).
- `--copy-from <query>` overrides the source worktree for the copy step (§8),
  resolving by the same query rules as other worktree-selecting commands.

### `wt list` (alias `ls`)
List worktrees. Default human output is a compact table with the following columns
in order:

| Column | Content |
|--------|---------|
| Status | `*` current worktree; `!` directory missing; `~` detached HEAD; space otherwise |
| Dirty | `M` if modified/staged tracked files; `?` if untracked files present; empty otherwise |
| Branch | Full branch name, or `(HEAD detached @ <hash>)` |
| Path | Relative path from repo root, or absolute if outside |
| ↑↓ (`ahead-behind`) | `↑N ↓M` commits ahead/behind upstream; `–` if no upstream tracking branch |
| Commit | Short hash + subject (truncated to fit) + relative timestamp |
| PR | `#N (state)` if a PR is recorded for this worktree; empty otherwise |

Additional flags:
- `--sort <field>` — sort by `branch` (default), `dirty`, `ahead`, `behind`,
  `activity` (most-recent commit first), or `path`. Prefix with `-` for descending
  order (e.g. `--sort -ahead`). Semantics: `dirty` orders modified/staged worktrees
  first, then untracked-only, then clean; `ahead`/`behind` sort by the numeric count.
  Worktrees with no value for the sort key (no upstream for `ahead`/`behind`, or a
  missing worktree for `dirty`/`activity`) sort **last** regardless of direction.
- `--filter <query>` — non-interactive fuzzy filter by branch/slug/path; same
  matching logic as the TUI `/` filter. Useful in scripts.

`--json` emits one JSON object per worktree (newline-delimited) with the following
stable fields:

```json
{
  "schema_version": 1,
  "path": "/absolute/path",
  "branch": "feature/login",
  "slug": "feature-login",
  "is_current": true,
  "is_main": false,
  "is_missing": false,
  "is_detached": false,
  "dirty": true,
  "has_untracked": false,
  "ahead": 2,
  "behind": 0,
  "upstream": "origin/feature/login",
  "base_ref": "main",
  "commit": {
    "hash": "abc1234",
    "subject": "Add login page",
    "author": "Alice",
    "timestamp": "2024-01-15T10:30:00Z"
  },
  "pr": { "number": 42, "state": "open", "title": "Add login page" }
}
```

Field rules (apply to `list`, `status`, and the `new`/`pr`/`remove` result object):
- `schema_version` is an integer, currently `1`; it is bumped only on a
  breaking change so consumers can detect incompatibility (§13).
- `pr` is `null` if no PR is recorded. `pr.state` is one of `open`, `closed`,
  `merged`, `draft` (mirrors `gh`).
- `ahead` and `behind` are `null` (not `0`) when no upstream tracking branch is
  configured. `upstream` and `base_ref` are `null` when not set.
- **Detached HEAD:** `branch` is `null` and `is_detached` is `true`. The
  `(HEAD detached @ <hash>)` rendering is human-output only.
- **Missing worktree** (directory deleted externally): `is_missing` is `true` and the
  fields that require the working tree — `dirty`, `has_untracked`, `ahead`, `behind`,
  `commit` — are `null` (they cannot be computed). `branch`, `slug`, `upstream`,
  `base_ref`, and `pr` are still populated from Git's admin records and `wt.*` config
  when available.

**Display conventions** (apply wherever `wt` renders commits and times — `list`,
`status`, TUI): short commit hashes are 7 characters, honoring `core.abbrev` when the
repo sets it. Relative timestamps render as a single largest unit
(`just now`, `5m ago`, `3h ago`, `2d ago`, `4mo ago`, `1y ago`).

### `wt switch [<query>]` (alias `sw`)
Navigate to a worktree. With a query, resolve and print its path. With no query,
open the TUI picker (§10) and print the chosen path. This is a navigation command:
path-only on stdout, UI on stderr. `--print-path` forces print-only even inside the
shell wrapper.

### `wt remove <query> [--force] [--keep-branch] [--no-hooks]` (alias `rm`)
Remove a linked worktree.
- Refuse if the worktree is **dirty** or has **unpushed work**, unless `--force`
  (see §12 for the precise, shared definitions). "Dirty" = modified or staged tracked
  files (untracked files only when `remove.untracked_blocks` is set); "unpushed work" =
  local commits ahead of the upstream (`ahead > 0`). A branch with no upstream is
  treated as having unpushed work and is not removed without `--force`.
- Never removes the primary worktree.
- By default also deletes the local branch **only if** it is fully merged and was
  created by `wt`; otherwise the branch is kept. "Fully merged" means merged into the
  branch's recorded **base ref** (§3), falling back to the repo's **default branch**
  when no base ref is recorded. (`prune --merged` deliberately uses the default branch
  instead — see below.) `--keep-branch` always keeps it; `--force` permits deleting an
  unmerged branch. "Created by `wt`" is recorded in `wt.*` config at creation (§11).
- Run the pre-remove hook (§8) before deletion, unless `--no-hooks`. A non-zero
  pre-remove hook **aborts** the removal (exit `1`) unless `--force`, in which case it
  is reported as a warning and removal proceeds (§8).
- If the worktree directory is already missing, skip the `git worktree remove` step
  and run `git worktree prune` instead to clean the admin record. `--force` is not
  required in this case, and the dirty/unpushed guards do not apply (nothing to check).

### `wt prune [--merged] [--gone] [--dry-run] [--force]`
Bulk cleanup. Candidates:
- `--merged`: worktrees whose branch is merged into the repo's **default branch**.
  (This differs intentionally from `remove`'s per-branch base-ref check: bulk cleanup
  uses one consistent target, the default branch.)
- `--gone`: worktrees whose upstream branch is gone, **and** worktrees whose directory
  is missing (see "missing worktree" in §3). "Gone" is determined **offline** from
  Git's locally-cached upstream tracking state (the `gone` marker left by a previous
  `git fetch --prune`); `wt` never touches the network here. To refresh which upstreams
  are gone, the user runs `git fetch --prune` first.
- Also reconciles Git's worktree admin metadata (equivalent to `git worktree prune`).
Always shows what will be removed and asks for confirmation unless `--force`: a CLI
list of the candidate worktrees followed by a single `Proceed? [y/N]` prompt on stderr
(not the TUI confirm dialog). `--dry-run` only reports (and is the `--json` form's
behavior — it prints the candidate set without removing). Dirty worktrees are skipped
unless `--force`.

### `wt pr <number | url | branch> [--no-switch] [--no-hooks]` (alias for checkout)
Check out a GitHub PR into its own worktree via `gh`.
- Accepted forms: a PR **number** (`123`), a GitHub PR **URL**
  (`https://github.com/<owner>/<repo>/pull/<n>`), or the PR's **head-branch** name.
- Resolve the PR (including fork PRs) to a head ref, fetch it, and create a worktree
  for it under the store. Record the PR number in `wt.*` config for that worktree
  so `list`/TUI can show PR state, and record the PR's **base branch** as the
  worktree's `base_ref` (§3).
- With no argument, open the interactive PR picker (TUI, §10) that lists open PRs via
  `gh` and **checks out** the chosen one.
- `wt pr list` is the non-interactive listing form: it prints open PRs (table or
  `--json`) and does **not** check anything out.
- Switches into the new worktree on success (same stdout path contract, §5).

### `wt status [<query>]`
Detailed status for one worktree (default: current) or, with `--all`, every
worktree: dirty files summary, ahead/behind, upstream, base ref, and PR state.

Human output format (one block per worktree):

```
worktree: /path/to/tree
branch:   feature/x → origin/feature/x
base:     main
ahead:    3  behind: 0
pr:       #42 (open) "Add login page"
dirty:
  M  src/main.rs
  M  src/lib.rs
  ?  scratch.txt
```

When the upstream is not configured, `branch:` shows `feature/x (no upstream)` and
`ahead`/`behind` are omitted. `pr:` is omitted when no PR is recorded. `base:` is
omitted when not recorded. For a **missing** worktree the block shows `path`,
`branch`, and `base`/upstream when known, plus the note `(directory already deleted)`,
and omits the `dirty:`, `ahead`, and `behind` sections (they cannot be computed).

`--json` output uses the same per-worktree schema as `wt list --json`. Without
`--all`, a single object is emitted for the resolved worktree. With `--all`, objects
are **newline-delimited**, one per worktree — identical framing to `wt list --json`
(never a wrapping array).

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
  files/dirs are copied from a **source worktree** into the new one. The default
  source is the worktree containing the current working directory (after any `-C`); if
  the CWD is not inside any worktree, it falls back to the primary worktree.
  `--copy-from <query>` overrides the source. Patterns matching tracked files are
  ignored (no need to copy those). Copying must never overwrite existing files in the
  target; a file skipped because the target already exists is silent at default
  verbosity and logged at `-v`.
- **Hooks:** optional shell commands run with the new worktree as CWD. The command is
  executed via `sh -c "<cmd>"` on Unix and `cmd /C "<cmd>"` on Windows, inheriting the
  parent process environment plus these `WT_*` variables:
  - `WT_WORKTREE_PATH`, `WT_BRANCH`, `WT_REPO_ROOT` — always set.
  - `WT_BASE_REF` — set when a base ref is known.
  - `WT_PR_NUMBER` — set **only** for PR-originated worktrees; **unset** otherwise, so
    a hook can branch on its presence.

  The two hooks:
  - `post_create` — after creation + copy (e.g. install deps, `direnv allow`). A
    non-zero exit is reported as a **warning** on stderr; the worktree is still
    created and the command exits `0` (the hook failure is non-fatal).
  - `pre_remove` — before a worktree is removed. A non-zero exit **aborts** the
    removal and the command exits `1`, unless `--force`, in which case the failure is
    reported as a warning, removal proceeds, and the command exits `0` on success.

  `--no-hooks` skips hooks for a single command (supported by `new`, `pr`, and
  `remove`).

**Atomicity (§13):** if `post_create` fails, the worktree is kept (the failure is only
a warning). If worktree *creation itself* fails partway through `new`/`pr` (before the
hook), `wt` rolls back the partial state via `git worktree remove`/`prune` and reports
the failure, leaving no half-created worktree.

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
- **Mechanism:** the generated completion scripts call a hidden helper,
  `wt __complete <kind> [partial]` (e.g. `worktrees`, `branches`, `pr-numbers`), which
  prints candidates one per line to stdout. The helper exits `0` with **no output**
  when invoked outside a repository, or — for `pr-numbers` — when `gh` is missing or
  unauthenticated, so the shell never sees an error or a hang. PR-number completion is
  best-effort and must not block the prompt.
- `wt shell-init` installs completion automatically so users get it from one line
  in their rc file; `wt completions` remains available for manual setup.

---

## 10. TUI

Launched by `wt` (no args) or `wt ui`. It is a live dashboard and action center,
and a first-class citizen of `wt` on equal footing with the CLI command surface.

### Layout

The TUI has three regions:

1. **Worktree list (left pane)** — each row shows, in order: status marker, dirty
   marker, branch name, ahead/behind, last-commit summary + relative time, and PR
   number/state if known. The dirty marker uses the same `M`/`?` glyphs as `wt list`
   (subject to `list.show_untracked`), not a single generic indicator. The active row
   is highlighted; missing worktrees appear dimmed with a `!` prefix. The list is
   ordered by the same default sort as `wt list` (`branch`); see the `sort-cycle` /
   `sort-reverse` key bindings below to change it at runtime.
2. **Detail pane (right pane)** — shown for the selected worktree, in order:
   - Path (absolute)
   - Branch → upstream (e.g. `feature/x → origin/feature/x`, or
     `feature/x (no upstream)`)
   - Base ref (recorded at creation by `wt`; blank if worktree was not created by `wt`)
   - Status: ahead/behind counts, dirty indicator
   - Last 5 commits (short hash, subject, relative time)
   - PR: number, title, state, URL (if recorded)
3. **Status/help line (bottom bar)**:
   - Left: current mode name and active filter string (if any)
   - Right: key hints for the most common actions in the current mode
   - Shows the full key-binding table when `?` is pressed

The list and detail panes are resizable. `\` toggles the list pane to give the
detail pane full width. `+`/`-` grow/shrink the list pane. When the terminal is
narrower than 60 columns, the detail pane is hidden automatically.

### Async loading

Loading is split so the TUI is always immediately interactive:

- **Synchronous (before first paint):** worktree enumeration, branch names, paths,
  current-worktree detection.
- **Asynchronous (populated after paint):** dirty/untracked status, ahead/behind
  counts, PR state. Each row shows a per-field spinner `…` until its async data
  arrives; there is no full-screen loading state.

Until a row's async data arrives, each pending field shows a steady `…` placeholder
(animation is an implementation choice, not a requirement). A failed async fetch (e.g.
upstream not configured, `gh` unavailable) fills the affected field with `–` and does
not surface as an error. `r` forces a full refresh of all async data.

### View modes

The TUI operates in distinct modal states (all transitions are keyboard-driven
within a single screen; there are no separate pages):

| Mode | Trigger | Description |
|------|---------|-------------|
| **List** | default / `Esc` | Main worktree list + detail pane |
| **Filter** | `/` | Fuzzy filter overlay on branch/slug/path; `Esc` clears |
| **Create** | `n` | Prompts for branch name and optional base ref, then creates |
| **PR picker** | `p` | Fetches open PRs via `gh`; `Enter` checks out the selected PR |
| **Confirm remove** | `d` | Shows worktree info and safety status; `y` confirms |
| **Help** | `?` | Full key-binding reference overlay; any key dismisses |

### Key bindings

All bindings are configurable via `ui.keybindings` (§11). Defaults:

| Key(s) | Action | Notes |
|--------|--------|-------|
| `↑` / `k` | navigate-up | |
| `↓` / `j` | navigate-down | |
| `PgUp` / `ctrl+u` | page-up | |
| `PgDn` / `ctrl+d` | page-down | |
| `g` / `Home` | go-to-top | |
| `G` / `End` | go-to-bottom | |
| `Tab` | focus-next-pane | cycles list → detail pane |
| `Shift+Tab` | focus-prev-pane | cycles detail → list pane |
| `Enter` | switch | select worktree → print path to stdout → shell `cd`s → TUI exits |
| `/` | filter | enter Filter mode |
| `Esc` | clear-filter | clears active filter; dismisses overlay modes (back) |
| `n` | new | open Create mode |
| `d` | remove | open Confirm-remove mode |
| `p` | pr-checkout | open PR picker mode |
| `o` | open-editor | open selected worktree in the resolved editor (suspends the TUI) |
| `r` | refresh | force full async refresh |
| `s` | sort-cycle | cycle the sort field (same fields as `wt list --sort`) |
| `S` | sort-reverse | toggle ascending/descending |
| `?` | help | show Help overlay |
| `q` | quit | exit TUI without switching worktree |
| `\` | toggle-sidebar | hide/show list pane (full-screen detail) |
| `+` | resize-sidebar-grow | grow list pane width by one column |
| `-` | resize-sidebar-shrink | shrink list pane width by one column |

`open-editor` launches the editor in the **foreground**: `wt` leaves raw mode and the
alternate screen, runs the editor (resolved as `editor` config → `$VISUAL` → `$EDITOR`,
erroring if all are unset), and restores the TUI when the editor exits.

### Modal behaviors

**Create mode** prompts in sequence:
1. Branch name (required; validated as a legal git ref name before submission)
2. Base ref (optional; tab-completes local branches; defaults to `default_base`
   config value)
3. `Enter` submits; `Esc` cancels at any prompt. Errors from git (e.g. branch
   already checked out) are shown inline without leaving the TUI.

**PR picker** columns: PR number, title (truncated), author, state, age. Data is
fetched via `gh pr list --json` on open, with a spinner while fetching. If `gh` is
unavailable or unauthenticated, the modal shows the error with a hint to run
`gh auth login`; `Esc` dismisses.

**Confirm-remove dialog** shows: branch name, path, dirty indicator, count of
unpushed commits, and for missing worktrees the note `(directory already deleted)`.
Prompt text: `Remove this worktree? [y/N]`. Dirty worktrees additionally show
`(has uncommitted changes — data may be lost)`. Missing worktrees skip the dirty
check. `y` proceeds; any other key or `Esc` cancels.

### Mouse support

Mouse is enabled by default (`ui.mouse = true`). Supported interactions:
- Click a row to select it.
- Scroll wheel to scroll the list.
- Click the detail pane to focus it (scrollable with arrow keys or scroll wheel).
- `ui.mouse = false` disables all mouse handling.

### Nerd Font support

`ui.nerd_fonts = true` (default: `false`) enables optional Nerd Font glyphs for
status markers and branch indicators instead of the default ASCII fallbacks. The
exact glyph set is an implementation choice; the requirement is that the ASCII
fallbacks remain correct and readable when Nerd Fonts are disabled.

### Terminal resilience

- The TUI must restore the terminal on exit regardless of cause (normal quit,
  `q`, signal, panic). Raw mode and alternate screen must be cleaned up.
- On `SIGWINCH`, the TUI redraws at the new terminal size. If the terminal shrinks
  below 60 columns wide, the detail pane is hidden; below 5 rows tall, the TUI
  restores the terminal and exits cleanly, printing `terminal too small (need ≥5 rows)`
  to stderr (exit `1`).

---

## 11. Configuration

Two layers, merged with **flags > per-repo > global > built-in defaults**:
- **Global:** `$XDG_CONFIG_HOME/wt/config.toml` (and the platform-appropriate
  equivalent on macOS/Windows).
- **Per-repo:** a `.wt.toml` file at the repo root, committed or not at the user's
  discretion. Per-repo settings override global. `wt init` writes this file, and
  `wt config edit` (without `--global`) opens it.
- Per-worktree metadata that `wt` itself records (e.g. originating PR number,
  "created by wt") lives in Git config under the `wt.*` namespace, not in these
  files — keeping it tied to Git's own state.

**Merge semantics across layers:**
- **Scalars** (strings, bools) follow the precedence above: a value set at a higher
  layer fully replaces lower ones.
- **`ui.keybindings`** deep-merges **per action key**: a per-repo (or global) table
  overrides only the individual bindings it names, leaving the rest at their defaults.
- **Array values** (`copy`, `list.columns`) **replace wholesale** when set at a higher
  layer — a per-repo `copy` list fully supersedes the global one rather than
  concatenating. To extend rather than replace, the user re-lists the inherited
  entries.

**Configurable keys:**

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `path_template` | string | `{repo_parent}/{repo}.worktrees/{repo}-{branch_slug}` | Worktree store template (§6) |
| `default_base` | string | resolved default branch | Base ref for `new` when branch is created |
| `copy` | string[] | `[]` | Glob patterns to copy into new worktrees (§8) |
| `hooks.post_create` | string | — | Shell command run after worktree creation (§8) |
| `hooks.pre_remove` | string | — | Shell command run before worktree removal (§8) |
| `editor` | string | `$VISUAL`, then `$EDITOR` | Command used by TUI `o` / CLI open. Resolution: this key → `$VISUAL` → `$EDITOR`; error if all unset |
| `remove.delete_merged_branch` | bool | `true` | Delete wt-created branch on `remove` if fully merged |
| `remove.untracked_blocks` | bool | `false` | If `true`, untracked files count as dirty for remove/prune guards |
| `pr.default_remote` | string | `origin` | Remote used for PR fetches |
| `list.show_untracked` | bool | `true` | Show `?` in dirty column for untracked files |
| `list.columns` | string[] | all | Ordered list of columns to display in `wt list`. Valid identifiers: `status`, `dirty`, `branch`, `path`, `ahead-behind`, `commit`, `pr` (default = all, in that order). An unknown identifier is a config error |
| `ui.nerd_fonts` | bool | `false` | Enable Nerd Font glyphs in TUI (§10) |
| `ui.mouse` | bool | `true` | Enable mouse support in TUI (§10) |
| `ui.color` | string | `auto` | Color output for both CLI and TUI: `auto`, `always`, or `never`. Same knob as the `--color` flag at a different layer; see precedence below |
| `ui.keybindings` | table | defaults | Action-name → key-string overrides for TUI (§10) |

**Keybinding configuration:** `ui.keybindings` is a TOML table mapping action
names to key strings. The complete set of action names (identical to the §10 key
table) is: `navigate-up`, `navigate-down`, `page-up`, `page-down`, `go-to-top`,
`go-to-bottom`, `focus-next-pane`, `focus-prev-pane`, `switch`, `filter`,
`clear-filter`, `new`, `remove`, `pr-checkout`, `open-editor`, `refresh`,
`sort-cycle`, `sort-reverse`, `help`, `quit`, `toggle-sidebar`,
`resize-sidebar-grow`, `resize-sidebar-shrink`. An unknown action name is a config
error. Key strings use standard terminal notation (`ctrl+c`, `alt+enter`, `f5`).

**Color precedence** (the `--color` flag, the `ui.color` config key, and the
`NO_COLOR` env var are reconciled in this order, first match wins):
1. an explicit `--color always|never` on the command line;
2. `NO_COLOR` set and non-empty → `never`;
3. `ui.color` (if set to `always`/`never`);
4. otherwise `auto` — color when stdout is a TTY.

`ui.color` and `--color` are the same setting at the config and flag layers; there is
no separate CLI-only color key.

**Validation timing:** configuration is parsed and validated on **every invocation**
(at load time), not lazily. Unknown keys, unknown `ui.keybindings` action names, and
malformed key strings are rejected immediately with a precise error (file, key,
reason); `wt` never silently ignores unknown keys. `wt config edit` opens the
appropriate file (`.wt.toml` per-repo, or the global config with `--global`) in the
resolved editor.

---

## 12. Safety, Errors, and Edge Cases

These behaviors are required, not optional polish:

- **Not in a repo:** any repo-scoped command exits non-zero with a clear message;
  completion degrades silently.
- **`-C <path>`:** changes the effective working directory before anything else;
  repository discovery then proceeds upward from `<path>` exactly as if `wt` had been
  launched there. It also becomes the basis for "the worktree the command ran from"
  in the copy step (§8).
- **Bare repository:** a bare repo has no current worktree. `list`/TUI show no `*`
  (current) marker. `switch`/`status` invoked with no query (which need a "current"
  worktree) report that there is no current worktree and exit `1` rather than
  crashing; with a query they work normally.
- **Branch already checked out elsewhere:** report which worktree holds it; do not
  attempt a duplicate checkout (Git forbids it).
- **Remove/prune safety guards.** Two independent guards refuse removal without
  `--force`, and on `--force` `wt` states plainly that work may be lost:
  - **Dirty:** modified tracked files or staged changes. Untracked files do *not*
    count as dirty by default; this is controlled by `remove.untracked_blocks` (§11).
  - **Unpushed work:** local commits ahead of the upstream (`ahead > 0`). A branch
    with no upstream is treated as having unpushed work.
  Both the CLI guards and the TUI confirm-remove dialog apply these same definitions.
  Missing worktrees skip both guards (there is no working tree to inspect).
- **Untracked files:** displayed with `?` in `wt list` / TUI when `list.show_untracked`
  is `true` (default), but do not block `remove`/`prune` unless `remove.untracked_blocks`
  is set.
- **Missing worktree:** when a worktree's directory has been deleted externally,
  `wt` must not error on this fact. The worktree is shown in `list`/TUI with a `!`
  marker. `remove` on a missing worktree runs `git worktree prune` to clean the
  admin record rather than `git worktree remove`; `--force` is not required.
  `prune --gone` includes missing worktrees as candidates.
- **No upstream tracking branch:** `wt list` and the TUI display `–` for the
  ahead/behind column; `wt status` notes that no upstream is configured. This is
  not an error and does not affect any remove/prune guard.
- **Path collision on create:** error unless it is the same branch's existing
  worktree (then idempotent no-op).
- **Stale admin metadata:** `prune` (and a best-effort check on `list`) reconciles
  Git's internal worktree records so manually-deleted directories don't linger.
- **`gh` missing/unauthenticated:** only PR commands fail, with a message pointing
  to `gh auth login`; everything else works. The TUI PR picker shows this error
  inline; it does not crash or disable other TUI functionality.
- **Detached HEAD / no default branch:** fall back to current `HEAD` as base and
  warn.
- **Subprocess failures:** surface the underlying `git`/`gh` stderr; do not swallow.
- **Exit codes:** `0` success; `1` user/operation error; `2` usage/argument error;
  `3` ambiguous query / nothing selected. The shell wrapper treats exit `3` (and a `0`
  exit with empty stdout, e.g. a cancelled picker) as "do not `cd`", avoiding a
  spurious navigation (§5).
- **Concurrency:** rely on Git's own locking for worktree mutations; detect lock
  contention (a non-zero `git` exit naming a `.lock` file) and surface the underlying
  stderr with the hint "another git process holds the worktree lock" rather than
  corrupting state.

---

## 13. Non-Functional Requirements

- **Single self-contained binary**, no runtime dependencies beyond `git` (always)
  and `gh` (PR commands only).
- **Platforms:** Linux and macOS are first-class; Windows is supported on a
  best-effort basis (path handling, shell snippets for PowerShell).
- **Performance targets** (local SSD; networked filesystems are out of scope):
  - `wt list` synchronous output (before async data): ≤ 200 ms for a repo with up
    to 100 worktrees.
  - TUI first paint (before async data arrives): ≤ 100 ms.
  - Shell completion response time: ≤ 50 ms.
  - These are achieved by using `gix` reads and avoiding any per-worktree subprocess
    fan-out for read-only listing.
- **Startup:** no network access on any command except explicit fetch/PR operations.
- **Output:** human output is concise and respects `NO_COLOR` and non-TTY stdout
  (auto-plain). `--json` output is stable and documented; every documented JSON object
  carries a `schema_version` integer (currently `1`) that is bumped only on a breaking
  change, so consumers can detect incompatibility.
- **Paging:** the long human listings (`wt list`, `wt status`) page through `$PAGER`
  (default `less -FRX`) only when stdout is a TTY and the output exceeds one screen.
  Paging is disabled by `--no-pager`, by `--json`, and whenever stdout is not a TTY,
  so scripts never block on a pager. No other command pages.
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
