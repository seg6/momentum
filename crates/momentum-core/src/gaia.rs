//! GAIA (Generic Application Interface Architecture) typed command layer.
//!
//! Each command is a zero-sized type implementing [`Command`] with:
//!
//! - `Request`: input parameter type (often `()`).
//! - `Response`: parsed output type.
//! - `encode_request` / `decode_response` derived from the type.
//! - Wire identifiers (`COMMAND`, `RESPONSE`, `ERROR`) following the
//!   Sennheiser convention `response = command | 0x0100`,
//!   `error = response | 0x0080`.

use crate::{Error, Result, VENDOR_SENNHEISER, state};

/// A typed GAIA command.
pub trait Command {
    type Request;
    type Response;

    /// Human-readable name used in error messages.
    const NAME: &'static str;
    /// Wire command identifier.
    const COMMAND: u16;
    /// Wire response identifier (success).
    const RESPONSE: u16 = Self::COMMAND | 0x0100;
    /// Wire response identifier (error).
    const ERROR: u16 = Self::RESPONSE | 0x0080;

    fn encode_request(request: &Self::Request, out: &mut Vec<u8>) -> Result<()>;
    fn decode_response(payload: &[u8]) -> Result<Self::Response>;

    /// Build a complete GAIA packet (vendor + command + parameters).
    fn build_packet(request: &Self::Request) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(8);
        out.extend_from_slice(&VENDOR_SENNHEISER.to_be_bytes());
        out.extend_from_slice(&Self::COMMAND.to_be_bytes());
        Self::encode_request(request, &mut out)?;
        Ok(out)
    }

    /// Validate the response header on a GAIA packet and return the payload.
    fn validate_packet(packet: &[u8]) -> Result<&[u8]> {
        if packet.len() < 4 {
            return Err(Error::ShortPacket);
        }
        let response = u16::from_be_bytes([packet[2], packet[3]]);
        let payload = &packet[4..];
        if response == Self::ERROR {
            return Err(Error::GaiaError {
                command: Self::NAME,
                response,
                payload: payload.into(),
            });
        }
        if response != Self::RESPONSE {
            return Err(Error::UnexpectedResponse {
                command: Self::NAME,
                got: response,
                expected: Self::RESPONSE,
            });
        }
        Ok(payload)
    }
}

/// Helper: pull the first `N` bytes off `payload` or return `PayloadTooShort`.
fn take<const N: usize>(command: &'static str, payload: &[u8]) -> Result<[u8; N]> {
    payload
        .get(..N)
        .map(|s| {
            let mut out = [0u8; N];
            out.copy_from_slice(s);
            out
        })
        .ok_or(Error::PayloadTooShort {
            command,
            got: payload.len(),
            expected: N,
        })
}

/// Encoder helpers for common request shapes.
mod encode {
    use crate::Result;

    pub fn unit(_: &(), _out: &mut Vec<u8>) -> Result<()> {
        Ok(())
    }
    pub fn u8(value: &u8, out: &mut Vec<u8>) -> Result<()> {
        out.push(*value);
        Ok(())
    }
}

macro_rules! gaia_commands {
    (
        $(
            $(#[$meta:meta])*
            $name:ident {
                wire: $wire:literal,
                label: $label:literal,
                request: $req:ty,
                encode: $encode:path,
                response: $resp:ty,
                decode: $decode:expr $(,)?
            }
        )*
    ) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy)]
            pub struct $name;

            impl Command for $name {
                type Request = $req;
                type Response = $resp;
                const NAME: &'static str = $label;
                const COMMAND: u16 = $wire;

                fn encode_request(request: &Self::Request, out: &mut Vec<u8>) -> Result<()> {
                    $encode(request, out)
                }

                fn decode_response(payload: &[u8]) -> Result<Self::Response> {
                    let decoder: fn(&[u8]) -> Result<Self::Response> = $decode;
                    decoder(payload)
                }
            }
        )*
    };
}

fn decode_unit(_payload: &[u8]) -> Result<()> {
    Ok(())
}

fn decode_u8_first(payload: &[u8]) -> Result<u8> {
    Ok(take::<1>("u8", payload)?[0])
}

fn decode_battery(payload: &[u8]) -> Result<u8> {
    // m4.json declares `Battery_GetBatteryLevels` (cmd 0x0603) as a
    // repeated UINT8 array. M4 over-ear firmware reports one byte
    // (the whole-device percentage). Any trailing bytes are ignored.
    payload.first().copied().ok_or(Error::PayloadTooShort {
        command: "Battery",
        got: 0,
        expected: 1,
    })
}

fn decode_charging(payload: &[u8]) -> Result<state::Charging> {
    // Same shape: schema declares an array, M4 reports one byte.
    let first = payload.first().copied().ok_or(Error::PayloadTooShort {
        command: "Charging",
        got: 0,
        expected: 1,
    })?;
    state::Charging::try_from(first)
}

fn decode_toggle(payload: &[u8]) -> Result<state::Toggle> {
    Ok(state::Toggle::from(decode_u8_first(payload)?))
}

fn decode_anc_modes(payload: &[u8]) -> Result<state::AncSubModes> {
    // M4 schema (m4.json, cmd 0x1A01 response): payload is exactly 6 bytes,
    // 3 successive (mode_tag, state) pairs in fixed order
    // (1=Wind, 2=Comfort, 3=Adaptive). Tag 2 is a no-op on this firmware
    // and intentionally skipped.
    let bytes = take::<6>("AncSubModes", payload)?;
    let mut out = state::AncSubModes::off();
    for pair in bytes.chunks_exact(2) {
        let (tag, value) = (pair[0], pair[1]);
        match tag {
            1 => out.wind = state::WindState::from(value),
            3 => out.adaptive = value != 0,
            // Comfort (tag 2) and unknown tags ignored.
            _ => {}
        }
    }
    Ok(out)
}

fn decode_text(payload: &[u8]) -> Result<String> {
    Ok(String::from_utf8_lossy(payload)
        .trim_end_matches('\0')
        .to_string())
}

fn decode_version(payload: &[u8]) -> Result<String> {
    if !payload.is_empty() && payload.len().is_multiple_of(2) {
        Ok(payload
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]).to_string())
            .collect::<Vec<_>>()
            .join("."))
    } else {
        decode_text(payload)
    }
}

fn decode_eq_config(payload: &[u8]) -> Result<EqConfig> {
    let bytes = take::<3>("EqConfig", payload)?;
    Ok(EqConfig {
        bands: bytes[0],
        min_gain: bytes[1] as i8,
        max_gain: bytes[2] as i8,
    })
}

fn decode_eq_gain(payload: &[u8]) -> Result<i8> {
    Ok(decode_u8_first(payload)? as i8)
}

fn encode_eq_set(req: &(u8, i8), out: &mut Vec<u8>) -> Result<()> {
    out.push(req.0);
    out.push(req.1 as u8);
    Ok(())
}

fn encode_anc_mode_set(req: &(state::AncSubMode, u8), out: &mut Vec<u8>) -> Result<()> {
    out.push(req.0.as_u8());
    out.push(req.1);
    Ok(())
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct EqConfig {
    pub bands: u8,
    pub min_gain: i8,
    pub max_gain: i8,
}

gaia_commands! {
    BatteryGet {
        wire: 0x0603,
        label: "Battery_GetBatteryLevels",
        request: (),
        encode: encode::unit,
        response: u8,
        decode: decode_battery,
    }
    BatteryChargingGet {
        wire: 0x0602,
        label: "Battery_ChargingState_Get_TW",
        request: (),
        encode: encode::unit,
        response: state::Charging,
        decode: decode_charging,
    }
    AncGet {
        wire: 0x1A05,
        label: "ANC_GetNoiseCancelation",
        request: (),
        encode: encode::unit,
        response: state::Toggle,
        decode: decode_toggle,
    }
    AncSet {
        wire: 0x1A04,
        label: "ANC_SetNoiseCancelation",
        request: u8,
        encode: encode::u8,
        response: (),
        decode: decode_unit,
    }
    AncModeGet {
        wire: 0x1A01,
        label: "ANC_GetNoiseCancelationMode",
        request: (),
        encode: encode::unit,
        response: state::AncSubModes,
        decode: decode_anc_modes,
    }
    AncModeSet {
        wire: 0x1A00,
        label: "ANC_SetNoiseCancelationMode",
        request: (state::AncSubMode, u8),
        encode: encode_anc_mode_set,
        response: (),
        decode: decode_unit,
    }
    TransparencyGet {
        wire: 0x1A03,
        label: "ANC_Transparency_Get",
        request: (),
        encode: encode::unit,
        response: u8,
        decode: decode_u8_first,
    }
    TransparencySet {
        wire: 0x1A02,
        label: "ANC_Transparency_Set",
        request: u8,
        encode: encode::u8,
        response: (),
        decode: decode_unit,
    }
    EqConfigGet {
        wire: 0x1000,
        label: "UserEQ_GetConfiguration",
        request: (),
        encode: encode::unit,
        response: EqConfig,
        decode: decode_eq_config,
    }
    EqGainGet {
        wire: 0x1002,
        label: "UserEQ_GetSliderGain",
        request: u8,
        encode: encode::u8,
        response: i8,
        decode: decode_eq_gain,
    }
    EqGainSet {
        wire: 0x1001,
        label: "UserEQ_SetSliderGain",
        request: (u8, i8),
        encode: encode_eq_set,
        response: (),
        decode: decode_unit,
    }
    ModelGet {
        wire: 0x1206,
        label: "Versions_GetModelId",
        request: (),
        encode: encode::unit,
        response: String,
        decode: decode_text,
    }
    ReleaseGet {
        wire: 0x1201,
        label: "Service_SystemReleaseVersion",
        request: (),
        encode: encode::unit,
        response: String,
        decode: decode_version,
    }
    HwRevisionGet {
        wire: 0x1200,
        label: "Service_GetHwRevision",
        request: (),
        encode: encode::unit,
        response: String,
        decode: decode_version,
    }
    FirmwareVersionsGet {
        wire: 0x1202,
        label: "System_Version_Get",
        request: (),
        encode: encode::unit,
        response: String,
        decode: decode_version,
    }
    /// Subscribe to notifications emitted by a Sennheiser feature plugin
    /// (`Config_RegisterNotification` on vendor `0x0495`). `request` is the
    /// 1-byte `featureID`; observed values on the M4 are 0, 2, 3, 4, 8, 9,
    /// 10, 11, 12, 13. The response carries no payload.
    RegisterNotificationSennheiser {
        wire: 0x0007,
        label: "Sennheiser_RegisterNotification",
        request: u8,
        encode: encode::u8,
        response: (),
        decode: decode_unit,
    }
}

/// Subscribe to notifications emitted by a **Qualcomm-core** feature
/// (`Config_RegisterNotification` on vendor `0x001D`). Distinct from
/// [`RegisterNotificationSennheiser`] because the vendor id is not the
/// Sennheiser default; it is the only command in this crate that uses
/// `VENDOR_QUALCOMM`, so it is hand-written rather than wedged into the
/// `gaia_commands!` macro.
#[derive(Debug, Clone, Copy)]
pub struct RegisterNotificationQualcomm;

impl Command for RegisterNotificationQualcomm {
    type Request = u8;
    type Response = ();
    const NAME: &'static str = "Qualcomm_RegisterNotification";
    const COMMAND: u16 = 0x0007;

    fn encode_request(request: &Self::Request, out: &mut Vec<u8>) -> Result<()> {
        out.push(*request);
        Ok(())
    }
    fn decode_response(_payload: &[u8]) -> Result<Self::Response> {
        Ok(())
    }

    fn build_packet(request: &Self::Request) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(5);
        out.extend_from_slice(&crate::VENDOR_QUALCOMM.to_be_bytes());
        out.extend_from_slice(&Self::COMMAND.to_be_bytes());
        Self::encode_request(request, &mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anc_get_wire_ids_follow_convention() {
        assert_eq!(AncGet::COMMAND, 0x1A05);
        assert_eq!(AncGet::RESPONSE, 0x1B05);
        assert_eq!(AncGet::ERROR, 0x1B85);
    }

    #[test]
    fn build_packet_for_unit_request() {
        let packet = AncGet::build_packet(&()).unwrap();
        assert_eq!(packet, [0x04, 0x95, 0x1a, 0x05]);
    }

    #[test]
    fn build_packet_for_eq_set() {
        let packet = EqGainSet::build_packet(&(2, -6)).unwrap();
        assert_eq!(packet, [0x04, 0x95, 0x10, 0x01, 0x02, 0xfa]);
    }

    #[test]
    fn validate_packet_rejects_error_response() {
        let pkt = [0x04, 0x95, 0x1b, 0x85, 0xff];
        let err = AncGet::validate_packet(&pkt).unwrap_err();
        assert!(matches!(err, Error::GaiaError { .. }));
    }

    #[test]
    fn decode_charging_via_full_pipeline() {
        let pkt = [0x04, 0x95, 0x07, 0x02, 0x01];
        let payload = BatteryChargingGet::validate_packet(&pkt).unwrap();
        let charging = BatteryChargingGet::decode_response(payload).unwrap();
        assert_eq!(charging, state::Charging::Charging);
    }

    #[test]
    fn decode_battery_returns_scalar_percent() {
        let pkt = [0x04, 0x95, 0x07, 0x03, 80];
        let payload = BatteryGet::validate_packet(&pkt).unwrap();
        let percent = BatteryGet::decode_response(payload).unwrap();
        assert_eq!(percent, 80);
    }

    #[test]
    fn decode_battery_ignores_trailing_slots() {
        // If firmware ever sends extras, the first byte still wins.
        let pkt = [0x04, 0x95, 0x07, 0x03, 80, 75, 60, 0xFF];
        let payload = BatteryGet::validate_packet(&pkt).unwrap();
        assert_eq!(BatteryGet::decode_response(payload).unwrap(), 80);
    }

    #[test]
    fn decode_anc_modes_parses_six_byte_pairs() {
        let pkt = [0x04, 0x95, 0x1b, 0x01, 0x01, 0x02, 0x02, 0x01, 0x03, 0x00];
        let payload = AncModeGet::validate_packet(&pkt).unwrap();
        let modes = AncModeGet::decode_response(payload).unwrap();
        assert_eq!(modes.wind, state::WindState::Auto);
        assert!(!modes.adaptive);
    }

    #[test]
    fn register_notification_qualcomm_uses_vendor_0x001d() {
        let packet = RegisterNotificationQualcomm::build_packet(&0x03).unwrap();
        assert_eq!(packet, [0x00, 0x1d, 0x00, 0x07, 0x03]);
    }

    #[test]
    fn register_notification_sennheiser_uses_vendor_0x0495() {
        let packet = RegisterNotificationSennheiser::build_packet(&0x0c).unwrap();
        assert_eq!(packet, [0x04, 0x95, 0x00, 0x07, 0x0c]);
    }

    #[test]
    fn register_notification_response_ids_apply_response_bit() {
        assert_eq!(RegisterNotificationQualcomm::RESPONSE, 0x0107);
        assert_eq!(RegisterNotificationSennheiser::RESPONSE, 0x0107);
    }
}
