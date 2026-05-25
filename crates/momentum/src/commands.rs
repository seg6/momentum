//! CLI subcommand handlers.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use momentum_core::{
    Headset, Notification, Transport,
    state::{AncSubMode, Toggle, WindState},
};
use momentum_transport::open_default;
use serde::Serialize;

use crate::args::{AncCommand, Cli, Command, EqCommand, Switch, WindArg};

const DEFAULT_CHANNEL: u8 = 1;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(4);

pub fn run(cli: Cli) -> Result<()> {
    let json = cli.json;
    let mut headset = open_headset(&cli)?;

    match cli.command {
        Command::Status => status(&mut headset, json),
        Command::Battery => battery(&mut headset, json),
        Command::Anc { command } => anc(&mut headset, command, json),
        Command::Transparency { level } => transparency(&mut headset, level, json),
        Command::Eq { command } => eq(&mut headset, command.unwrap_or(EqCommand::Show), json),
        Command::Info => info(&mut headset, json),
        Command::Watch => watch(&mut headset),
    }
}

fn open_headset(cli: &Cli) -> Result<Headset<Box<dyn Transport>>> {
    let address = cli
        .address
        .clone()
        .context("no headset address: pass --address or set MOMENTUM_ADDRESS")?;
    let channel = cli.spp_channel.unwrap_or(DEFAULT_CHANNEL);
    let timeout = cli.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let transport = open_default(&address, channel, timeout)
        .with_context(|| format!("connecting to {address}"))?;
    Ok(Headset::new(transport))
}

fn status<T: Transport>(h: &mut Headset<T>, json: bool) -> Result<()> {
    let state = h.show_state()?;
    if json {
        return print_json(&state);
    }
    println!("MOMENTUM 4");
    print_row("battery", format!("{}%", state.battery));
    print_row("charging", state.charging);
    print_row("anc", state.noise);
    print_row("anc modes", state.anc_modes);
    print_row("transparency", state.transparency);
    Ok(())
}

fn battery<T: Transport>(h: &mut Headset<T>, json: bool) -> Result<()> {
    let percent = h.battery()?;
    if json {
        return print_json(&serde_json::json!({ "level": percent }));
    }
    println!("{percent}%");
    Ok(())
}

fn anc<T: Transport>(h: &mut Headset<T>, command: Option<AncCommand>, json: bool) -> Result<()> {
    match command {
        None => {
            let master = h.anc()?;
            let modes = h.anc_modes()?;
            if json {
                return print_json(&serde_json::json!({
                    "on": matches!(master, Toggle::On),
                    "wind": modes.wind,
                    "adaptive": modes.adaptive,
                }));
            }
            println!("ANC");
            print_row("master", master);
            print_row("wind", modes.wind);
            print_row("adaptive", on_off(modes.adaptive));
            Ok(())
        }
        Some(AncCommand::On) => h.set_anc(Toggle::On).map_err(Into::into),
        Some(AncCommand::Off) => h.set_anc(Toggle::Off).map_err(Into::into),
        Some(AncCommand::Wind { state }) => {
            let value = match state {
                WindArg::Off => WindState::Off,
                WindArg::On => WindState::On,
                WindArg::Auto => WindState::Auto,
            }
            .as_u8();
            h.set_anc_mode(AncSubMode::Wind, value).map_err(Into::into)
        }
        Some(AncCommand::Adaptive { state }) => h
            .set_anc_mode(AncSubMode::Adaptive, switch_u8(state))
            .map_err(Into::into),
    }
}

fn transparency<T: Transport>(h: &mut Headset<T>, level: Option<u8>, json: bool) -> Result<()> {
    match level {
        Some(level) => h.set_transparency(level).map_err(Into::into),
        None => {
            let level = h.transparency()?;
            if json {
                print_json(&serde_json::json!({ "level": level }))
            } else {
                println!("{level}");
                Ok(())
            }
        }
    }
}

fn switch_u8(s: Switch) -> u8 {
    match s {
        Switch::On => 1,
        Switch::Off => 0,
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn eq<T: Transport>(h: &mut Headset<T>, command: EqCommand, json: bool) -> Result<()> {
    match command {
        EqCommand::Show => {
            let config = h.eq_config()?;
            let bands: Vec<i8> = (0..config.bands)
                .map(|b| h.eq_gain(b))
                .collect::<momentum_core::Result<_>>()?;
            if json {
                return print_json(&serde_json::json!({
                    "min": config.min_gain,
                    "max": config.max_gain,
                    "bands": bands,
                }));
            }
            println!("EQ");
            print_row("range", format!("{}..{}", config.min_gain, config.max_gain));
            for (i, gain) in bands.iter().enumerate() {
                print_row(&format!("band {}", i + 1), gain);
            }
            Ok(())
        }
        EqCommand::Set { band, gain } => {
            // Public API uses 1-indexed bands; the wire is 0-indexed.
            let band0 = band
                .checked_sub(1)
                .ok_or_else(|| anyhow!("band must be >= 1"))?;
            h.set_eq_gain(band0, gain)?;
            Ok(())
        }
    }
}

fn info<T: Transport>(h: &mut Headset<T>, json: bool) -> Result<()> {
    let info = h.device_info()?;
    if json {
        return print_json(&info);
    }
    println!("Device Info");
    print_row("model", &info.model);
    print_row("release", &info.release);
    print_row("hw revision", &info.hw_revision);
    print_row("gaia build", &info.gaia_build);
    Ok(())
}

fn watch<T: Transport>(h: &mut Headset<T>) -> Result<()> {
    let rx = h
        .notifications()
        .ok_or_else(|| anyhow!("transport does not surface notifications"))?;

    for (vendor, feature, error) in h.subscribe_all() {
        tracing::warn!(
            vendor = format_args!("{vendor:#06x}"),
            feature,
            %error,
            "notification feature registration failed"
        );
    }

    loop {
        match rx.recv() {
            Ok(frame) => {
                let notification = Notification::decode(&frame);
                let line = serde_json::to_string(&notification)
                    .context("encoding notification as JSON")?;
                println!("{line}");
            }
            Err(_) => {
                return Err(anyhow!("notification stream closed"));
            }
        }
    }
}

fn print_row<V: core::fmt::Display>(label: &str, value: V) {
    println!("  {label:<22} {value}");
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}
