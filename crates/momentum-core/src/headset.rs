//! High-level device facade.
//!
//! Wraps a [`Transport`] and exposes typed methods that back the
//! `momentum` CLI verbs (`battery`, `anc`, `transparency`, `eq`,
//! `info`, `status`, `watch`). All wire details ([`crate::gaia`],
//! [`crate::spp`]) stay inside this crate.

use serde::{Deserialize, Serialize};

use crate::{
    Error, NotificationFrame, Result, Transport,
    gaia::{self, Command, EqConfig},
    state::{self, AncSubMode, AncSubModes, Toggle},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub model: String,
    pub release: String,
    pub hw_revision: String,
    pub gaia_build: String,
}

/// Snapshot of common runtime state, suitable for `momentum status` output.
///
/// Intentionally narrow: every field maps to a CLI verb that can read
/// it directly. Adding a field here without a corresponding verb is a
/// design smell.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowState {
    pub battery: u8,
    pub charging: state::Charging,
    pub noise: Toggle,
    pub anc_modes: AncSubModes,
    pub transparency: u8,
}

pub struct Headset<T: Transport> {
    transport: T,
}

/// Qualcomm-core feature IDs used by `RegisterNotification_q*` (vendor
/// `0x001D`, cmd `0x0007`). Order matches m4.json schema declaration.
///
/// **Note:** featureIDs 1 (`qEarbud`), 2 (`qAudioCuration`) and 3
/// (`qVoiceUI`) consistently reject registration with error 5
/// (InvalidParameter) on M4 fw 3.37.3 — the schema declares them but
/// the firmware doesn't actually route notifications through them.
/// We keep only the IDs the device accepts so we don't spam warnings
/// every connect.
const QUALCOMM_FEATURE_IDS: &[u8] = &[0, 4, 6];

/// Sennheiser-vendor feature IDs used by `RegisterNotification_*` (vendor
/// `0x0495`, cmd `0x0007`). Order matches m4.json schema declaration.
const SENNHEISER_FEATURE_IDS: &[u8] = &[0, 2, 3, 4, 8, 9, 10, 11, 12, 13];

impl<T: Transport> core::fmt::Debug for Headset<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Headset").finish_non_exhaustive()
    }
}

impl<T: Transport> Headset<T> {
    pub const fn new(transport: T) -> Self {
        Self { transport }
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    /// Subscribe to the transport's raw notification stream. Returns `None`
    /// when the underlying transport does not surface notifications (e.g.
    /// the mock used in unit tests).
    pub fn notifications(&self) -> Option<crossbeam_channel::Receiver<NotificationFrame>> {
        self.transport.subscribe()
    }

    /// Issue every `RegisterNotification_*` call observed in the Sennheiser
    /// `m4.json` schema. Per-feature failures are returned as a list rather
    /// than aborting the whole call — see the source for the rationale.
    pub fn subscribe_all(&mut self) -> Vec<(u16, u8, Error)> {
        let mut failures = Vec::new();
        for feature_id in QUALCOMM_FEATURE_IDS {
            if let Err(error) = self.call::<gaia::RegisterNotificationQualcomm>(feature_id) {
                failures.push((crate::VENDOR_QUALCOMM, *feature_id, error));
            }
        }
        for feature_id in SENNHEISER_FEATURE_IDS {
            if let Err(error) = self.call::<gaia::RegisterNotificationSennheiser>(feature_id) {
                failures.push((crate::VENDOR_SENNHEISER, *feature_id, error));
            }
        }
        failures
    }

    pub fn call<C: Command>(&mut self, request: &C::Request) -> Result<C::Response> {
        let packet = C::build_packet(request)?;
        let response = self.transport.transact(&packet)?;
        let payload = C::validate_packet(&response)?;
        C::decode_response(payload)
    }

    pub fn battery(&mut self) -> Result<u8> {
        self.call::<gaia::BatteryGet>(&())
    }
    pub fn charging(&mut self) -> Result<state::Charging> {
        self.call::<gaia::BatteryChargingGet>(&())
    }

    pub fn anc(&mut self) -> Result<Toggle> {
        self.call::<gaia::AncGet>(&())
    }
    pub fn set_anc(&mut self, on: Toggle) -> Result<()> {
        self.call::<gaia::AncSet>(&on.as_u8())
    }
    pub fn anc_modes(&mut self) -> Result<AncSubModes> {
        self.call::<gaia::AncModeGet>(&())
    }
    /// Set a single ANC sub-mode (wind/adaptive).
    ///
    /// For `Adaptive`, `value` is `0`/`1` (off/on). For `Wind`, the
    /// firmware also accepts `2` (auto). Values outside `0..=2` are
    /// passed through unchanged; the headset rejects unsupported
    /// values with a GAIA error.
    pub fn set_anc_mode(&mut self, sub: AncSubMode, value: u8) -> Result<()> {
        self.call::<gaia::AncModeSet>(&(sub, value))
    }

    pub fn transparency(&mut self) -> Result<u8> {
        self.call::<gaia::TransparencyGet>(&())
    }
    pub fn set_transparency(&mut self, level: u8) -> Result<()> {
        if level > 0 && self.anc_modes()?.adaptive {
            self.set_anc_mode(AncSubMode::Adaptive, 0)?;
        }
        self.call::<gaia::TransparencySet>(&level)
    }

    pub fn eq_config(&mut self) -> Result<EqConfig> {
        self.call::<gaia::EqConfigGet>(&())
    }
    pub fn eq_gain(&mut self, band: u8) -> Result<i8> {
        self.call::<gaia::EqGainGet>(&band)
    }
    pub fn set_eq_gain(&mut self, band: u8, gain: i8) -> Result<()> {
        self.call::<gaia::EqGainSet>(&(band, gain))
    }

    pub fn device_info(&mut self) -> Result<DeviceInfo> {
        Ok(DeviceInfo {
            model: self.call::<gaia::ModelGet>(&())?,
            release: self.call::<gaia::ReleaseGet>(&())?,
            hw_revision: self.call::<gaia::HwRevisionGet>(&())?,
            gaia_build: self.call::<gaia::FirmwareVersionsGet>(&())?,
        })
    }

    /// Aggregate `status` snapshot. Issues one request per field; if
    /// any individual call fails the whole call fails.
    pub fn show_state(&mut self) -> Result<ShowState> {
        Ok(ShowState {
            battery: self.battery()?,
            charging: self.charging()?,
            noise: self.anc()?,
            anc_modes: self.anc_modes()?,
            transparency: self.transparency()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;

    /// Replays a fixed script of `(expected_request, response)` pairs.
    struct MockTransport {
        script: std::cell::RefCell<std::collections::VecDeque<(Vec<u8>, Vec<u8>)>>,
    }

    impl MockTransport {
        fn new(script: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) -> Self {
            Self {
                script: std::cell::RefCell::new(script.into_iter().collect()),
            }
        }
    }

    impl Transport for MockTransport {
        fn transact(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
            let (expected, response) = self
                .script
                .borrow_mut()
                .pop_front()
                .expect("unexpected request");
            assert_eq!(packet, expected.as_slice(), "request mismatch");
            Ok(response)
        }
    }

    #[test]
    fn battery_round_trip() {
        let transport = MockTransport::new([(
            vec![0x04, 0x95, 0x06, 0x03], // BatteryGet
            vec![0x04, 0x95, 0x07, 0x03, 80],
        )]);
        let mut headset = Headset::new(transport);
        let percent = headset.battery().unwrap();
        assert_eq!(percent, 80);
    }

    #[test]
    fn set_anc_round_trip() {
        let transport = MockTransport::new([(
            vec![0x04, 0x95, 0x1a, 0x04, 1], // AncSet on
            vec![0x04, 0x95, 0x1b, 0x04],    // empty success response
        )]);
        let mut headset = Headset::new(transport);
        headset.set_anc(Toggle::On).unwrap();
    }

    #[test]
    fn set_transparency_disables_adaptive_first() {
        let transport = MockTransport::new([
            (
                vec![0x04, 0x95, 0x1a, 0x01],
                vec![0x04, 0x95, 0x1b, 0x01, 1, 0, 2, 0, 3, 1],
            ),
            (
                vec![0x04, 0x95, 0x1a, 0x00, 3, 0], // Adaptive off
                vec![0x04, 0x95, 0x1b, 0x00],
            ),
            (
                vec![0x04, 0x95, 0x1a, 0x02, 40],
                vec![0x04, 0x95, 0x1b, 0x02],
            ),
        ]);
        let mut headset = Headset::new(transport);
        headset.set_transparency(40).unwrap();
    }
}
