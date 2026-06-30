pub mod args;
pub mod commands;
pub mod discover;
#[cfg(target_os = "macos")]
mod macos;
pub mod tui;

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
    if matches!(cli.command, args::Command::Tui) {
        anyhow::bail!("tui requires an interactive terminal; call momentum::run");
    }
    commands::run(cli, writer)
}

/// Execute a CLI command, writing output to stdout.
pub fn run(cli: args::Cli) -> Result<()> {
    if matches!(cli.command, args::Command::Tui) {
        return tui::run(cli);
    }
    run_with_output(cli, &mut io::stdout().lock())
}

/// Entry point used by the binary. On macOS the command logic runs on a worker
/// thread while the main thread pumps a `CFRunLoop` for IOBluetooth callbacks;
/// elsewhere it just runs inline.
pub fn run_main(cli: args::Cli) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::runloop::drive(move || run(cli))
    }
    #[cfg(not(target_os = "macos"))]
    {
        run(cli)
    }
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

/// On macOS, re-spawn ourselves as our own TCC-responsible process so Bluetooth
/// access prompts (once) instead of crashing. No-op on other platforms. Must be
/// called before any Bluetooth work and before argument parsing.
pub fn ensure_macos_responsibility() -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::responsibility::ensure_own_responsibility()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(())
    }
}
