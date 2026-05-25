//! Platform-specific [`momentum_core::Transport`] implementations.

mod error;

#[cfg(target_os = "linux")]
mod bdaddr;
#[cfg(target_os = "linux")]
mod linux;

use std::time::Duration;

pub use error::{Error, Result};
pub use momentum_core::Transport;

#[cfg(target_os = "linux")]
pub use bdaddr::BdAddr;
#[cfg(target_os = "linux")]
pub use linux::SppTransport;

/// Open the platform's default transport for `address` on RFCOMM `channel`.
///
/// On Linux this returns a [`SppTransport`] boxed as `dyn Transport`. On
/// every other OS it returns [`Error::Unsupported`].
#[cfg(target_os = "linux")]
pub fn open_default(address: &str, channel: u8, timeout: Duration) -> Result<Box<dyn Transport>> {
    let addr: BdAddr = address.parse()?;
    Ok(Box::new(SppTransport::connect(addr, channel, timeout)?))
}

#[cfg(not(target_os = "linux"))]
pub fn open_default(
    _address: &str,
    _channel: u8,
    _timeout: Duration,
) -> Result<Box<dyn Transport>> {
    Err(Error::Unsupported)
}
