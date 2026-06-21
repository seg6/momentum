//! Windows RFCOMM/SPP transport.
//!
//! Connects to an already-paired classic-Bluetooth headset using WinSock's
//! Bluetooth address family (`AF_BTH`) and RFCOMM protocol. The GAIA/SPP
//! framing and response/notification demultiplexing mirrors the Linux
//! transport so higher layers see the same blocking [`Transport`] interface.

use std::io;
use std::mem::{MaybeUninit, size_of};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use momentum_core::{NotificationFrame, Transport, spp};
use tracing::{debug, trace, warn};
use windows_sys::Win32::Devices::Bluetooth::{AF_BTH, BTHPROTO_RFCOMM, SOCKADDR_BTH};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    INVALID_SOCKET, SD_BOTH, SO_SNDTIMEO, SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ERROR, SOL_SOCKET,
    WSADATA, WSAECONNABORTED, WSAECONNRESET, WSAEINTR, WSAENOTCONN, WSAENOTSOCK, WSAESHUTDOWN,
    WSAGetLastError, WSAStartup, closesocket, connect as winsock_connect, recv, send, setsockopt,
    shutdown, socket as winsock_socket,
};
use windows_sys::Win32::System::Threading::CreateMutexW;

use crate::{BdAddr, Error, Result};

const READ_BUFFER_SIZE: usize = 1024;
const NOTIFICATION_BACKLOG: usize = 256;
const RESPONSE_BIT: u16 = 0x0100;
const ERROR_BIT: u16 = 0x0080;

struct InFlight {
    expected_vendor: u16,
    expected_response: u16,
    expected_error: u16,
    tx: Sender<Vec<u8>>,
}

struct Socket(SOCKET);

impl Socket {
    fn raw(&self) -> SOCKET {
        self.0
    }
}

impl Drop for Socket {
    fn drop(&mut self) {
        if self.0 != INVALID_SOCKET {
            // SAFETY: `self.0` is a WinSock socket owned by this wrapper.
            unsafe {
                closesocket(self.0);
            }
        }
    }
}

struct ProcessLock {
    handle: HANDLE,
}

// Windows HANDLE values are kernel references that can be closed from the
// owning process regardless of the thread that drops this wrapper.
unsafe impl Send for ProcessLock {}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            // SAFETY: `handle` came from `CreateMutexW` and is owned here.
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

/// RFCOMM transport with SPP framing.
pub struct SppTransport {
    socket: Arc<Socket>,
    _lock: ProcessLock,
    write_buffer: Vec<u8>,
    timeout: Duration,
    in_flight: Arc<Mutex<Option<InFlight>>>,
    notifications: Receiver<NotificationFrame>,
    reader: Option<JoinHandle<()>>,
}

impl core::fmt::Debug for SppTransport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SppTransport")
            .field("socket", &self.socket.raw())
            .finish_non_exhaustive()
    }
}

impl SppTransport {
    /// Connect to `addr` on RFCOMM `channel`, applying `timeout` to writes
    /// and to each `transact` round-trip.
    pub fn connect(addr: BdAddr, channel: u8, timeout: Duration) -> Result<Self> {
        ensure_winsock()?;
        let lock = acquire_lock(addr)?;
        let sock_addr = SOCKADDR_BTH {
            addressFamily: AF_BTH,
            btAddr: addr.as_u64(),
            serviceClassId: Default::default(),
            port: u32::from(channel),
        };

        debug!(%addr, channel, "connecting RFCOMM");
        let socket = Arc::new(connect_socket(&sock_addr, timeout)?);
        let in_flight: Arc<Mutex<Option<InFlight>>> = Arc::new(Mutex::new(None));
        let (notif_tx, notif_rx) = bounded::<NotificationFrame>(NOTIFICATION_BACKLOG);

        let reader = {
            let socket = Arc::clone(&socket);
            let in_flight = Arc::clone(&in_flight);
            let notif_rx_for_reader = notif_rx.clone();
            thread::Builder::new()
                .name("momentum-spp-reader".into())
                .spawn(move || reader_loop(socket, in_flight, notif_tx, notif_rx_for_reader))
                .map_err(|e| Error::from(io::Error::other(e.to_string())))?
        };

        Ok(Self {
            socket,
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
        if let Err(e) = write_all(self.socket.raw(), &self.write_buffer) {
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
        // SAFETY: `self.socket` owns a valid WinSock socket until this Drop returns.
        unsafe {
            shutdown(self.socket.raw(), SD_BOTH);
        }
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

fn reader_loop(
    socket: Arc<Socket>,
    in_flight: Arc<Mutex<Option<InFlight>>>,
    notifications: Sender<NotificationFrame>,
    notifications_drain: Receiver<NotificationFrame>,
) {
    let raw = socket.raw();
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
        match read_some(raw, &mut buf) {
            Ok(0) => {
                debug!("SPP reader: EOF");
                break;
            }
            Ok(n) => read_buffer.extend_from_slice(&buf[..n]),
            Err(e) => {
                if is_shutdown_error(&e) {
                    debug!("SPP reader: socket closed");
                } else {
                    warn!(error = %e, "SPP reader: read failed");
                }
                break;
            }
        }
    }

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

fn acquire_lock(addr: BdAddr) -> Result<ProcessLock> {
    let name = format!("Local\\momentum-{}", addr.to_string().replace(':', "_"));
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: null security attributes request defaults; `wide` is NUL-terminated.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide.as_ptr()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error().into());
    }

    // SAFETY: `GetLastError` is meaningful immediately after `CreateMutexW`.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: `handle` was returned by `CreateMutexW` and is not retained.
        unsafe {
            CloseHandle(handle);
        }
        return Err(Error::Busy(addr.to_string()));
    }

    Ok(ProcessLock { handle })
}

fn connect_socket(addr: &SOCKADDR_BTH, timeout: Duration) -> io::Result<Socket> {
    let raw = unsafe { winsock_socket(i32::from(AF_BTH), SOCK_STREAM, BTHPROTO_RFCOMM as i32) };
    if raw == INVALID_SOCKET {
        return Err(wsa_error());
    }
    let socket = Socket(raw);
    set_timeout(socket.raw(), SO_SNDTIMEO, timeout)?;

    // SAFETY: `addr` lives until the end of this call; `SOCKADDR_BTH` has the
    // layout WinSock expects for `AF_BTH` RFCOMM sockets.
    let result = unsafe {
        winsock_connect(
            socket.raw(),
            core::ptr::from_ref(addr).cast::<SOCKADDR>(),
            size_of::<SOCKADDR_BTH>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        return Err(wsa_error());
    }
    Ok(socket)
}

fn set_timeout(socket: SOCKET, opt: i32, timeout: Duration) -> io::Result<()> {
    let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
    // SAFETY: `millis` is a stack value of the type WinSock expects for
    // `SO_SNDTIMEO`, and `socket` is an open socket.
    let rc = unsafe {
        setsockopt(
            socket,
            SOL_SOCKET,
            opt,
            core::ptr::from_ref(&millis).cast::<u8>(),
            size_of::<u32>() as i32,
        )
    };
    if rc == SOCKET_ERROR {
        return Err(wsa_error());
    }
    Ok(())
}

fn write_all(socket: SOCKET, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let chunk_len = data.len().min(i32::MAX as usize);
        // SAFETY: `data` is a borrowed slice valid for the duration of the call.
        let written = unsafe { send(socket, data.as_ptr(), chunk_len as i32, 0) };
        if written == SOCKET_ERROR {
            let error = wsa_error();
            if error.raw_os_error() == Some(WSAEINTR) {
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

fn read_some(socket: SOCKET, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let chunk_len = buf.len().min(i32::MAX as usize);
        // SAFETY: `buf` is a mutable borrowed slice valid for the call.
        let len = unsafe { recv(socket, buf.as_mut_ptr(), chunk_len as i32, 0) };
        if len >= 0 {
            return Ok(len as usize);
        }
        let error = wsa_error();
        if error.raw_os_error() == Some(WSAEINTR) {
            continue;
        }
        return Err(error);
    }
}

fn ensure_winsock() -> io::Result<()> {
    static STARTUP_RESULT: OnceLock<i32> = OnceLock::new();
    let code = *STARTUP_RESULT.get_or_init(|| {
        let mut data = MaybeUninit::<WSADATA>::zeroed();
        // SAFETY: `data` points to writable storage for WinSock startup data.
        unsafe { WSAStartup(0x0202, data.as_mut_ptr()) }
    });
    if code == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code))
    }
}

fn wsa_error() -> io::Error {
    // SAFETY: `WSAGetLastError` reads thread-local WinSock error state.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn is_shutdown_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code)
            if code == WSAECONNRESET
                || code == WSAECONNABORTED
                || code == WSAENOTCONN
                || code == WSAENOTSOCK
                || code == WSAESHUTDOWN
    )
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
