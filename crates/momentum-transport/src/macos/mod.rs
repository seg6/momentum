//! macOS RFCOMM/SPP transport built on IOBluetooth via `objc2`.
//!
//! IOBluetooth is callback-driven and delivers its RFCOMM delegate callbacks
//! (open-complete, inbound data, close) to the **main thread's** run loop. The
//! binary keeps that loop pumped (see the `macos::runloop` module in the
//! `momentum` crate), so here we only need to own the Objective-C objects and
//! drive writes.
//!
//! An IOBluetooth channel is not `Send`, so a dedicated IO thread owns the
//! device and channel for their whole lifetime and serializes writes/close from
//! a command queue. Inbound bytes arrive on the main thread via the delegate and
//! land in a shared buffer the SPP reader drains.

mod delegate;

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use libc::{EAGAIN, EWOULDBLOCK, LOCK_EX, LOCK_NB, flock as libc_flock};
use momentum_core::{NotificationFrame, Transport};
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::NSString;
use objc2_io_bluetooth::{IOBluetoothDevice, IOBluetoothRFCOMMChannel};
use tracing::debug;

use self::delegate::Delegate;
use crate::spp_transport::{SppIo, SppTransport as SharedSppTransport};
use crate::{BdAddr, Error, Result};

// --- shared state between the IO thread, delegate, and SPP reader -----------

pub(crate) struct Shared {
    pub(crate) state: Mutex<BufState>,
    pub(crate) cv: Condvar,
}

pub(crate) struct BufState {
    pub(crate) buf: VecDeque<u8>,
    pub(crate) closed: bool,
    /// `None` until the async open completes; then the `IOReturn` status.
    pub(crate) open_status: Option<i32>,
}

enum Command {
    Write(Vec<u8>, Sender<io::Result<()>>),
    Shutdown,
}

// --- IO thread --------------------------------------------------------------

/// Owns the IOBluetooth objects and serializes writes/close until shutdown.
/// Reports whether the open was *initiated* via `setup_tx`; the open *result*
/// arrives asynchronously through the delegate into `shared`.
fn io_thread_main(
    address: String,
    channel_id: u8,
    shared: Arc<Shared>,
    commands: Receiver<Command>,
    setup_tx: Sender<std::result::Result<(), String>>,
) {
    let ns_address = NSString::from_str(&address);
    debug!(%address, "resolving IOBluetooth device");
    // SAFETY: passing a valid NSString; returns nil for unknown devices.
    let device = match unsafe { IOBluetoothDevice::deviceWithAddressString(Some(&ns_address)) } {
        Some(device) => device,
        None => {
            let _ = setup_tx.send(Err(format!("no IOBluetooth device for {address}")));
            return;
        }
    };

    let delegate = Delegate::new(Arc::clone(&shared));
    let delegate_obj: &AnyObject = &delegate;
    let mut channel: Option<Retained<IOBluetoothRFCOMMChannel>> = None;
    debug!(channel_id, "opening RFCOMM channel (async)");
    // SAFETY: `channel` is a valid out-slot; the delegate outlives the channel
    // because it stays in scope on this thread for the channel's whole lifetime.
    let rc = unsafe {
        device.openRFCOMMChannelAsync_withChannelID_delegate(
            Some(&mut channel),
            channel_id,
            Some(delegate_obj),
        )
    };
    if rc != 0 {
        let _ = setup_tx.send(Err(format!(
            "openRFCOMMChannelAsync(channel {channel_id}) failed: 0x{rc:08x}"
        )));
        return;
    }
    let channel = match channel {
        Some(channel) => channel,
        None => {
            let _ = setup_tx.send(Err("IOBluetooth returned a null RFCOMM channel".into()));
            return;
        }
    };
    if setup_tx.send(Ok(())).is_err() {
        return; // connect() gave up.
    }

    // Serialize writes/close here; the open-complete and data callbacks run on
    // the main run loop and update `shared` directly.
    while let Ok(command) = commands.recv() {
        match command {
            Command::Write(data, reply) => {
                let _ = reply.send(write_all_chunked(&channel, &data));
            }
            Command::Shutdown => break,
        }
    }

    // SAFETY: closing a live channel; safe to call once.
    unsafe { channel.closeChannel() };
    let mut state = shared.state.lock().unwrap();
    state.closed = true;
    drop(state);
    shared.cv.notify_all();
    // `channel`, `device`, and `delegate` are released here, on the IO thread.
}

fn write_all_chunked(channel: &IOBluetoothRFCOMMChannel, data: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < data.len() {
        let chunk = (data.len() - offset).min(u16::MAX as usize);
        // SAFETY: `writeSync` reads `chunk` bytes from the pointer; it does not
        // retain or mutate them. The cast to `*mut` matches the ObjC prototype.
        let rc = unsafe {
            channel.writeSync_length(data[offset..].as_ptr() as *mut c_void, chunk as u16)
        };
        if rc != 0 {
            return Err(io::Error::other(format!("RFCOMM writeSync failed: 0x{rc:08x}")));
        }
        offset += chunk;
    }
    Ok(())
}

// --- SppIo adapter ----------------------------------------------------------

struct MacosRfcomm {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    io_thread: Mutex<Option<JoinHandle<()>>>,
    /// Held for its `Drop`, which releases the per-device `flock`.
    _lock: File,
}

impl SppIo for MacosRfcomm {
    fn read_some(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if !state.buf.is_empty() {
                let n = state.buf.len().min(buf.len());
                for (slot, byte) in buf.iter_mut().zip(state.buf.drain(..n)) {
                    *slot = byte;
                }
                return Ok(n);
            }
            if state.closed {
                return Ok(0);
            }
            state = self.shared.cv.wait(state).unwrap();
        }
    }

    fn write_all(&self, data: &[u8]) -> io::Result<()> {
        let (reply_tx, reply_rx) = bounded::<io::Result<()>>(1);
        self.commands
            .send(Command::Write(data.to_vec(), reply_tx))
            .map_err(|_| io::Error::other("RFCOMM IO thread is gone"))?;
        reply_rx
            .recv()
            .map_err(|_| io::Error::other("RFCOMM IO thread dropped the write reply"))?
    }

    fn shutdown(&self) {
        let _ = self.commands.send(Command::Shutdown);
        let mut state = self.shared.state.lock().unwrap();
        state.closed = true;
        drop(state);
        self.shared.cv.notify_all();
    }
}

impl Drop for MacosRfcomm {
    fn drop(&mut self) {
        if let Some(handle) = self.io_thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

// --- public transport -------------------------------------------------------

/// RFCOMM transport with Sennheiser SPP framing.
pub struct SppTransport {
    inner: SharedSppTransport<MacosRfcomm>,
}

impl core::fmt::Debug for SppTransport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SppTransport").finish_non_exhaustive()
    }
}

impl SppTransport {
    /// Connect to `addr` on RFCOMM `channel`, applying `timeout` to the open
    /// handshake and to each subsequent `transact` round-trip.
    pub fn connect(addr: BdAddr, channel: u8, timeout: Duration) -> Result<Self> {
        let lock = acquire_lock(addr)?;
        let shared = Arc::new(Shared {
            state: Mutex::new(BufState {
                buf: VecDeque::new(),
                closed: false,
                open_status: None,
            }),
            cv: Condvar::new(),
        });
        let (cmd_tx, cmd_rx) = unbounded::<Command>();
        let (setup_tx, setup_rx) = bounded::<std::result::Result<(), String>>(1);

        let address = addr.to_string();
        let shared_io = Arc::clone(&shared);
        debug!(%addr, channel, "opening RFCOMM via IOBluetooth");
        let io_thread = thread::Builder::new()
            .name("momentum-iobt".into())
            .spawn(move || io_thread_main(address, channel, shared_io, cmd_rx, setup_tx))
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;

        let stop = |io_thread: JoinHandle<()>, cmd_tx: &Sender<Command>| {
            let _ = cmd_tx.send(Command::Shutdown);
            let _ = io_thread.join();
        };

        match setup_rx.recv_timeout(timeout) {
            Ok(Ok(())) => {}
            Ok(Err(message)) => {
                let _ = io_thread.join();
                return Err(Error::Io(io::Error::other(message)));
            }
            Err(_) => {
                stop(io_thread, &cmd_tx);
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out resolving the IOBluetooth device",
                )));
            }
        }

        match wait_for_open(&shared, timeout) {
            Some(0) => {}
            Some(status) => {
                stop(io_thread, &cmd_tx);
                return Err(Error::Io(io::Error::other(format!(
                    "RFCOMM open(channel {channel}) failed: 0x{status:08x}"
                ))));
            }
            None => {
                stop(io_thread, &cmd_tx);
                return Err(Error::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out opening the RFCOMM channel",
                )));
            }
        }

        let io = MacosRfcomm {
            shared,
            commands: cmd_tx,
            io_thread: Mutex::new(Some(io_thread)),
            _lock: lock,
        };
        Ok(Self {
            inner: SharedSppTransport::new(io, timeout)?,
        })
    }
}

impl Transport for SppTransport {
    fn transact(&mut self, packet: &[u8]) -> momentum_core::Result<Vec<u8>> {
        self.inner.transact(packet)
    }

    fn subscribe(&self) -> Option<Receiver<NotificationFrame>> {
        self.inner.subscribe()
    }
}

/// Block until the async open reports a status or `timeout` elapses.
fn wait_for_open(shared: &Shared, timeout: Duration) -> Option<i32> {
    let deadline = Instant::now() + timeout;
    let mut state = shared.state.lock().unwrap();
    loop {
        if let Some(status) = state.open_status {
            return Some(status);
        }
        if state.closed {
            return Some(-1);
        }
        let remaining = deadline.checked_duration_since(Instant::now())?;
        let (next, timed_out) = shared.cv.wait_timeout(state, remaining).unwrap();
        state = next;
        if timed_out.timed_out() && state.open_status.is_none() {
            return None;
        }
    }
}

/// List paired-and-connected devices whose name contains `needle`
/// (case-insensitive), as `(address, name)` with colon-separated addresses.
pub fn connected_devices_matching(needle: &str) -> Result<Vec<(String, String)>> {
    let needle = needle.to_ascii_lowercase();
    let mut out = Vec::new();
    // SAFETY: returns an autoreleased NSArray of IOBluetoothDevice or nil.
    let Some(devices) = (unsafe { IOBluetoothDevice::pairedDevices() }) else {
        return Ok(out);
    };
    for device in devices.iter() {
        let Some(device) = device.downcast_ref::<IOBluetoothDevice>() else {
            continue;
        };
        // SAFETY: simple accessors on a live device.
        if !unsafe { device.isConnected() } {
            continue;
        }
        let name = unsafe { device.name() }.to_string();
        if !name.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        let Some(address) = (unsafe { device.addressString() }) else {
            continue;
        };
        out.push((normalize_address(&address.to_string()), name));
    }
    Ok(out)
}

/// IOBluetooth reports addresses as `xx-xx-…`; the rest of the app uses
/// colon-separated uppercase.
fn normalize_address(address: &str) -> String {
    address.replace('-', ":").to_ascii_uppercase()
}

fn acquire_lock(addr: BdAddr) -> Result<File> {
    let path = std::env::temp_dir().join(format!(
        "momentum-{}.lock",
        addr.to_string().replace(':', "_")
    ));
    let file = File::create(&path)?;
    // SAFETY: `file` owns a valid fd; `LOCK_EX | LOCK_NB` is a defined flag set.
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
