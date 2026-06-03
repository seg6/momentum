mod args;
mod commands;
mod discover;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use crate::args::Cli;

fn main() -> Result<()> {
    // Restore default SIGPIPE handling so piping into `head`/`less`
    // exits cleanly instead of erroring when stdout closes.
    // SAFETY: replacing a signal handler with the default disposition
    // is sound.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    init_logging(cli.debug);
    commands::run(cli)
}

fn init_logging(debug: bool) {
    let default = if debug {
        "momentum=debug,momentum_transport=debug"
    } else {
        "warn"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
