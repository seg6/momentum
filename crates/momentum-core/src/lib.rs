//! Protocol types and device model for the Sennheiser MOMENTUM 4.
//!
//! This crate is OS-agnostic. It defines:
//!
//! - [`Error`] / [`Result`] for protocol failures.
//! - The wire format under [`spp`] and [`gaia`].
//! - Strongly typed state enums under [`state`].
//! - A [`Headset`] facade that turns a [`Transport`] into typed methods
//!   like [`Headset::battery`] or [`Headset::set_anc`].
//!
//! The [`Transport`] trait is the seam where platform crates plug in.

mod error;
pub mod gaia;
mod headset;
pub mod notification;
pub mod spp;
pub mod state;
mod transport;

pub use error::{Error, Result};
pub use headset::{DeviceInfo, Headset, ShowState};
pub use notification::{Notification, NotificationKey};
pub use transport::{NotificationFrame, Transport};

/// Sennheiser GAIA vendor identifier.
pub const VENDOR_SENNHEISER: u16 = 0x0495;
pub const VENDOR_QUALCOMM: u16 = 0x001D;
