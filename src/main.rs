//! `wt` — a command-line and terminal UI application.
//!
//! This entry point is intentionally thin: it wires up error reporting and
//! tracing, then delegates to the `wt` library crate. It is excluded from
//! coverage measurement (see the `coverage` recipe in the `Justfile`).

use color_eyre::eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tracing_subscriber::fmt::init();

    println!("{}", wt::greeting("world"));

    Ok(())
}
