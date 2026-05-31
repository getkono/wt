# wt

A command-line and terminal UI (TUI) application.

## Prerequisites

- [Rust (rustup)](https://rustup.rs) — toolchain (pinned via `rust-toolchain.toml`)
- [just](https://github.com/casey/just) — command runner
- [Lefthook](https://github.com/evilmartians/lefthook) — git hooks manager
- [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) — code coverage tool

## Quick Start

```bash
cargo run
```

## Development

| Command             | Description                          |
| ------------------- | ------------------------------------ |
| `cargo run`         | Run the application                  |
| `just test`         | Run tests                            |
| `just format`       | Format code                          |
| `just lint`         | Lint with Clippy (warnings as errors)|
| `just lint-fix`     | Lint and auto-fix                    |
| `just coverage`     | Run tests with coverage (min 80%)    |

After cloning, run `lefthook install` once to activate the git hooks.

## Tech Stack

- **Runtime:** Rust (edition 2024)
- **Formatter:** rustfmt
- **Linter:** Clippy
- **Task runner:** just
- **Key Dependencies:** tokio, eyre + color-eyre, tracing + tracing-subscriber, thiserror

## Architecture

The logic lives in the library crate (`src/lib.rs`) so it is unit-testable and
measured by coverage. The binary (`src/main.rs`) is a thin entry point that
wires up error reporting and tracing, then delegates to the library; it is
excluded from coverage.

## Git Hooks

This project uses [Lefthook](https://github.com/evilmartians/lefthook).
Pre-commit hooks auto-fix formatting and linting on staged Rust files.
Pre-push hooks run format checks, Clippy, tests, and the coverage gate.

## CI/CD

GitHub Actions runs format checks, linting, tests, and coverage on pushes to
`main` and pull requests.

## Code Coverage

This project uses [`cargo-llvm-cov`](https://github.com/taiki-e/cargo-llvm-cov)
for LLVM-based code coverage. CI enforces a minimum of 80% line coverage and
uploads the report as a CI artifact.

```bash
just coverage
```

## License

Proprietary — all rights reserved. See [LICENSE](LICENSE) for details.
