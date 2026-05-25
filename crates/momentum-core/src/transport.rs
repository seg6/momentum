//! Transport seam: anything that can exchange a GAIA packet with the headset.
//!
//! Implementations live in `momentum-transport`.
//! The trait deals in raw GAIA packets, not framed SPP bytes — framing is
//! the transport's responsibility because BLE GATT would frame differently.

use crate::Result;
use crossbeam_channel::Receiver;

/// A raw GAIA notification frame as it came off the wire.
///
/// `opcode` is `u16::from_be_bytes(packet[2..4])` of the GAIA packet — i.e.
/// the unmodified (command | error_bit) value. `payload` is everything after
/// the 4-byte vendor+opcode header.
#[derive(Debug, Clone)]
pub struct NotificationFrame {
    pub vendor: u16,
    pub opcode: u16,
    pub payload: Vec<u8>,
}

pub trait Transport: Send {
    /// Send a GAIA packet and return the response payload (post-validation,
    /// i.e. the bytes after the 4-byte vendor + response header).
    ///
    /// Implementations are expected to:
    ///
    /// 1. Apply transport framing (SPP for RFCOMM).
    /// 2. Read until a full response packet has been received.
    /// 3. Return the response packet *unvalidated* — [`crate::Headset`]
    ///    validates against the command's expected response ID.
    fn transact(&mut self, packet: &[u8]) -> Result<Vec<u8>>;

    /// Subscribe to incoming notification frames pushed by the device
    /// (i.e. inbound GAIA packets whose opcode is not a response to an
    /// in-flight command).
    ///
    /// The default implementation returns `None`, meaning the transport
    /// does not surface notifications (e.g. mock transports used in tests).
    /// A real SPP transport returns `Some(receiver)`; the same receiver is
    /// returned on every call so callers may safely clone the underlying
    /// channel via `crossbeam_channel::Receiver::clone`.
    fn subscribe(&self) -> Option<Receiver<NotificationFrame>> {
        None
    }
}

impl<T: Transport + ?Sized> Transport for Box<T> {
    fn transact(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        (**self).transact(packet)
    }
    fn subscribe(&self) -> Option<Receiver<NotificationFrame>> {
        (**self).subscribe()
    }
}

impl<T: Transport + ?Sized> Transport for &mut T {
    fn transact(&mut self, packet: &[u8]) -> Result<Vec<u8>> {
        (**self).transact(packet)
    }
    fn subscribe(&self) -> Option<Receiver<NotificationFrame>> {
        (**self).subscribe()
    }
}
