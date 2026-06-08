//! Linux RFCOMM/SPP transport.
//!
//! Connects to RFCOMM channel 1 on a classic-Bluetooth headset, performs SPP
//! framing of GAIA packets, and exposes the result as a
//! [`momentum_core::Transport`].
//!
//! ## libc, not [`bluer`]
//!
//! `BlueZ` has no stable Rust binding that covers RFCOMM cleanly without
//! pulling in tokio. We instead open `socket(AF_BLUETOOTH, SOCK_STREAM,
//! BTPROTO_RFCOMM)` directly. `BTPROTO_RFCOMM` is **3** per
//! `<bluetooth/bluetooth.h>` — the `libc` crate does not export it on every
//! distro, so we declare it locally.
//!
//! ## File lock
//!
//! Two processes talking to the same headset over RFCOMM is a quick way to
//! desync framing. We hold an exclusive `flock` on
//! `$XDG_RUNTIME_DIR/momentum-<addr>.lock` for the lifetime of a transport.
//!
//! ## Threading model
//!
//! The transport spawns a **background reader thread** on connect that owns
//! a clone of the socket fd. It blocks on `read(2)` and demultiplexes every
//! complete SPP frame by inspecting bit `0x0100` of the GAIA opcode:
//!
//! * **response / error** (response bit set) — forwarded over a single-slot
//!   crossbeam channel to whichever `transact` call is currently waiting.
//! * **notification** (response bit clear) — forwarded over a bounded
//!   crossbeam channel as a [`NotificationFrame`]. The channel uses a
//!   drop-oldest policy (see [`NOTIFICATION_BACKLOG`]) so a missing or
//!   lagging subscriber cannot grow memory unboundedly. Subscribers
//!   obtain a `Receiver` via [`Transport::subscribe`].
//!
//! `transact` is fully blocking from the caller's perspective and enforces
//! the per-operation timeout via `Receiver::recv_timeout`. The kernel
//! `SO_RCVTIMEO/SO_SNDTIMEO` knobs are no longer used; the writer side
//! still uses `SO_SNDTIMEO` to bound `write(2)` against a stalled link.

use std::env;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use libc::{
    AF_BLUETOOTH, EAGAIN, EBADF, EBUSY, EINTR, EWOULDBLOCK, LOCK_EX, LOCK_NB, SHUT_RDWR,
    SO_SNDTIMEO, SOCK_STREAM, SOL_SOCKET, c_void, connect, flock as libc_flock, read, setsockopt,
    shutdown, sockaddr, socket, timeval, write,
};
use momentum_core::{NotificationFrame, Transport, spp};
use tracing::{debug, trace, warn};

use crate::{BdAddr, Error, Result};

/// `BTPROTO_RFCOMM` from `<bluetooth/bluetooth.h>`. Not exported by every
/// `libc` build, so we define it locally.
const BTPROTO_RFCOMM: i32 = 3;

const READ_BUFFER_SIZE: usize = 1024;
const CONNECT_MAX_ATTEMPTS: u32 = 8;
const CONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// Bound on the notification channel.
///
/// Frames accumulate here until a [`Transport::subscribe`] consumer
/// drains them. Without an active subscriber (e.g. during a one-shot
/// `momentum battery`), or with a subscriber that has fallen behind, we
/// drop the **oldest** queued frame to make room for the new one — see
/// [`reader_loop`]. The bound is sized to absorb a burst of correlated
/// pushes (e.g. the post-`subscribe_all` connect snapshot) without
/// trimming any actual events from a healthy subscriber.
const NOTIFICATION_BACKLOG: usize = 256;

/// Bit in a GAIA opcode that flags a host-bound packet as a response to a
/// previously-issued command (rather than an unsolicited notification).
const RESPONSE_BIT: u16 = 0x0100;

/// Bit in an *error* response on top of `RESPONSE_BIT`: a packet is an
/// error response when both bits are set (per Sennheiser GAIA convention,
/// `error = response | 0x0080`).
const ERROR_BIT: u16 = 0x0080;

/// A pending request slot. The reader thread routes an inbound frame
/// into the bounded sender only if its opcode matches the expected
/// response opcode (`command | 0x0100`) or the corresponding error
/// opcode (`command | 0x0180`). Any other frame with the response bit
/// set is treated as an unrequested notification (e.g. the device's
/// post-register state snapshot), so it goes to the notification
/// channel where decoders can pick it up.
struct InFlight {
    expected_vendor: u16,
    expected_response: u16,
    expected_error: u16,
    tx: Sender<Vec<u8>>,
}

/// `struct sockaddr_rc` from `<bluetooth/rfcomm.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrRc {
    family: libc::sa_family_t,
    bdaddr: [u8; 6],
    channel: u8,
}

/// RFCOMM transport with SPP framing.
pub struct SppTransport {
    /// Shared with the reader thread.
    fd: Arc<OwnedFd>,
    /// Held for its `Drop` — releases the `flock` on close.
    _lock: File,
    write_buffer: Vec<u8>,
    /// `recv_timeout` budget for [`Transport::transact`].
    timeout: Duration,
    /// Single-in-flight slot. `transact` installs an [`InFlight`] here
    /// before writing; the reader thread takes it out and pushes the
    /// frame whose opcode matches.
    in_flight: Arc<Mutex<Option<InFlight>>>,
    /// Persistent receiver handed to subscribers via [`Transport::subscribe`].
    notifications: Receiver<NotificationFrame>,
    /// Joined in `Drop`. Optional only so we can `take()` it.
    reader: Option<JoinHandle<()>>,
}

impl core::fmt::Debug for SppTransport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SppTransport")
            .field("fd", &self.fd.as_raw_fd())
            .finish_non_exhaustive()
    }
}

impl SppTransport {
    /// Connect to `addr` on RFCOMM `channel`, applying `timeout` to writes
    /// and to each `transact` round-trip.
    pub fn connect(addr: BdAddr, channel: u8, timeout: Duration) -> Result<Self> {
        let lock = acquire_lock(addr)?;
        let sock_addr = SockAddrRc {
            family: AF_BLUETOOTH as libc::sa_family_t,
            bdaddr: addr.raw(),
            channel,
        };

        let mut last_error: Option<io::Error> = None;
        let fd = (|| -> Option<OwnedFd> {
            for attempt in 0..CONNECT_MAX_ATTEMPTS {
                debug!(attempt, %addr, channel, "connecting RFCOMM");
                match connect_socket(&sock_addr, timeout) {
                    Ok(fd) => return Some(fd),
                    Err(error) => {
                        let retry = error.raw_os_error() == Some(EBUSY);
                        last_error = Some(error);
                        if !retry || attempt + 1 == CONNECT_MAX_ATTEMPTS {
                            return None;
                        }
                        thread::sleep(CONNECT_BACKOFF);
                    }
                }
            }
            None
        })()
        .ok_or_else(|| {
            Error::from(last_error.unwrap_or_else(|| io::Error::other("SPP connection failed")))
        })?;

        let fd = Arc::new(fd);
        let in_flight: Arc<Mutex<Option<InFlight>>> = Arc::new(Mutex::new(None));
        let (notif_tx, notif_rx) = bounded::<NotificationFrame>(NOTIFICATION_BACKLOG);

        let reader = {
            let fd = Arc::clone(&fd);
            let in_flight = Arc::clone(&in_flight);
            let notif_rx_for_reader = notif_rx.clone();
            thread::Builder::new()
                .name("momentum-spp-reader".into())
                .spawn(move || reader_loop(fd, in_flight, notif_tx, notif_rx_for_reader))
                .map_err(|e| Error::from(io::Error::other(e.to_string())))?
        };

        Ok(Self {
            fd,
            _lock: lock,
            write_buffer: Vec::with_capacity(READ_BUFFER_SIZE),
            timeout,
            in_flight,
            notifications: notif_rx,
            reader: Some(reader),
        })
    }
}

impl Transport for SppTransport {
    fn transact(&mut self, packet: &[u8]) -> momentum_core::Result<Vec<u8>> {
        // The expected response opcode is derived from the outbound
        // command opcode (`bytes 2..4`) by setting the response bit.
        // `expected_error` carries the same opcode plus the error bit.
        // Together they form the only two opcodes the reader will route
        // into our in-flight slot; anything else with the response bit
        // set is treated as an unrequested notification.
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
        if let Err(e) = write_all(self.fd.as_raw_fd(), &self.write_buffer) {
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

impl Drop for SppTransport {
    fn drop(&mut self) {
        // Politely tear down the connection so the reader thread observes
        // EOF on its next `read(2)`.
        // SAFETY: `self.fd` owns a valid fd until this `Drop` returns.
        unsafe {
            shutdown(self.fd.as_raw_fd(), SHUT_RDWR);
        }
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

fn reader_loop(
    fd: Arc<OwnedFd>,
    in_flight: Arc<Mutex<Option<InFlight>>>,
    notifications: Sender<NotificationFrame>,
    notifications_drain: Receiver<NotificationFrame>,
) {
    let raw = fd.as_raw_fd();
    let mut read_buffer: Vec<u8> = Vec::with_capacity(READ_BUFFER_SIZE);

    loop {
        loop {
            match spp::extract_frame(&mut read_buffer) {
                Ok(Some(payload)) => {
                    dispatch_frame(payload, &in_flight, &notifications, &notifications_drain)
                }
                Ok(None) => break,
                Err(e) => {
                    // Framing error: drop the buffer and resync. The headset
                    // never sends torn streams in practice, but a bus glitch
                    // could orphan bytes; better to lose them than spin.
                    warn!(error = %e, "SPP framing error; resyncing");
                    read_buffer.clear();
                    break;
                }
            }
        }

        let mut buf = [0u8; READ_BUFFER_SIZE];
        match read_some(raw, &mut buf) {
            Ok(0) => {
                debug!("SPP reader: EOF");
                break;
            }
            Ok(n) => read_buffer.extend_from_slice(&buf[..n]),
            Err(e) => {
                // EBADF after Drop's shutdown is the normal exit path.
                if e.raw_os_error() == Some(EBADF) {
                    debug!("SPP reader: fd closed");
                } else {
                    warn!(error = %e, "SPP reader: read failed");
                }
                break;
            }
        }
    }

    // Notify any pending transact() by dropping the in_flight sender.
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

    // Only consume the in-flight slot if the inbound opcode actually
    // matches the response (or error) the slot is waiting for. This
    // distinguishes "RSP to my last command" from "device-pushed state
    // snapshot that happens to ride the same response-bit channel".
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
    // Drop-oldest backpressure: if no one is listening (one-shot CLI
    // command) or the subscriber has fallen behind, the bounded
    // channel will eventually fill. Discard the oldest queued frame
    // so memory stays capped at NOTIFICATION_BACKLOG and the freshest
    // events still get through.
    if let Err(TrySendError::Full(frame)) = notifications.try_send(frame) {
        let _ = notifications_drain.try_recv();
        if notifications.try_send(frame).is_err() {
            trace!("notification channel full and drain raced; frame dropped");
        }
    }
}

fn acquire_lock(addr: BdAddr) -> Result<File> {
    let path = lockfile_path(addr);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(&path)?;
    // SAFETY: `file` owns a valid fd; `LOCK_EX` is a defined constant.
    let result = unsafe { libc_flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if matches!(error.raw_os_error(), Some(code) if code == EWOULDBLOCK || code == EAGAIN) {
            return Err(Error::Busy(addr.to_string()));
        }
        return Err(error.into());
    }
    Ok(file)
}

fn lockfile_path(addr: BdAddr) -> PathBuf {
    let dir = env::var_os("XDG_RUNTIME_DIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let name = addr.to_string().replace(':', "_");
    dir.join(format!("momentum-{name}.lock"))
}

fn connect_socket(addr: &SockAddrRc, timeout: Duration) -> io::Result<OwnedFd> {
    // SAFETY: `socket(2)` accepts these family/type/protocol constants; the
    // returned fd is checked for -1 before we take ownership.
    let raw = unsafe { socket(AF_BLUETOOTH, SOCK_STREAM, BTPROTO_RFCOMM) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a freshly-returned valid socket fd.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    // We rely on the in-process `recv_timeout` rather than `SO_RCVTIMEO`
    // because the reader thread reads continuously; the per-`transact`
    // budget belongs at the channel layer.
    set_timeout(fd.as_raw_fd(), SO_SNDTIMEO, timeout)?;

    // SAFETY: `addr` lives until the end of this call; `size_of::<SockAddrRc>`
    // is the layout the kernel expects (verified at compile time by `#[repr(C)]`).
    let result = unsafe {
        connect(
            fd.as_raw_fd(),
            core::ptr::from_ref(addr).cast::<sockaddr>(),
            size_of::<SockAddrRc>() as libc::socklen_t,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn set_timeout(fd: i32, opt: i32, timeout: Duration) -> io::Result<()> {
    let tv = timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: `tv` is a stack value of the correct type and length.
    let rc = unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            opt,
            core::ptr::from_ref(&tv).cast::<c_void>(),
            size_of::<timeval>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn write_all(fd: i32, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        // SAFETY: `data` is a borrowed slice valid for the duration of the call.
        let written = unsafe { write(fd, data.as_ptr().cast::<c_void>(), data.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(EINTR) {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write SPP frame",
            ));
        }
        data = &data[written as usize..];
    }
    Ok(())
}

fn read_some(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: `buf` is a mutable borrowed slice valid for the call.
        let len = unsafe { read(fd, buf.as_mut_ptr().cast::<c_void>(), buf.len()) };
        if len >= 0 {
            return Ok(len as usize);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(EINTR) {
            continue;
        }
        return Err(error);
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
