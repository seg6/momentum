//! Windows RFCOMM/SPP byte-stream adapter.
//!
//! This module owns only WinSock Bluetooth setup and process locking. The
//! shared `spp_transport` module handles SPP framing, request/response
//! matching, notification routing, and the reader thread.

use std::io;
use std::mem::{MaybeUninit, size_of};
use std::sync::OnceLock;
use std::time::Duration;

use crossbeam_channel::Receiver;
use momentum_core::{NotificationFrame, Transport};
use tracing::debug;
use windows_sys::Win32::Devices::Bluetooth::{AF_BTH, BTHPROTO_RFCOMM, SOCKADDR_BTH};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    INVALID_SOCKET, SD_BOTH, SO_SNDTIMEO, SOCK_STREAM, SOCKADDR, SOCKET, SOCKET_ERROR, SOL_SOCKET,
    WSADATA, WSAECONNABORTED, WSAECONNRESET, WSAEINTR, WSAENOTCONN, WSAENOTSOCK, WSAESHUTDOWN,
    WSAGetLastError, WSAStartup, closesocket, connect as winsock_connect, recv, send, setsockopt,
    shutdown, socket as winsock_socket,
};
use windows_sys::Win32::System::Threading::CreateMutexW;

use crate::spp_transport::{SppIo, SppTransport as SharedSppTransport};
use crate::{BdAddr, Error, Result};

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
unsafe impl Sync for ProcessLock {}

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

struct WindowsRfcomm {
    socket: Socket,
    _lock: ProcessLock,
}

impl SppIo for WindowsRfcomm {
    fn read_some(&self, buf: &mut [u8]) -> io::Result<usize> {
        read_some(self.socket.raw(), buf)
    }

    fn write_all(&self, data: &[u8]) -> io::Result<()> {
        write_all(self.socket.raw(), data)
    }

    fn shutdown(&self) {
        // SAFETY: `self.socket` owns a valid WinSock socket while this adapter is alive.
        unsafe {
            shutdown(self.socket.raw(), SD_BOTH);
        }
    }
}

/// RFCOMM transport with Sennheiser SPP framing.
pub struct SppTransport {
    inner: SharedSppTransport<WindowsRfcomm>,
}

impl core::fmt::Debug for SppTransport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SppTransport").finish_non_exhaustive()
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
        let socket = connect_socket(&sock_addr, timeout)?;
        Ok(Self {
            inner: SharedSppTransport::new(
                WindowsRfcomm {
                    socket,
                    _lock: lock,
                },
                timeout,
            )?,
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

#[allow(dead_code)]
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
