//! Best-effort headset address autodetection.

#[cfg(target_os = "windows")]
use std::mem::size_of;
#[cfg(target_os = "linux")]
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Command as ProcessCommand;

#[cfg(target_os = "linux")]
use anyhow::bail;
use anyhow::{Result, anyhow};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Devices::Bluetooth::{
    BLUETOOTH_DEVICE_INFO, BLUETOOTH_DEVICE_SEARCH_PARAMS, BluetoothFindDeviceClose,
    BluetoothFindFirstDevice, BluetoothFindNextDevice,
};

const NAME_NEEDLE: &str = "MOMENTUM";

#[cfg(target_os = "linux")]
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

    choose_match(&matches, "bluetoothctl")
}

#[cfg(target_os = "windows")]
pub fn autodetect() -> Result<String> {
    let search = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: 0,
        fReturnRemembered: 0,
        fReturnUnknown: 0,
        fReturnConnected: 1,
        fIssueInquiry: 0,
        cTimeoutMultiplier: 0,
        hRadio: std::ptr::null_mut(),
    };
    let mut info = bluetooth_device_info();

    // SAFETY: `search` and `info` point to initialized structures with correct dwSize fields.
    let find = unsafe { BluetoothFindFirstDevice(&search, &mut info) };
    if find.is_null() {
        return choose_match(&[], "Windows Bluetooth API");
    }

    let mut matches = Vec::new();
    loop {
        let name = device_name(&info);
        if name.to_ascii_uppercase().contains(NAME_NEEDLE) {
            // SAFETY: reading the active `ullLong` view of the documented address union.
            let addr = format_bth_addr(unsafe { info.Address.Anonymous.ullLong });
            matches.push((addr, name));
        }

        let mut next = bluetooth_device_info();
        // SAFETY: `find` is a live enumeration handle; `next` has its dwSize initialized.
        if unsafe { BluetoothFindNextDevice(find, &mut next) } == 0 {
            break;
        }
        info = next;
    }

    // SAFETY: `find` came from `BluetoothFindFirstDevice` and is no longer used.
    unsafe {
        BluetoothFindDeviceClose(find);
    }

    choose_match(&matches, "Windows Bluetooth API")
}

#[cfg(target_os = "macos")]
pub fn autodetect() -> Result<String> {
    let output = ProcessCommand::new("system_profiler")
        .args(["SPBluetoothDataType", "-json"])
        .output()
        .map_err(|e| {
            anyhow!(
                "could not run `system_profiler SPBluetoothDataType -json` ({e}); \
                 pass --address or set MOMENTUM_ADDRESS"
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "`system_profiler SPBluetoothDataType -json` exited {}: {}",
            output.status,
            stderr.trim()
        );
    }

    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let devices = macos_connected_devices(&report);
    let named: Vec<(String, String)> = devices
        .iter()
        .filter(|(_, name, _)| name.to_ascii_uppercase().contains(NAME_NEEDLE))
        .map(|(addr, name, _)| (addr.clone(), name.clone()))
        .collect();
    if !named.is_empty() {
        return choose_match(&named, "system_profiler");
    }

    // `system_profiler -json` may report paired headphones under generic keys
    // like `headset`, even when the Bluetooth Settings UI shows the real name.
    // If there is exactly one connected headset, use it as the macOS fallback.
    let headsets: Vec<(String, String)> = devices
        .into_iter()
        .filter(|(_, _, minor)| minor.as_deref() == Some("Headset"))
        .map(|(addr, name, _)| (addr, name))
        .collect();
    choose_match(&headsets, "system_profiler connected headsets")
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn autodetect() -> Result<String> {
    Err(anyhow!(
        "automatic discovery is not implemented on {}; pass --address or set MOMENTUM_ADDRESS",
        std::env::consts::OS
    ))
}

fn choose_match(matches: &[(String, String)], source: &str) -> Result<String> {
    match matches {
        [] => Err(anyhow!(
            "no connected MOMENTUM device found via {source}; \
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

#[cfg(target_os = "windows")]
fn bluetooth_device_info() -> BLUETOOTH_DEVICE_INFO {
    let mut info = BLUETOOTH_DEVICE_INFO::default();
    info.dwSize = size_of::<BLUETOOTH_DEVICE_INFO>() as u32;
    info
}

#[cfg(target_os = "windows")]
fn device_name(info: &BLUETOOTH_DEVICE_INFO) -> String {
    let len = info
        .szName
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(info.szName.len());
    String::from_utf16_lossy(&info.szName[..len])
}

#[cfg(target_os = "windows")]
fn format_bth_addr(addr: u64) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        (addr >> 40) & 0xff,
        (addr >> 32) & 0xff,
        (addr >> 24) & 0xff,
        (addr >> 16) & 0xff,
        (addr >> 8) & 0xff,
        addr & 0xff,
    )
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_device_line(line: &str) -> Option<(String, String)> {
    // bluetoothctl 5.87 emits SGR color codes even when stdout is captured,
    // so normalize its output before looking for the documented line prefix.
    let line = strip_ansi_csi(line);
    let rest = line.trim().strip_prefix("Device ")?;
    let (addr, name) = rest.split_once(' ')?;
    if !is_bdaddr(addr) {
        return None;
    }
    Some((addr.to_string(), name.trim().to_string()))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn strip_ansi_csi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'[') {
            let sequence = &bytes[index + 2..];
            if let Some(end) = sequence
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
            {
                index += end + 3;
                continue;
            }
        }

        output.push(bytes[index]);
        index += 1;
    }

    // Removing ASCII escape sequences from valid UTF-8 preserves validity.
    String::from_utf8(output).expect("stripped bluetoothctl output remains UTF-8")
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
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

#[cfg(target_os = "macos")]
fn macos_connected_devices(report: &serde_json::Value) -> Vec<(String, String, Option<String>)> {
    report
        .get("SPBluetoothDataType")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|controller| controller.get("device_connected"))
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(|entry| entry.as_object())
        .flat_map(|entry| entry.iter())
        .filter_map(|(name, details)| {
            let addr = details.get("device_address")?.as_str()?;
            if !is_bdaddr(addr) {
                return None;
            }
            let minor = details
                .get("device_minorType")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            Some((addr.to_string(), name.to_string(), minor))
        })
        .collect()
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
    fn parses_colorized_bluetoothctl_line() {
        let parsed = parse_device_line("\x1b[1;30mDevice 80:C3:BA:2A:6A:1D MOMENTUM 4\x1b[0m");
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

    #[cfg(target_os = "windows")]
    #[test]
    fn formats_windows_bluetooth_address() {
        assert_eq!(format_bth_addr(0x0000_80C3_BA2A_6A1D), "80:C3:BA:2A:6A:1D");
    }
}
