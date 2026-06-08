use std::io::Write;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use momentum_core::{
    Headset, Notification, Transport,
    state::{AncSubMode, Toggle, WindState},
};
use momentum_transport::open_default;
use serde::Serialize;

use crate::args::{AncCommand, Cli, Command, EqCommand, Switch, WindArg};
use crate::discover;

const DEFAULT_CHANNEL: u8 = 1;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(4);

pub fn run(cli: Cli, w: &mut impl Write) -> Result<()> {
    let json = cli.json;
    let mut headset = open_headset(&cli)?;

    match cli.command {
        Command::Status => status(&mut headset, json, w),
        Command::Battery => battery(&mut headset, json, w),
        Command::Anc { command } => anc(&mut headset, command, json, w),
        Command::Transparency { level } => transparency(&mut headset, level, json, w),
        Command::Eq { command } => eq(&mut headset, command.unwrap_or(EqCommand::Show), json, w),
        Command::Info => info(&mut headset, json, w),
        Command::Watch => watch(&mut headset, w),
    }
}

fn open_headset(cli: &Cli) -> Result<Headset<Box<dyn Transport>>> {
    let address = match cli.address.clone() {
        Some(addr) => addr,
        None => discover::autodetect()?,
    };
    let channel = cli.spp_channel.unwrap_or(DEFAULT_CHANNEL);
    let timeout = cli.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let transport = open_default(&address, channel, timeout)
        .with_context(|| format!("connecting to {address}"))?;
    Ok(Headset::new(transport))
}

fn status<T: Transport>(h: &mut Headset<T>, json: bool, w: &mut impl Write) -> Result<()> {
    let state = h.show_state()?;
    if json {
        return print_json(&state, w);
    }
    writeln!(w, "MOMENTUM 4")?;
    print_row("battery", format!("{}%", state.battery), w)?;
    print_row("charging", state.charging, w)?;
    print_row("anc", state.noise, w)?;
    print_row("anc modes", state.anc_modes, w)?;
    print_row("transparency", state.transparency, w)?;
    Ok(())
}

fn battery<T: Transport>(h: &mut Headset<T>, json: bool, w: &mut impl Write) -> Result<()> {
    let percent = h.battery()?;
    if json {
        return print_json(&serde_json::json!({ "level": percent }), w);
    }
    writeln!(w, "{percent}%")?;
    Ok(())
}

fn anc<T: Transport>(
    h: &mut Headset<T>,
    command: Option<AncCommand>,
    json: bool,
    w: &mut impl Write,
) -> Result<()> {
    match command {
        None => {
            let master = h.anc()?;
            let modes = h.anc_modes()?;
            if json {
                return print_json(
                    &serde_json::json!({
                        "on": matches!(master, Toggle::On),
                        "wind": modes.wind,
                        "adaptive": modes.adaptive,
                    }),
                    w,
                );
            }
            writeln!(w, "ANC")?;
            print_row("master", master, w)?;
            print_row("wind", modes.wind, w)?;
            print_row("adaptive", on_off(modes.adaptive), w)?;
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

fn transparency<T: Transport>(
    h: &mut Headset<T>,
    level: Option<u8>,
    json: bool,
    w: &mut impl Write,
) -> Result<()> {
    match level {
        Some(level) => h.set_transparency(level).map_err(Into::into),
        None => {
            let level = h.transparency()?;
            if json {
                print_json(&serde_json::json!({ "level": level }), w)
            } else {
                writeln!(w, "{level}")?;
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

fn eq<T: Transport>(
    h: &mut Headset<T>,
    command: EqCommand,
    json: bool,
    w: &mut impl Write,
) -> Result<()> {
    match command {
        EqCommand::Show => {
            let config = h.eq_config()?;
            let bands: Vec<i8> = (0..config.bands)
                .map(|b| h.eq_gain(b))
                .collect::<momentum_core::Result<_>>()?;
            if json {
                return print_json(
                    &serde_json::json!({
                        "min": config.min_gain,
                        "max": config.max_gain,
                        "bands": bands,
                    }),
                    w,
                );
            }
            writeln!(w, "EQ")?;
            print_row("range", format!("{}..{}", config.min_gain, config.max_gain), w)?;
            for (i, gain) in bands.iter().enumerate() {
                print_row(&format!("band {}", i + 1), gain, w)?;
            }
            Ok(())
        }
        EqCommand::Set { band, gain } => {
            let config = h.eq_config()?;
            if band == 0 || band > config.bands {
                return Err(anyhow!("band must be in 1..={}, got {band}", config.bands));
            }
            if gain < config.min_gain || gain > config.max_gain {
                return Err(anyhow!(
                    "gain must be in {}..={}, got {gain}",
                    config.min_gain,
                    config.max_gain
                ));
            }
            let band0 = band - 1;
            h.set_eq_gain(band0, gain)?;
            Ok(())
        }
    }
}

fn info<T: Transport>(h: &mut Headset<T>, json: bool, w: &mut impl Write) -> Result<()> {
    let info = h.device_info()?;
    if json {
        return print_json(&info, w);
    }
    writeln!(w, "Device Info")?;
    print_row("model", &info.model, w)?;
    print_row("release", &info.release, w)?;
    print_row("hw revision", &info.hw_revision, w)?;
    print_row("gaia build", &info.gaia_build, w)?;
    Ok(())
}

fn watch<T: Transport>(h: &mut Headset<T>, w: &mut impl Write) -> Result<()> {
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
                writeln!(w, "{line}")?;
                w.flush()?;
            }
            Err(_) => {
                return Err(anyhow!("notification stream closed"));
            }
        }
    }
}

fn print_row<V: core::fmt::Display>(label: &str, value: V, w: &mut impl Write) -> Result<()> {
    writeln!(w, "  {label:<22} {value}")?;
    Ok(())
}

fn print_json<T: Serialize>(value: &T, w: &mut impl Write) -> Result<()> {
    writeln!(w, "{}", serde_json::to_string(value)?)?;
    Ok(())
}
