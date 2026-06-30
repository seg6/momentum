//! Linux RFCOMM/SPP byte-stream adapter.
//!
//! This module owns only Linux-specific Bluetooth socket setup and process
//! locking. The shared `spp_transport` module handles SPP framing,
//! request/response matching, notification routing, and the reader thread.

use std::env;
use std::fs::File;
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crossbeam_channel::Receiver;
use libc::{
    AF_BLUETOOTH, EAGAIN, EBUSY, EINTR, EWOULDBLOCK, LOCK_EX, LOCK_NB, SHUT_RDWR, SO_SNDTIMEO,
    SOCK_STREAM, SOL_SOCKET, c_void, connect, flock as libc_flock, read, setsockopt, shutdown,
    sockaddr, socket, timeval, write,
};
use momentum_core::{NotificationFrame, Transport};
use tracing::debug;

use crate::spp_transport::{SppIo, SppTransport as SharedSppTransport};
use crate::{BdAddr, Error, Result};

/// `BTPROTO_RFCOMM` from `<bluetooth/bluetooth.h>`. Not exported by every
/// `libc` build, so we define it locally.
const BTPROTO_RFCOMM: i32 = 3;

const CONNECT_MAX_ATTEMPTS: u32 = 8;
const CONNECT_BACKOFF: Duration = Duration::from_millis(500);

/// `struct sockaddr_rc` from `<bluetooth/rfcomm.h>`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrRc {
    family: libc::sa_family_t,
    bdaddr: [u8; 6],
    channel: u8,
}

struct LinuxRfcomm {
    fd: OwnedFd,
    /// Held for its `Drop`, which releases the `flock` on close.
    _lock: File,
}

impl SppIo for LinuxRfcomm {
    fn read_some(&self, buf: &mut [u8]) -> io::Result<usize> {
        read_some(self.fd.as_raw_fd(), buf)
    }

    fn write_all(&self, data: &[u8]) -> io::Result<()> {
        write_all(self.fd.as_raw_fd(), data)
    }

    fn shutdown(&self) {
        // SAFETY: `self.fd` owns a valid fd while this adapter is alive.
        unsafe {
            shutdown(self.fd.as_raw_fd(), SHUT_RDWR);
        }
    }
}

/// RFCOMM transport with Sennheiser SPP framing.
pub struct SppTransport {
    inner: SharedSppTransport<LinuxRfcomm>,
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

        Ok(Self {
            inner: SharedSppTransport::new(LinuxRfcomm { fd, _lock: lock }, timeout)?,
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
    set_timeout(fd.as_raw_fd(), SO_SNDTIMEO, timeout)?;

    // SAFETY: `addr` lives until the end of this call; `size_of::<SockAddrRc>`
    // is the layout the kernel expects (verified by `#[repr(C)]`).
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
