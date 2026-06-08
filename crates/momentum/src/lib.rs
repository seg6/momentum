pub mod args;
pub mod commands;
pub mod discover;

// Re-export the most common types at crate root so consumers can write
// `momentum::Cli::parse()` instead of `momentum::args::Cli::parse()`.
pub use args::{AncCommand, Cli, Command, EqCommand, Switch, WindArg};

use anyhow::Result;
use std::io::{self, Write};

/// Execute a CLI command, writing output to the given writer.
///
/// Library consumers (e.g. the Tauri UI) can capture output by passing
/// a `Vec<u8>` or similar, avoiding a round-trip through a subprocess.
pub fn run_with_output(cli: args::Cli, writer: &mut impl Write) -> Result<()> {
    commands::run(cli, writer)
}

/// Execute a CLI command, writing output to stdout.
pub fn run(cli: args::Cli) -> Result<()> {
    run_with_output(cli, &mut io::stdout().lock())
}

pub fn init_logging(debug: bool) {
    let default = if debug {
        "momentum=debug,momentum_transport=debug"
    } else {
        "warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
