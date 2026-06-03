//! Best-effort headset address autodetection.
//!
//! Shells out to `bluetoothctl devices Connected` and looks for a single
//! device whose name contains `MOMENTUM`. No new dependencies; relies on
//! the BlueZ CLI that ships with every working stack.

use std::process::Command;

use anyhow::{Result, anyhow, bail};

const NAME_NEEDLE: &str = "MOMENTUM";

pub fn autodetect() -> Result<String> {
    let output = Command::new("bluetoothctl")
        .args(["devices", "Connected"])
        .output()
        .map_err(|e| {
            anyhow!(
                "could not run `bluetoothctl devices Connected` ({e}); \
                 pass --address or set MOMENTUM_ADDRESS"
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "`bluetoothctl devices Connected` exited {}: {}",
            output.status,
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let matches: Vec<(String, String)> = stdout
        .lines()
        .filter_map(parse_device_line)
        .filter(|(_, name)| name.to_ascii_uppercase().contains(NAME_NEEDLE))
        .collect();

    match matches.as_slice() {
        [] => Err(anyhow!(
            "no connected MOMENTUM device found via bluetoothctl; \
             connect the headset or pass --address / MOMENTUM_ADDRESS"
        )),
        [(addr, _)] => Ok(addr.clone()),
        many => {
            let listing = many
                .iter()
                .map(|(a, n)| format!("  {a}  {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            Err(anyhow!(
                "multiple connected MOMENTUM devices; pass --address explicitly:\n{listing}"
            ))
        }
    }
}

fn parse_device_line(line: &str) -> Option<(String, String)> {
    let rest = line.trim().strip_prefix("Device ")?;
    let (addr, name) = rest.split_once(' ')?;
    if !is_bdaddr(addr) {
        return None;
    }
    Some((addr.to_string(), name.trim().to_string()))
}

fn is_bdaddr(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() != 17 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        let is_sep = i % 3 == 2;
        if is_sep {
            if *b != b':' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_line() {
        let parsed = parse_device_line("Device 80:C3:BA:2A:6A:1D MOMENTUM 4");
        assert_eq!(
            parsed,
            Some(("80:C3:BA:2A:6A:1D".into(), "MOMENTUM 4".into()))
        );
    }

    #[test]
    fn rejects_non_device_lines() {
        assert!(parse_device_line("Controller AA:BB:CC:DD:EE:FF foo").is_none());
        assert!(parse_device_line("").is_none());
        assert!(parse_device_line("Device not-a-mac whatever").is_none());
    }

    #[test]
    fn bdaddr_validation() {
        assert!(is_bdaddr("80:C3:BA:2A:6A:1D"));
        assert!(is_bdaddr("aa:bb:cc:dd:ee:ff"));
        assert!(!is_bdaddr("80-C3-BA-2A-6A-1D"));
        assert!(!is_bdaddr("80:C3:BA:2A:6A"));
        assert!(!is_bdaddr("zz:C3:BA:2A:6A:1D"));
    }
}
