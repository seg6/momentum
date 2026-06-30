//! Platform-specific [`momentum_core::Transport`] implementations.

mod error;
mod spp_transport;

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
mod bdaddr;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use std::time::Duration;

pub use error::{Error, Result};
pub use momentum_core::Transport;

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub use bdaddr::BdAddr;
#[cfg(target_os = "linux")]
pub use linux::SppTransport;
#[cfg(target_os = "macos")]
pub use macos::{SppTransport, connected_devices_matching};
#[cfg(target_os = "windows")]
pub use windows::SppTransport;

/// Open the platform's default transport for `address` on RFCOMM `channel`.
///
/// On supported platforms this returns a [`SppTransport`] boxed as
/// `dyn Transport`. On every other OS it returns [`Error::Unsupported`].
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub fn open_default(address: &str, channel: u8, timeout: Duration) -> Result<Box<dyn Transport>> {
    let addr: BdAddr = address.parse()?;
    Ok(Box::new(SppTransport::connect(addr, channel, timeout)?))
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
pub fn open_default(
    _address: &str,
    _channel: u8,
    _timeout: Duration,
) -> Result<Box<dyn Transport>> {
    Err(Error::Unsupported)
}
