//! Shared blocking SPP transport session.
//!
//! Platform modules provide only a byte-oriented RFCOMM stream via [`SppIo`].
//! This module owns all Sennheiser SPP framing, response matching, reader
//! threading, and notification backpressure.

#![cfg_attr(
    not(any(target_os = "linux", target_os = "windows", target_os = "macos")),
    allow(dead_code)
)]

use std::io;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use momentum_core::{NotificationFrame, Transport, spp};
use tracing::{debug, trace, warn};

use crate::{Error, Result};

const READ_BUFFER_SIZE: usize = 1024;
const NOTIFICATION_BACKLOG: usize = 256;

/// Bit in a GAIA opcode that flags a host-bound packet as a response to a
/// previously-issued command rather than an unsolicited notification.
const RESPONSE_BIT: u16 = 0x0100;

/// Bit in an error response on top of [`RESPONSE_BIT`].
const ERROR_BIT: u16 = 0x0080;

/// Platform-specific RFCOMM byte stream used by the shared SPP session.
pub(crate) trait SppIo: Send + Sync + 'static {
    fn read_some(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn write_all(&self, data: &[u8]) -> io::Result<()>;
    fn shutdown(&self);
}

struct InFlight {
    expected_vendor: u16,
    expected_response: u16,
    expected_error: u16,
    tx: Sender<Vec<u8>>,
}

/// Generic RFCOMM/SPP transport. OS-specific modules wrap this type to expose
/// their platform-shaped `SppTransport::connect` constructor.
pub(crate) struct SppTransport<I: SppIo> {
    io: Arc<I>,
    write_buffer: Vec<u8>,
    timeout: Duration,
    in_flight: Arc<Mutex<Option<InFlight>>>,
    notifications: Receiver<NotificationFrame>,
    reader: Option<JoinHandle<()>>,
}

impl<I: SppIo> core::fmt::Debug for SppTransport<I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SppTransport").finish_non_exhaustive()
    }
}

impl<I: SppIo> SppTransport<I> {
    pub(crate) fn new(io: I, timeout: Duration) -> Result<Self> {
        let io = Arc::new(io);
        let in_flight: Arc<Mutex<Option<InFlight>>> = Arc::new(Mutex::new(None));
        let (notif_tx, notif_rx) = bounded::<NotificationFrame>(NOTIFICATION_BACKLOG);

        let reader = {
            let io = Arc::clone(&io);
            let in_flight = Arc::clone(&in_flight);
            let notif_rx_for_reader = notif_rx.clone();
            thread::Builder::new()
                .name("momentum-spp-reader".into())
                .spawn(move || reader_loop(io, in_flight, notif_tx, notif_rx_for_reader))
                .map_err(|e| Error::from(io::Error::other(e.to_string())))?
        };

        Ok(Self {
            io,
            write_buffer: Vec::with_capacity(READ_BUFFER_SIZE),
            timeout,
            in_flight,
            notifications: notif_rx,
            reader: Some(reader),
        })
    }
}

impl<I: SppIo> Transport for SppTransport<I> {
    fn transact(&mut self, packet: &[u8]) -> momentum_core::Result<Vec<u8>> {
        if packet.len() < 4 {
            return Err(momentum_core::Error::Transport(Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "GAIA packet shorter than 4 bytes",
            ))));
        }

        let expected_vendor = u16::from_be_bytes([packet[0], packet[1]]);
        let command_opcode = u16::from_be_bytes([packet[2], packet[3]]);
        let expected_response = command_opcode | RESPONSE_BIT;
        let expected_error = expected_response | ERROR_BIT;

        let (tx, rx) = bounded::<Vec<u8>>(1);
        {
            let mut guard = self
                .in_flight
                .lock()
                .map_err(|_| momentum_core::Error::Transport(Box::new(Error::Closed)))?;
            *guard = Some(InFlight {
                expected_vendor,
                expected_response,
                expected_error,
                tx,
            });
        }

        self.write_buffer.clear();
        spp::write_frame(packet, &mut self.write_buffer)?;
        trace!(bytes = ?packet, "send GAIA");
        if let Err(e) = self.io.write_all(&self.write_buffer) {
            let _ = self.in_flight.lock().map(|mut g| g.take());
            return Err(momentum_core::Error::Transport(Box::new(e)));
        }

        match rx.recv_timeout(self.timeout) {
            Ok(payload) => {
                trace!(bytes = ?payload, "recv GAIA");
                Ok(payload)
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let _ = self.in_flight.lock().map(|mut g| g.take());
                Err(momentum_core::Error::Transport(Box::new(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SPP response timeout",
                ))))
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                Err(momentum_core::Error::Transport(Box::new(Error::Closed)))
            }
        }
    }

    fn subscribe(&self) -> Option<Receiver<NotificationFrame>> {
        Some(self.notifications.clone())
    }
}

impl<I: SppIo> Drop for SppTransport<I> {
    fn drop(&mut self) {
        self.io.shutdown();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

fn reader_loop<I: SppIo>(
    io: Arc<I>,
    in_flight: Arc<Mutex<Option<InFlight>>>,
    notifications: Sender<NotificationFrame>,
    notifications_drain: Receiver<NotificationFrame>,
) {
    let mut read_buffer: Vec<u8> = Vec::with_capacity(READ_BUFFER_SIZE);

    loop {
        loop {
            match spp::extract_frame(&mut read_buffer) {
                Ok(Some(payload)) => {
                    dispatch_frame(payload, &in_flight, &notifications, &notifications_drain)
                }
                Ok(None) => break,
                Err(e) => {
                    warn!(error = %e, "SPP framing error; resyncing");
                    read_buffer.clear();
                    break;
                }
            }
        }

        let mut buf = [0u8; READ_BUFFER_SIZE];
        match io.read_some(&mut buf) {
            Ok(0) => {
                debug!("SPP reader: EOF");
                break;
            }
            Ok(n) => read_buffer.extend_from_slice(&buf[..n]),
            Err(e) => {
                debug!(error = %e, "SPP reader stopped");
                break;
            }
        }
    }

    // Notify any pending transact() by dropping the in-flight sender.
    if let Ok(mut guard) = in_flight.lock() {
        guard.take();
    }
}

fn dispatch_frame(
    payload: Vec<u8>,
    in_flight: &Mutex<Option<InFlight>>,
    notifications: &Sender<NotificationFrame>,
    notifications_drain: &Receiver<NotificationFrame>,
) {
    if payload.len() < 4 {
        warn!(len = payload.len(), "SPP reader: short GAIA packet");
        return;
    }
    let vendor = u16::from_be_bytes([payload[0], payload[1]]);
    let opcode = u16::from_be_bytes([payload[2], payload[3]]);

    let matched = {
        let mut guard = match in_flight.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let take = guard.as_ref().is_some_and(|slot| {
            vendor == slot.expected_vendor
                && (opcode == slot.expected_response || opcode == slot.expected_error)
        });
        if take { guard.take() } else { None }
    };

    if let Some(slot) = matched {
        let _ = slot.tx.send(payload);
        return;
    }

    if (opcode & RESPONSE_BIT) != 0 {
        trace!(
            vendor = format_args!("{:#06x}", vendor),
            opcode = format_args!("{:#06x}", opcode),
            "response-bit frame did not match in-flight slot; routing as notification"
        );
    }
    let frame = NotificationFrame {
        vendor,
        opcode,
        payload: payload[4..].to_vec(),
    };
    if let Err(TrySendError::Full(frame)) = notifications.try_send(frame) {
        let _ = notifications_drain.try_recv();
        if notifications.try_send(frame).is_err() {
            trace!("notification channel full and drain raced; frame dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_matches_in_flight_by_vendor_and_opcode() {
        let (response_tx, response_rx) = bounded::<Vec<u8>>(1);
        let in_flight = Mutex::new(Some(InFlight {
            expected_vendor: momentum_core::VENDOR_QUALCOMM,
            expected_response: 0x0107,
            expected_error: 0x0187,
            tx: response_tx,
        }));
        let (notification_tx, notification_rx) = bounded::<NotificationFrame>(4);

        dispatch_frame(
            vec![0x04, 0x95, 0x01, 0x07],
            &in_flight,
            &notification_tx,
            &notification_rx,
        );

        assert!(response_rx.try_recv().is_err());
        let notification = notification_rx.try_recv().unwrap();
        assert_eq!(notification.vendor, momentum_core::VENDOR_SENNHEISER);
        assert_eq!(notification.opcode, 0x0107);
        assert!(in_flight.lock().unwrap().is_some());
    }
}
