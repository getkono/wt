# wt

A command-line and terminal UI (TUI) application, written in Rust (edition 2024).

## Layout

- `src/lib.rs` — the `wt` library crate. **All real logic lives here** so it is
  unit-testable and counted by coverage.
- `src/main.rs` — thin binary entry point. Wires up `color_eyre` and
  `tracing_subscriber`, then delegates to the library. **Excluded from coverage**
  (the `coverage` recipe passes `--ignore-filename-regex 'src/main\.rs'`).

When adding features, put logic in `lib.rs` (or new library modules) with tests,
and keep `main.rs` minimal. Do not move testable logic into `main.rs` — it will
not be covered and will not count toward the 80% threshold.

## Packages

- **tokio** (full) — async runtime for the application's event loop and async I/O.
- **eyre + color-eyre** — application-level error reporting with readable,
  colorful backtraces; `color_eyre::install()` runs at startup in `main`.
- **tracing + tracing-subscriber** — structured, async-aware diagnostics; the
  subscriber is configured in `main`. Use `tracing` macros (not `println!`) for
  diagnostics in library code. Verbosity is controlled by `RUST_LOG` (default
  `warn`); e.g. `RUST_LOG=wt=debug wt prune --merged --dry-run` traces selection.
- **thiserror** — derive typed error enums for the library crate's public APIs.

## Quality

Validate changes:

```bash
mise run test            # correctness
mise run format-check    # formatting
mise run lint            # Clippy (warnings as errors)
mise run coverage        # coverage (minimum 80% line coverage)
```

Code quality rules for this project:

- All `pub` items in `lib.rs` need doc comments.
- Return typed errors with `thiserror` from library APIs; reserve `eyre`/`color-eyre`
  for the binary's `main` and application-level glue.
- No `unwrap()`/`expect()` in non-test code — propagate errors with `?`.
- Keep `main.rs` a thin entry point; all testable logic belongs in the library.
- Clippy runs with `-D warnings`, so the tree must be warning-clean.

## License

MIT — see [LICENSE](LICENSE) for details.
