//! Typed device-pushed notifications.
//!
//! The headset emits unsolicited GAIA frames when its state changes
//! (battery, wear-state, ANC, etc.). The transport layer surfaces them
//! verbatim as [`NotificationFrame`]; this module decodes them into a
//! [`Notification`] enum that mirrors the typed `state` module.
//!
//! The enum carries one variant per notification ID we recognise plus a
//! generic [`Notification::Unknown`] fallback so a firmware update that
//! adds a new event does not break the daemon.
//!
//! ## Schema cross-reference
//!
//! Mapping derived from `reference/m4.json` and `reference/audio-mode-1.json`:
//!
//! | Opcode  | Property                       | Decoder              |
//! |---------|--------------------------------|----------------------|
//! | 0x0282  | Upgrade_Progress               | `(u8 phase, u8 pct)` |
//! | 0x0482  | PhysicalDevice_State           | [`state::WearState`] |
//! | 0x0682  | Battery_ChargingStatus         | [`state::Charging`]  |
//! | 0x0683  | Battery_Percent                | `u8`                 |
//! | 0x0880  | Sound_CodecUsed                | `u8`                 |
//! | 0x0884  | AudioMode (preset)             | [`state::Preset`]    |
//! | 0x0898  | LowLatencyMode                 | [`state::Toggle`]    |
//! | 0x089A  | Sound_Sample_Rate              | `u32` Hz             |
//! | 0x1082  | user-eq-2 (gain changed)       | `Vec<i8>` centibel   |
//! | 0x1089  | user-eq-2 (sub-mode changed)   | `u8`                 |
//! | 0x108B  | UserEQ band center frequencies | `Vec<EqBandU16>` Hz  |
//! | 0x108D  | UserEQ band Q-factors          | `Vec<EqBandU16>` Q×1000 |
//! | 0x108F  | UserEQ band filter type        | `Vec<EqBandU8>` (enum, semantics undocumented) |
//! | 0x1091  | UserEQ per-band unknown        | `Vec<EqBandU16>` (semantics undocumented) |
//! | 0x1093  | UserEQ status / preset id      | `Vec<u8>` (semantics undocumented) |
//! | 0x1484  | PairedDevices_ConnectionEvent  | `(u8 idx, u8 stat)`  |
//! | 0x1687  | Setting_AutoLockMode (touch)   | [`state::Toggle`]    |
//! | 0x1883  | TransparentHearing             | [`state::Toggle`]    |
//! | 0x1885  | TransparentHearing_Status      | [`state::Toggle`]    |
//! | 0x1A81  | ANC sub-mode pair table        | [`state::AncSubModes`] |
//! | 0x1A83  | ANC_Transparency level         | `u8`                 |
//! | 0x1A85  | ANC_Status                     | [`state::Toggle`]    |
//! | 0x2081  | PersonalizedSound_Mode (push)  | [`state::PersonalizationMode`] |
//! | 0x2088  | PersonalizedSound_Timeout      | (no payload)         |
//!
//! ## Qualcomm-vendor frames
//!
//! The device also pushes frames under vendor `0x001D` (Qualcomm GAIA
//! core) that are not declared as notifications in any local schema.
//! The only observed instance on M4 fw 3.37.3 is opcode `0x0080`
//! (notification-shape of Qualcomm core command `0x0000 Core_Api_Version`)
//! arriving once after `Connect` with payload `[0]`. We surface these
//! as [`Notification::QualcommCore`] so they are labeled and
//! inspectable rather than falling into [`Notification::Unknown`].

use crate::state::{
    AncSubMode, AncSubModes, Charging, PersonalizationMode, Preset, Toggle, WearState, WindState,
};
use crate::{NotificationFrame, VENDOR_QUALCOMM, VENDOR_SENNHEISER};
use serde::Serialize;

/// Stable cache-key for [`Notification`] values, suitable for use in a
/// "last value per kind" replay cache.
///
/// Most variants collapse to a unit key (e.g. `Battery`, `WearState`),
/// but variants that carry an identifying opcode (`QualcommCore`,
/// `Unknown`) split their key so distinct opcodes do not overwrite
/// each other in the cache. Returned by [`Notification::cache_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationKey {
    UpgradeProgress,
    WearState,
    Charging,
    Battery,
    Codec,
    SampleRate,
    Preset,
    LowLatency,
    EqGain,
    EqSubMode,
    EqBandFrequencies,
    EqBandQFactors,
    EqBandTypes,
    EqBandParam1091,
    EqStatus,
    ConnectionChanged,
    TouchLock,
    TransparentHearing,
    TransparentHearingStatus,
    AncModes,
    AncTransparencyLevel,
    AncStatus,
    /// `sound-personalization-1` `PersonalizedSound_Mode_GetRsp`
    /// (notif `0x2081`): the personalized-sound feature mode.
    PersonalizationMode,
    /// `sound-personalization-1` `PersonalizedSound_Timeout_RSP`
    /// (notif `0x2088`): bare timeout signal, empty payload.
    PersonalizationTimeout,
    /// Qualcomm-vendor frame keyed by the underlying GAIA command id
    /// (notification bit already stripped) so distinct Qualcomm pushes
    /// keep separate cache slots.
    QualcommCore(u16),
    /// Untyped frame keyed by `(vendor, opcode)` so distinct unknown
    /// opcodes do not collide.
    Unknown(u16, u16),
}

/// A device-pushed event decoded into its typed form.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notification {
    UpgradeProgress {
        phase: u8,
        percent: u8,
    },
    WearState {
        state: WearState,
    },
    Charging {
        status: Charging,
    },
    Battery {
        percent: u8,
    },
    Codec {
        codec: u8,
    },
    /// Active SPP audio sample rate in Hz (UINT32 big-endian).
    /// Schema: `tw4.json` etc., `Sound_Sample_Rate` notification `0x089a`.
    /// Not documented in `m4.json` but the M4 firmware emits it anyway.
    SampleRate {
        hz: u32,
    },
    Preset {
        preset: Preset,
    },
    LowLatency {
        state: Toggle,
    },
    EqGain {
        /// Per-band gain in centibel (1 dB = 10 cB), signed INT8.
        /// Schema: `user-eq-2` `UserEQ_GetSliderGain_Notification`,
        /// fixed-size 5-element array.
        gains_centibel: Vec<i8>,
    },
    EqSubMode {
        mask: u8,
    },
    /// EQ band center frequencies in Hz (UINT16 BE per band, `[band_id, hi, lo]` triples).
    /// Opcode `0x108B`. Five bands on the M4.
    /// Semantics confirmed by inspection of M4 fw 3.37.3 push values:
    /// 90, 325, 1500, 6500, 6500 Hz match the documented M4 EQ band centers.
    EqBandFrequencies {
        bands: Vec<EqBandU16>,
    },
    /// EQ band Q-factors (UINT16 BE per band, `[band_id, hi, lo]` triples).
    /// Opcode `0x108D`. Units suspected to be Q × 1000 (raw bytes preserved).
    EqBandQFactors {
        bands: Vec<EqBandU16>,
    },
    /// EQ band filter type / shape (UINT8 per band, `[band_id, value]` pairs).
    /// Opcode `0x108F`. Enum semantics undocumented in available schemas;
    /// raw values preserved for downstream inspection.
    EqBandTypes {
        bands: Vec<EqBandU8>,
    },
    /// Per-band EQ parameter at opcode `0x1091` (UINT16 BE per band).
    /// Schema-undocumented; raw values preserved.
    EqBandParam1091 {
        bands: Vec<EqBandU16>,
    },
    /// EQ status / selected preset id at opcode `0x1093`.
    /// Schema-undocumented; raw bytes preserved (observed `[0, 0]`).
    EqStatus {
        bytes: Vec<u8>,
    },
    ConnectionChanged {
        device_index: u8,
        status: u8,
    },
    TouchLock {
        state: Toggle,
    },
    TransparentHearing {
        state: Toggle,
    },
    TransparentHearingStatus {
        state: Toggle,
    },
    AncModes {
        modes: AncSubModes,
    },
    AncTransparencyLevel {
        level: u8,
    },
    AncStatus {
        state: Toggle,
    },
    /// `sound-personalization-1` `PersonalizedSound_Mode_GetRsp`
    /// (notif `0x2081`). Single `UINT8` enum payload.
    PersonalizationMode {
        mode: PersonalizationMode,
    },
    /// `sound-personalization-1` `PersonalizedSound_Timeout_RSP`
    /// (notif `0x2088`). Schema declares an empty parameter list, so
    /// the variant carries no data — its arrival is itself the signal.
    PersonalizationTimeout,
    /// Qualcomm GAIA core (vendor `0x001D`) device-pushed frame whose
    /// opcode is not documented as a notification in any local schema.
    /// `command` is the underlying GAIA command id with the
    /// notification bit cleared (`opcode & !0x0080`). Carries the raw
    /// payload for downstream tooling.
    QualcommCore {
        command: u16,
        bytes: Vec<u8>,
    },
    Unknown {
        vendor: u16,
        opcode: u16,
        bytes: Vec<u8>,
    },
}

/// Per-band UINT16-BE EQ parameter as carried by GAIA notifications
/// `0x108B`, `0x108D`, `0x1091`. Each band is encoded as the 3-byte
/// triple `[band_id, value_hi, value_lo]`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EqBandU16 {
    pub band: u8,
    pub value: u16,
}

/// Per-band UINT8 EQ parameter as carried by GAIA notification `0x108F`.
/// Each band is encoded as the 2-byte pair `[band_id, value]`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EqBandU8 {
    pub band: u8,
    pub value: u8,
}

impl Notification {
    /// Decode a raw [`NotificationFrame`] into a typed [`Notification`].
    /// Always succeeds — payloads that fail to parse fall back to
    /// [`Notification::Unknown`] so a malformed event does not poison
    /// the subscriber channel.
    pub fn decode(frame: &NotificationFrame) -> Self {
        match (frame.vendor, frame.opcode) {
            (VENDOR_SENNHEISER, 0x0282) => match frame.payload.as_slice() {
                [phase, percent, ..] => Self::UpgradeProgress {
                    phase: *phase,
                    percent: *percent,
                },
                _ => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x0482) => match frame.payload.first() {
                Some(byte) => match WearState::try_from(*byte) {
                    Ok(state) => Self::WearState { state },
                    Err(_) => Self::unknown(frame),
                },
                None => Self::unknown(frame),
            },
            // Sennheiser `PhysicalDevice_State_GetRsp` opcode (cmd
            // `0x0402` + response bit). Schema-wise this is a Get
            // response shape, but the M4 firmware pushes it
            // proactively on connect as a "current wear state"
            // snapshot. Same payload contract as the `0x0482`
            // notification, so we surface it the same way.
            (VENDOR_SENNHEISER, 0x0502) => match frame.payload.first() {
                Some(byte) => match WearState::try_from(*byte) {
                    Ok(state) => Self::WearState { state },
                    Err(_) => Self::unknown(frame),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x0682) => match frame.payload.first() {
                Some(byte) => match Charging::try_from(*byte) {
                    Ok(status) => Self::Charging { status },
                    Err(_) => Self::unknown(frame),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x0683) => match frame.payload.first() {
                Some(byte) => Self::Battery { percent: *byte },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x0880) => match frame.payload.first() {
                Some(byte) => Self::Codec { codec: *byte },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x0884) => match frame.payload.get(..2) {
                Some(bytes) => Self::Preset {
                    preset: Preset::from(u16::from_be_bytes([bytes[0], bytes[1]])),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x0898) => match frame.payload.first() {
                Some(byte) => Self::LowLatency {
                    state: Toggle::from(*byte),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x089a) => match frame.payload.get(..4) {
                Some(bytes) => Self::SampleRate {
                    hz: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x1082) => Self::EqGain {
                gains_centibel: frame.payload.iter().map(|b| *b as i8).collect(),
            },
            (VENDOR_SENNHEISER, 0x1089) => match frame.payload.first() {
                Some(byte) => Self::EqSubMode { mask: *byte },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x108B) => decode_eq_bands_u16(&frame.payload)
                .map(|bands| Self::EqBandFrequencies { bands })
                .unwrap_or_else(|| Self::unknown(frame)),
            (VENDOR_SENNHEISER, 0x108D) => decode_eq_bands_u16(&frame.payload)
                .map(|bands| Self::EqBandQFactors { bands })
                .unwrap_or_else(|| Self::unknown(frame)),
            (VENDOR_SENNHEISER, 0x108F) => decode_eq_bands_u8(&frame.payload)
                .map(|bands| Self::EqBandTypes { bands })
                .unwrap_or_else(|| Self::unknown(frame)),
            (VENDOR_SENNHEISER, 0x1091) => decode_eq_bands_u16(&frame.payload)
                .map(|bands| Self::EqBandParam1091 { bands })
                .unwrap_or_else(|| Self::unknown(frame)),
            (VENDOR_SENNHEISER, 0x1093) => Self::EqStatus {
                bytes: frame.payload.clone(),
            },
            (VENDOR_SENNHEISER, 0x1484) => match frame.payload.get(..2) {
                Some([idx, status]) => Self::ConnectionChanged {
                    device_index: *idx,
                    status: *status,
                },
                _ => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x1687) => match frame.payload.first() {
                Some(byte) => Self::TouchLock {
                    state: Toggle::from(*byte),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x1883) => match frame.payload.first() {
                Some(byte) => Self::TransparentHearing {
                    state: Toggle::from(*byte),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x1885) => match frame.payload.first() {
                Some(byte) => Self::TransparentHearingStatus {
                    state: Toggle::from(*byte),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x1A81) => decode_anc_modes(&frame.payload)
                .map(|modes| Self::AncModes { modes })
                .unwrap_or_else(|| Self::unknown(frame)),
            (VENDOR_SENNHEISER, 0x1A83) => match frame.payload.first() {
                Some(byte) => Self::AncTransparencyLevel { level: *byte },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x1A85) => match frame.payload.first() {
                Some(byte) => Self::AncStatus {
                    state: Toggle::from(*byte),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x2081) => match frame.payload.first() {
                Some(byte) => match PersonalizationMode::try_from(*byte) {
                    Ok(mode) => Self::PersonalizationMode { mode },
                    Err(_) => Self::unknown(frame),
                },
                None => Self::unknown(frame),
            },
            (VENDOR_SENNHEISER, 0x2088) => Self::PersonalizationTimeout,
            // Qualcomm core (vendor `0x001D`) frames that arrive on the
            // notification channel. The on-wire opcode has the
            // notification/response bit `0x0080` set; we strip it so
            // callers see the underlying command id (e.g. `0x0000` =
            // `Core_Api_Version`). Observed on M4 fw 3.37.3: opcode
            // `0x0080` payload `[0]` once after `Connect`.
            (VENDOR_QUALCOMM, opcode) => Self::QualcommCore {
                command: opcode & !0x0080,
                bytes: frame.payload.clone(),
            },
            _ => Self::unknown(frame),
        }
    }

    fn unknown(frame: &NotificationFrame) -> Self {
        Self::Unknown {
            vendor: frame.vendor,
            opcode: frame.opcode,
            bytes: frame.payload.clone(),
        }
    }

    /// Stable identifier for this notification's "kind", suitable as a
    /// hash-map key in a last-value replay cache. Variants that carry
    /// an identifying opcode (`QualcommCore`, `Unknown`) qualify the
    /// key so distinct opcodes do not overwrite each other.
    #[must_use]
    pub fn cache_key(&self) -> NotificationKey {
        match self {
            Self::UpgradeProgress { .. } => NotificationKey::UpgradeProgress,
            Self::WearState { .. } => NotificationKey::WearState,
            Self::Charging { .. } => NotificationKey::Charging,
            Self::Battery { .. } => NotificationKey::Battery,
            Self::Codec { .. } => NotificationKey::Codec,
            Self::SampleRate { .. } => NotificationKey::SampleRate,
            Self::Preset { .. } => NotificationKey::Preset,
            Self::LowLatency { .. } => NotificationKey::LowLatency,
            Self::EqGain { .. } => NotificationKey::EqGain,
            Self::EqSubMode { .. } => NotificationKey::EqSubMode,
            Self::EqBandFrequencies { .. } => NotificationKey::EqBandFrequencies,
            Self::EqBandQFactors { .. } => NotificationKey::EqBandQFactors,
            Self::EqBandTypes { .. } => NotificationKey::EqBandTypes,
            Self::EqBandParam1091 { .. } => NotificationKey::EqBandParam1091,
            Self::EqStatus { .. } => NotificationKey::EqStatus,
            Self::ConnectionChanged { .. } => NotificationKey::ConnectionChanged,
            Self::TouchLock { .. } => NotificationKey::TouchLock,
            Self::TransparentHearing { .. } => NotificationKey::TransparentHearing,
            Self::TransparentHearingStatus { .. } => NotificationKey::TransparentHearingStatus,
            Self::AncModes { .. } => NotificationKey::AncModes,
            Self::AncTransparencyLevel { .. } => NotificationKey::AncTransparencyLevel,
            Self::AncStatus { .. } => NotificationKey::AncStatus,
            Self::PersonalizationMode { .. } => NotificationKey::PersonalizationMode,
            Self::PersonalizationTimeout => NotificationKey::PersonalizationTimeout,
            Self::QualcommCore { command, .. } => NotificationKey::QualcommCore(*command),
            Self::Unknown { vendor, opcode, .. } => NotificationKey::Unknown(*vendor, *opcode),
        }
    }
}

/// Decode an EQ per-band UINT16-BE payload as carried by notifications
/// `0x108B`, `0x108D`, `0x1091`. Each band is a 3-byte triple
/// `[band_id, value_hi, value_lo]`. Returns `None` if the payload is
/// empty or not a multiple of 3 bytes.
fn decode_eq_bands_u16(payload: &[u8]) -> Option<Vec<EqBandU16>> {
    if payload.is_empty() || !payload.len().is_multiple_of(3) {
        return None;
    }
    Some(
        payload
            .chunks_exact(3)
            .map(|c| EqBandU16 {
                band: c[0],
                value: u16::from_be_bytes([c[1], c[2]]),
            })
            .collect(),
    )
}

/// Decode an EQ per-band UINT8 payload as carried by notification
/// `0x108F`. Each band is a 2-byte pair `[band_id, value]`. Returns
/// `None` if the payload is empty or not a multiple of 2 bytes.
fn decode_eq_bands_u8(payload: &[u8]) -> Option<Vec<EqBandU8>> {
    if payload.is_empty() || !payload.len().is_multiple_of(2) {
        return None;
    }
    Some(
        payload
            .chunks_exact(2)
            .map(|c| EqBandU8 {
                band: c[0],
                value: c[1],
            })
            .collect(),
    )
}

/// Decode the 6-byte ANC pair payload `[1, wind, 2, comfort, 3, adaptive]`
/// as emitted by both `0x1A01` (response) and `0x1A81` (notification).
/// The `comfort` pair (tag 2) is intentionally ignored: on M4 fw 3.37.3
/// it is a no-op that Smart Control does not surface.
///
/// Returns `None` only if the payload is too short.
fn decode_anc_modes(payload: &[u8]) -> Option<AncSubModes> {
    if payload.len() < 6 {
        return None;
    }
    let pairs = [
        (payload[0], payload[1]),
        (payload[2], payload[3]),
        (payload[4], payload[5]),
    ];
    let mut wind = WindState::Unknown(0);
    let mut adaptive = false;
    for (mode, state) in pairs {
        match AncSubMode::try_from(mode) {
            Ok(AncSubMode::Wind) => wind = WindState::from(state),
            Ok(AncSubMode::Adaptive) => adaptive = state != 0,
            // Comfort (2) and any future tags pass through silently.
            Err(_) => {}
        }
    }
    Some(AncSubModes { wind, adaptive })
}

/// Vendor IDs that may legitimately produce notifications on this device.
/// Sennheiser-vendor events are decoded individually; Qualcomm-core events
/// currently surface as [`Notification::QualcommCore`].
pub const NOTIFICATION_VENDORS: &[u16] = &[VENDOR_SENNHEISER, VENDOR_QUALCOMM];

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(opcode: u16, payload: &[u8]) -> NotificationFrame {
        NotificationFrame {
            vendor: VENDOR_SENNHEISER,
            opcode,
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn decodes_battery() {
        let n = Notification::decode(&frame(0x0683, &[42]));
        match n {
            Notification::Battery { percent } => assert_eq!(percent, 42),
            other => panic!("expected Battery, got {other:?}"),
        }
    }

    #[test]
    fn decodes_anc_status() {
        let n = Notification::decode(&frame(0x1A85, &[1]));
        match n {
            Notification::AncStatus { state } => assert_eq!(state, Toggle::On),
            other => panic!("expected AncStatus, got {other:?}"),
        }
    }

    #[test]
    fn decodes_anc_modes_pairs() {
        let n = Notification::decode(&frame(0x1A81, &[1, 2, 2, 1, 3, 0]));
        match n {
            Notification::AncModes { modes } => {
                assert_eq!(modes.wind, WindState::Auto);
                assert!(!modes.adaptive);
            }
            other => panic!("expected AncModes, got {other:?}"),
        }
    }

    #[test]
    fn decodes_wear_state() {
        let n = Notification::decode(&frame(0x0482, &[3]));
        match n {
            Notification::WearState { state } => assert_eq!(state, WearState::OnHead),
            other => panic!("expected WearState OnHead, got {other:?}"),
        }
    }

    #[test]
    fn unknown_opcode_round_trips() {
        let n = Notification::decode(&frame(0xDEAD, &[1, 2, 3]));
        match n {
            Notification::Unknown {
                vendor,
                opcode,
                bytes,
            } => {
                assert_eq!(vendor, VENDOR_SENNHEISER);
                assert_eq!(opcode, 0xDEAD);
                assert_eq!(bytes, vec![1, 2, 3]);
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn empty_payload_falls_back_to_unknown() {
        let n = Notification::decode(&frame(0x0683, &[]));
        assert!(matches!(n, Notification::Unknown { .. }));
    }

    #[test]
    fn serialize_is_tagged_snake_case() {
        let n = Notification::Battery { percent: 80 };
        let s = serde_json::to_string(&n).unwrap();
        assert_eq!(s, r#"{"kind":"battery","percent":80}"#);
    }

    #[test]
    fn sample_rate_decodes_u32_be() {
        // On-wire bytes 0x0000BB80 = 48_000 Hz, captured from M4 fw 3.37.3.
        let frame = NotificationFrame {
            vendor: 0x0495,
            opcode: 0x089a,
            payload: vec![0x00, 0x00, 0xBB, 0x80],
        };
        match Notification::decode(&frame) {
            Notification::SampleRate { hz } => assert_eq!(hz, 48_000),
            other => panic!("expected SampleRate, got {other:?}"),
        }
    }

    #[test]
    fn eq_gain_decodes_signed_centibel() {
        // On-wire bytes [30, 15, -15, 0, -15] (15->1.5dB, -15->-1.5dB, etc.)
        let frame = NotificationFrame {
            vendor: 0x0495,
            opcode: 0x1082,
            payload: vec![30, 15, 241, 0, 241], // 241 = -15 in i8
        };
        match Notification::decode(&frame) {
            Notification::EqGain { gains_centibel } => {
                assert_eq!(gains_centibel, vec![30i8, 15, -15, 0, -15]);
            }
            other => panic!("expected EqGain, got {other:?}"),
        }
    }

    #[test]
    fn eq_band_frequencies_match_m4_centers() {
        // Captured on-wire push from M4 fw 3.37.3:
        //   opcode 0x108B payload = [0,0,90, 1,1,69, 2,5,220, 3,25,100, 4,25,100]
        // Decoded as `[band, u16_be]` per band:
        //   band 0 = 90 Hz, 1 = 325 Hz, 2 = 1500 Hz, 3 = 6500 Hz, 4 = 6500 Hz
        // These match the documented M4 EQ band centers.
        let n = Notification::decode(&frame(
            0x108B,
            &[0, 0, 90, 1, 1, 69, 2, 5, 220, 3, 25, 100, 4, 25, 100],
        ));
        match n {
            Notification::EqBandFrequencies { bands } => {
                let vals: Vec<(u8, u16)> = bands.iter().map(|b| (b.band, b.value)).collect();
                assert_eq!(
                    vals,
                    vec![(0, 90), (1, 325), (2, 1500), (3, 6500), (4, 6500)]
                );
            }
            other => panic!("expected EqBandFrequencies, got {other:?}"),
        }
    }

    #[test]
    fn eq_band_qfactors_decodes_u16_per_band() {
        // Captured: opcode 0x108D payload = [0,8,0, 1,11,92, 2,11,92, 3,11,92, 4,11,92]
        let n = Notification::decode(&frame(
            0x108D,
            &[0, 8, 0, 1, 11, 92, 2, 11, 92, 3, 11, 92, 4, 11, 92],
        ));
        match n {
            Notification::EqBandQFactors { bands } => {
                let vals: Vec<(u8, u16)> = bands.iter().map(|b| (b.band, b.value)).collect();
                assert_eq!(
                    vals,
                    vec![(0, 2048), (1, 2908), (2, 2908), (3, 2908), (4, 2908)]
                );
            }
            other => panic!("expected EqBandQFactors, got {other:?}"),
        }
    }

    #[test]
    fn eq_band_types_decodes_u8_per_band() {
        // Captured: opcode 0x108F payload = [0,14, 1,14, 2,14, 3,14, 4,14]
        let n = Notification::decode(&frame(0x108F, &[0, 14, 1, 14, 2, 14, 3, 14, 4, 14]));
        match n {
            Notification::EqBandTypes { bands } => {
                assert_eq!(bands.len(), 5);
                assert!(bands.iter().all(|b| b.value == 14));
                assert_eq!(
                    bands.iter().map(|b| b.band).collect::<Vec<_>>(),
                    vec![0, 1, 2, 3, 4]
                );
            }
            other => panic!("expected EqBandTypes, got {other:?}"),
        }
    }

    #[test]
    fn eq_band_param_1091_decodes_u16_per_band() {
        // Captured: opcode 0x1091 payload = [0,0,0, 1,0,0, 2,0,0, 3,0,0, 4,0,0]
        let n = Notification::decode(&frame(
            0x1091,
            &[0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0],
        ));
        match n {
            Notification::EqBandParam1091 { bands } => {
                assert_eq!(bands.len(), 5);
                assert!(bands.iter().all(|b| b.value == 0));
            }
            other => panic!("expected EqBandParam1091, got {other:?}"),
        }
    }

    #[test]
    fn eq_status_1093_preserves_raw_bytes() {
        let n = Notification::decode(&frame(0x1093, &[0, 0]));
        match n {
            Notification::EqStatus { bytes } => assert_eq!(bytes, vec![0, 0]),
            other => panic!("expected EqStatus, got {other:?}"),
        }
    }

    #[test]
    fn eq_band_frequencies_malformed_falls_back_to_unknown() {
        // 4 bytes is not a multiple of 3 -> Unknown.
        let n = Notification::decode(&frame(0x108B, &[0, 0, 90, 1]));
        assert!(matches!(n, Notification::Unknown { .. }));
    }

    #[test]
    fn qualcomm_core_0x0080_decodes_with_stripped_bit() {
        // M4 fw 3.37.3 post-connect push: vendor=0x001D opcode=0x0080
        // payload=[0]. Notification bit `0x0080` is stripped so the
        // surfaced command id is `0x0000` (= Core_Api_Version).
        let frame = NotificationFrame {
            vendor: VENDOR_QUALCOMM,
            opcode: 0x0080,
            payload: vec![0],
        };
        match Notification::decode(&frame) {
            Notification::QualcommCore { command, bytes } => {
                assert_eq!(command, 0x0000);
                assert_eq!(bytes, vec![0]);
            }
            other => panic!("expected QualcommCore, got {other:?}"),
        }
    }
}
