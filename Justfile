# List available recipes
default:
    @just --list

# Run the application
run:
    cargo run

# Format the code
format:
    cargo fmt

# Check formatting without modifying files (used by CI and pre-push)
format-check:
    cargo fmt --check

# Lint with Clippy, treating warnings as errors
lint:
    cargo clippy --all-targets -- -D warnings

# Lint and auto-fix where possible (used by the pre-commit hook)
lint-fix:
    cargo clippy --fix --allow-dirty --allow-staged --all-targets

# Run the test suite
test:
    cargo test

# Run tests with coverage (minimum 80% line coverage; the main.rs entry point is excluded)
coverage:
    cargo llvm-cov --ignore-filename-regex 'src/main\.rs' --fail-under-lines 80
