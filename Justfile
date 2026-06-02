# List available recipes
default:
    @just --list

# Run the application
run:
    cargo run

# Build and install the `wt` binary to ~/.cargo/bin
install:
    cargo install --path .
    @echo ''
    @echo '✓ Installed wt to ~/.cargo/bin'
    @echo 'Next: enable shell integration for navigation + dynamic tab completion.'
    @echo 'Add to your shell rc (see README → "Enable shell integration"):'
    @echo '  eval "$(wt shell-init bash)"    # bash'
    @echo '  eval "$(wt shell-init zsh)"     # zsh'
    @echo '  wt shell-init fish | source     # fish'

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
