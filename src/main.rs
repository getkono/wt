//! `wt` — thin binary entry point.
//!
//! Wires up error reporting and tracing, builds the runtime context over real
//! stdio and the process environment, then delegates to the library. This file
//! is intentionally minimal and is excluded from coverage measurement (see the
//! `coverage` recipe in the `Justfile`).

use std::io::IsTerminal;
use std::process::ExitCode;

fn main() -> ExitCode {
    if let Err(error) = color_eyre::install() {
        eprintln!("wt: failed to install error reporter: {error}");
        return ExitCode::FAILURE;
    }
    // Diagnostics go to stderr so stdout stays reserved for paths/JSON (§5).
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let out = wt::Stream::new(Box::new(std::io::stdout()), std::io::stdout().is_terminal());
    let err = wt::Stream::new(Box::new(std::io::stderr()), std::io::stderr().is_terminal());
    let cwd = std::env::current_dir().unwrap_or_default();
    let git = std::sync::Arc::new(wt::git::RealGit);
    let gh = std::sync::Arc::new(wt::gh::RealGh);
    let agent = std::sync::Arc::new(wt::agent::RealAgent);
    let input = Box::new(wt::cx::StdinInput);
    let mut cx = wt::Cx::new(out, err, wt::Env::from_real(), cwd, git, gh, agent, input);

    let args = std::env::args().skip(1).collect();
    ExitCode::from(wt::run(args, &mut cx))
}
