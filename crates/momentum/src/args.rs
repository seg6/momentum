//! CLI argument definitions.
//!
//! Kept deliberately small. See `commands.rs` for what each verb does.

use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

/// Sennheiser MOMENTUM 4 remote control.
///
/// Each invocation opens the Bluetooth transport, runs one operation,
/// prints the result, and exits. There is no daemon. To watch for
/// device-pushed events, use `momentum watch`.
#[derive(Debug, Parser)]
#[command(name = "momentum", version, about, long_about = None)]
pub struct Cli {
    /// Headset Bluetooth address (e.g. `AA:BB:CC:DD:EE:FF`).
    #[arg(long, env = "MOMENTUM_ADDRESS", global = true)]
    pub address: Option<String>,

    /// RFCOMM channel (defaults to 1).
    #[arg(long, env = "MOMENTUM_CHANNEL", global = true)]
    pub spp_channel: Option<u8>,

    /// Per-operation timeout (e.g. "4s", "500ms"). Default is 4s.
    #[arg(long, env = "MOMENTUM_TIMEOUT", value_parser = parse_duration, global = true)]
    pub timeout: Option<Duration>,

    /// Emit machine-readable JSON instead of human text.
    ///
    /// Ignored by `watch`, which always emits newline-delimited JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Enable debug logging (also respects `RUST_LOG`).
    #[arg(long, global = true)]
    pub debug: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print a snapshot of common runtime state.
    Status,
    /// Show battery level.
    Battery,
    /// Read or change Active Noise Cancellation.
    Anc {
        #[command(subcommand)]
        command: Option<AncCommand>,
    },
    /// Read or set the transparency level (0..=100).
    Transparency {
        #[arg(value_parser = parse_level)]
        level: Option<u8>,
    },
    /// Show EQ band gains, or set one with `eq set <band> <gain>`.
    Eq {
        #[command(subcommand)]
        command: Option<EqCommand>,
    },
    /// Show device model and firmware release.
    Info,
    /// Subscribe to device notifications and print events until killed.
    Watch,
}

#[derive(Debug, Subcommand)]
pub enum AncCommand {
    /// Turn the master ANC switch on.
    On,
    /// Turn the master ANC switch off.
    Off,
    /// Wind-noise sub-mode (off/on/auto).
    Wind { state: WindArg },
    /// Adaptive sub-mode (on/off).
    Adaptive { state: Switch },
}

#[derive(Debug, Subcommand)]
pub enum EqCommand {
    /// Print all band gains (default).
    Show,
    /// Set the gain of a single band (`band` is 1-indexed).
    Set { band: u8, gain: i8 },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum Switch {
    On,
    Off,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "lowercase")]
pub enum WindArg {
    Off,
    On,
    Auto,
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    humantime::parse_duration(value).map_err(|e| format!("invalid duration: {e}"))
}

fn parse_level(value: &str) -> Result<u8, String> {
    match value.parse::<u8>() {
        Ok(n) if n <= 100 => Ok(n),
        Ok(n) => Err(format!("level must be 0..=100, got {n}")),
        Err(_) => Err(format!("expected a number 0..=100, got `{value}`")),
    }
}
