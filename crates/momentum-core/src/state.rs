//! Typed device-state enums.
//!
//! Every enum here implements:
//!
//! - `TryFrom<u8>` (or `u16`) for parsing the wire byte.
//! - [`core::fmt::Display`] for user-facing rendering.
//! - [`serde::Serialize`] for the JSON daemon protocol.
//!
//! An `Unknown(raw)` variant absorbs values we don't have names for so a
//! firmware update never causes a hard parse error — but the variant still
//! round-trips losslessly through serde and `Display`.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

macro_rules! u8_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $variant:ident = $value:literal => $label:literal ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        $vis enum $name {
            $( $variant, )*
            Unknown(u8),
        }

        impl $name {
            pub const KIND: &'static str = stringify!($name);

            /// Stable kebab-case identifier used in serde output and `Display`.
            pub const fn label(self) -> &'static str {
                match self {
                    $( Self::$variant => $label, )*
                    Self::Unknown(_) => "unknown",
                }
            }
        }

        impl TryFrom<u8> for $name {
            type Error = Error;
            fn try_from(value: u8) -> Result<Self> {
                Ok(match value {
                    $( $value => Self::$variant, )*
                    other => Self::Unknown(other),
                })
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $( Self::$variant => f.write_str($label), )*
                    Self::Unknown(raw) => write!(f, "unknown({raw})"),
                }
            }
        }

        impl Serialize for $name {
            /// Flat string representation: known variants serialize as their
            /// label (e.g. `"adaptive"`), unknown raw bytes as
            /// `"unknown(<u8>)"` — same as [`Display`], guaranteeing the
            /// human-readable text and the daemon JSON agree.
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            /// Inverse of [`Serialize`]: parse the kebab-case label
            /// (e.g. `"adaptive"`) into the corresponding variant.
            /// `"unknown(<u8>)"` round-trips back into
            /// `Self::Unknown(<u8>)`. Any other string is an error.
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> core::result::Result<Self, D::Error> {
                use serde::de::{Error as _, Unexpected};
                let s = <String as Deserialize>::deserialize(deserializer)?;
                match s.as_str() {
                    $( $label => Ok(Self::$variant), )*
                    other => {
                        if let Some(rest) = other.strip_prefix("unknown(").and_then(|r| r.strip_suffix(')')) {
                            rest.parse::<u8>()
                                .map(Self::Unknown)
                                .map_err(|_| D::Error::invalid_value(Unexpected::Str(other), &concat!(stringify!($name), " label")))
                        } else {
                            Err(D::Error::invalid_value(Unexpected::Str(other), &concat!(stringify!($name), " label")))
                        }
                    }
                }
            }
        }
    };
}

u8_enum! {
    pub enum Charging {
        NotCharging = 0 => "not charging",
        Charging    = 1 => "charging",
        Complete    = 2 => "complete",
    }
}

u8_enum! {
    pub enum WearState {
        Unknown_ = 0 => "unknown",
        InCase   = 1 => "in case",
        OffHead  = 2 => "off head",
        OnHead   = 3 => "on head",
    }
}

/// On/off-style boolean reported by the headset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Toggle {
    On,
    Off,
}

impl Toggle {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::On => 1,
            Self::Off => 0,
        }
    }
}

impl From<bool> for Toggle {
    fn from(value: bool) -> Self {
        if value { Self::On } else { Self::Off }
    }
}

impl From<u8> for Toggle {
    fn from(value: u8) -> Self {
        if value == 0 { Self::Off } else { Self::On }
    }
}

impl fmt::Display for Toggle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::On => "on",
            Self::Off => "off",
        })
    }
}

/// Audio EQ preset selected on the headset.
///
/// Schema-confirmed mapping from `audio-mode-1.json` (`AudioMode.Value.map`):
/// `{ USEREQ: 1, SPEECHMODE: 2, FLAT: 3 }`. We label them `Custom / Speech /
/// Flat` to match the official app's user-facing UI strings
/// (`device_preset_title_*` in `assets/app/i18n/en.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Custom,
    Speech,
    Flat,
    Unknown(u16),
}

impl Preset {
    pub const KIND: &'static str = "Preset";

    #[must_use]
    pub const fn as_u16(self) -> u16 {
        match self {
            Self::Custom => 1,
            Self::Speech => 2,
            Self::Flat => 3,
            Self::Unknown(raw) => raw,
        }
    }
}

impl From<u16> for Preset {
    fn from(value: u16) -> Self {
        match value {
            1 => Self::Custom,
            2 => Self::Speech,
            3 => Self::Flat,
            raw => Self::Unknown(raw),
        }
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Custom => f.write_str("custom"),
            Self::Speech => f.write_str("speech"),
            Self::Flat => f.write_str("flat"),
            Self::Unknown(raw) => write!(f, "unknown({raw})"),
        }
    }
}

impl Serialize for Preset {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Preset {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        use serde::de::{Error as _, Unexpected};
        let s = <String as Deserialize>::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "custom" => Self::Custom,
            "speech" => Self::Speech,
            "flat" => Self::Flat,
            other => {
                if let Some(rest) = other
                    .strip_prefix("unknown(")
                    .and_then(|r| r.strip_suffix(')'))
                {
                    rest.parse::<u16>().map(Self::Unknown).map_err(|_| {
                        D::Error::invalid_value(Unexpected::Str(other), &"Preset label")
                    })?
                } else {
                    return Err(D::Error::invalid_value(
                        Unexpected::Str(other),
                        &"Preset label",
                    ));
                }
            }
        })
    }
}

/// ANC sub-modes the M4 reports independently
/// (`ANC_GetNoiseCancelationMode`, schema `m4.json` cmd `0x1A00/0x1A01`).
///
/// Each sub-mode has its own on/off state and they are **not mutually
/// exclusive**: wind and adaptive can both be on (or both off) at once.
///
/// The schema also defines a `Comfort` sub-mode (tag `2`) but on M4 fw
/// 3.37.3 it appears to be a no-op and Smart Control does not expose
/// it, so this crate ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncSubMode {
    Wind,
    Adaptive,
}

impl AncSubMode {
    pub const KIND: &'static str = "AncSubMode";

    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Wind => 1,
            Self::Adaptive => 3,
        }
    }
}

impl TryFrom<u8> for AncSubMode {
    type Error = Error;
    fn try_from(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Wind),
            3 => Ok(Self::Adaptive),
            _ => Err(Error::ParameterRange {
                name: "anc_sub_mode",
                value: i64::from(value),
                min: 1,
                max: 3,
            }),
        }
    }
}

impl fmt::Display for AncSubMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Wind => "wind",
            Self::Adaptive => "adaptive",
        })
    }
}

impl Serialize for AncSubMode {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for AncSubMode {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        use serde::de::{Error as _, Unexpected};
        let s = <String as Deserialize>::deserialize(deserializer)?;
        match s.as_str() {
            "wind" => Ok(Self::Wind),
            "adaptive" => Ok(Self::Adaptive),
            other => Err(D::Error::invalid_value(
                Unexpected::Str(other),
                &"AncSubMode label",
            )),
        }
    }
}

/// State value for the Wind sub-mode. The schema permits an `Auto`
/// value (2) here that the other two sub-modes do not support.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindState {
    Off,
    On,
    Auto,
    Unknown(u8),
}

impl WindState {
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::On => 1,
            Self::Auto => 2,
            Self::Unknown(raw) => raw,
        }
    }
}

impl From<u8> for WindState {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Off,
            1 => Self::On,
            2 => Self::Auto,
            raw => Self::Unknown(raw),
        }
    }
}

impl fmt::Display for WindState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::On => f.write_str("on"),
            Self::Auto => f.write_str("auto"),
            Self::Unknown(raw) => write!(f, "unknown({raw})"),
        }
    }
}

impl Serialize for WindState {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> core::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WindState {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> core::result::Result<Self, D::Error> {
        use serde::de::{Error as _, Unexpected};
        let s = <String as Deserialize>::deserialize(deserializer)?;
        Ok(match s.as_str() {
            "off" => Self::Off,
            "on" => Self::On,
            "auto" => Self::Auto,
            other => {
                if let Some(rest) = other
                    .strip_prefix("unknown(")
                    .and_then(|r| r.strip_suffix(')'))
                {
                    rest.parse::<u8>().map(Self::Unknown).map_err(|_| {
                        D::Error::invalid_value(Unexpected::Str(other), &"WindState label")
                    })?
                } else {
                    return Err(D::Error::invalid_value(
                        Unexpected::Str(other),
                        &"WindState label",
                    ));
                }
            }
        })
    }
}

/// Aggregated ANC sub-mode state. Returned by
/// `ANC_GetNoiseCancelationMode` (cmd `0x1A01`) which delivers a
/// 6-byte payload of `(mode_tag, state)` pairs. We surface only the
/// tags the M4 actually responds to — see [`AncSubMode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AncSubModes {
    pub wind: WindState,
    pub adaptive: bool,
}

impl AncSubModes {
    pub const KIND: &'static str = "AncSubModes";

    /// All-off baseline (the device's ANC sub-modes when nothing is engaged).
    #[must_use]
    pub const fn off() -> Self {
        Self {
            wind: WindState::Off,
            adaptive: false,
        }
    }

    /// Read the state of one sub-mode by tag.
    #[must_use]
    pub fn state_of(&self, sub: AncSubMode) -> u8 {
        match sub {
            AncSubMode::Wind => self.wind.as_u8(),
            AncSubMode::Adaptive => u8::from(self.adaptive),
        }
    }
}

impl fmt::Display for AncSubModes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "wind={} adaptive={}",
            self.wind,
            if self.adaptive { "on" } else { "off" },
        )
    }
}

u8_enum! {
    /// Personalized-sound mode reported by `sound-personalization-1`
    /// `PersonalizedSound_Mode` (cmd `0x2000/0x2001`, notif `0x2081`).
    /// Schema map (verbatim): `not_parameterized=0`, `disabled=1`,
    /// `standby=2`, `enabled=3`, `calibrating=4`.
    pub enum PersonalizationMode {
        NotParameterized = 0 => "not_parameterized",
        Disabled         = 1 => "disabled",
        Standby          = 2 => "standby",
        Enabled          = 3 => "enabled",
        Calibrating      = 4 => "calibrating",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charging_round_trips() {
        let c = Charging::try_from(2).unwrap();
        assert_eq!(c, Charging::Complete);
        assert_eq!(c.to_string(), "complete");
    }

    #[test]
    fn charging_unknown_value_does_not_panic() {
        assert_eq!(Charging::try_from(99).unwrap(), Charging::Unknown(99));
    }

    #[test]
    fn toggle_from_u8() {
        assert_eq!(Toggle::from(0_u8), Toggle::Off);
        assert_eq!(Toggle::from(1_u8), Toggle::On);
        assert_eq!(Toggle::from(255_u8), Toggle::On);
    }

    #[test]
    fn serializes_as_flat_strings() {
        assert_eq!(
            serde_json::to_string(&Charging::NotCharging).unwrap(),
            "\"not charging\""
        );
        assert_eq!(
            serde_json::to_string(&AncSubMode::Adaptive).unwrap(),
            "\"adaptive\""
        );
        assert_eq!(
            serde_json::to_string(&Preset::Custom).unwrap(),
            "\"custom\""
        );

        assert_eq!(
            serde_json::to_string(&Charging::Unknown(99)).unwrap(),
            "\"unknown(99)\""
        );
    }

    #[test]
    fn enums_round_trip_through_serde() {
        let toggle_json = serde_json::to_string(&Toggle::On).unwrap();
        let toggle: Toggle = serde_json::from_str(&toggle_json).unwrap();
        assert_eq!(toggle, Toggle::On);

        let charging_json = serde_json::to_string(&Charging::NotCharging).unwrap();
        let charging: Charging = serde_json::from_str(&charging_json).unwrap();
        assert_eq!(charging, Charging::NotCharging);

        let unknown_json = serde_json::to_string(&Charging::Unknown(99)).unwrap();
        let unknown: Charging = serde_json::from_str(&unknown_json).unwrap();
        assert_eq!(unknown, Charging::Unknown(99));

        let preset_json = serde_json::to_string(&Preset::Speech).unwrap();
        let preset: Preset = serde_json::from_str(&preset_json).unwrap();
        assert_eq!(preset, Preset::Speech);

        let wind_json = serde_json::to_string(&WindState::Auto).unwrap();
        let wind: WindState = serde_json::from_str(&wind_json).unwrap();
        assert_eq!(wind, WindState::Auto);

        let wind_unknown_json = serde_json::to_string(&WindState::Unknown(7)).unwrap();
        let wind_unknown: WindState = serde_json::from_str(&wind_unknown_json).unwrap();
        assert_eq!(wind_unknown, WindState::Unknown(7));

        let sub_json = serde_json::to_string(&AncSubMode::Adaptive).unwrap();
        let sub: AncSubMode = serde_json::from_str(&sub_json).unwrap();
        assert_eq!(sub, AncSubMode::Adaptive);

        let modes = AncSubModes {
            wind: WindState::On,
            adaptive: false,
        };
        let modes_json = serde_json::to_string(&modes).unwrap();
        let modes_back: AncSubModes = serde_json::from_str(&modes_json).unwrap();
        assert_eq!(modes_back, modes);
    }
}
