//! Make momentum its own TCC-responsible process.
//!
//! When a CLI is launched from a shell, macOS attributes its TCC requests (e.g.
//! Bluetooth) to the *responsible process* — which walks up the launch chain to
//! the terminal or window manager, not momentum. That ancestor has no
//! `NSBluetoothAlwaysUsageDescription`, so TCC **crashes** the process instead
//! of prompting.
//!
//! The fix (as used by LLDB, Qt Creator, and iTerm2) is to re-spawn ourselves
//! with `responsibility_spawnattrs_setdisclaim`, which makes the child its own
//! responsible process. TCC then reads momentum's embedded Info.plist, prompts
//! once, and attaches the grant to momentum's signing identity.

use std::ffi::CString;
use std::os::unix::ffi::OsStringExt;
use std::sync::atomic::{AtomicI32, Ordering};

use anyhow::{Context, Result, bail};
use libc::{c_char, c_int, pid_t, posix_spawnattr_t};

/// Set once on the re-spawned worker so it doesn't disclaim a second time.
const GUARD_ENV: &str = "MOMENTUM_MACOS_DISCLAIMED";

unsafe extern "C" {
    /// Private libsystem API: mark the spawn so the child becomes its own TCC
    /// responsible process rather than inheriting the parent's.
    fn responsibility_spawnattrs_setdisclaim(attrs: *mut posix_spawnattr_t, disclaim: c_int)
    -> c_int;

    /// macOS accessor for the process environment pointer.
    fn _NSGetEnviron() -> *mut *const *const c_char;
}

/// If we are not yet the disclaimed worker, re-spawn ourselves disclaimed,
/// wait, and exit forwarding the worker's status. Otherwise return so the
/// caller continues as the worker.
pub fn ensure_own_responsibility() -> Result<()> {
    if std::env::var_os(GUARD_ENV).is_some() {
        return Ok(());
    }

    let exe = std::env::current_exe().context("locating current executable")?;
    let exe_c = CString::new(exe.into_os_string().into_vec())
        .context("executable path contains a NUL byte")?;

    // argv: our executable path, then the original arguments after argv[0].
    let mut argv_owned = vec![exe_c.clone()];
    for arg in std::env::args_os().skip(1) {
        argv_owned
            .push(CString::new(arg.into_vec()).context("argument contains a NUL byte")?);
    }
    let mut argv: Vec<*mut c_char> = argv_owned
        .iter()
        .map(|a| a.as_ptr() as *mut c_char)
        .collect();
    argv.push(std::ptr::null_mut());

    // Inherit the environment plus the re-entry guard.
    // SAFETY: single-threaded startup; we set the var before reading `environ`.
    unsafe { std::env::set_var(GUARD_ENV, "1") };
    // SAFETY: `_NSGetEnviron` returns a valid pointer to the live environ array.
    let environ = unsafe { *_NSGetEnviron() } as *mut *mut c_char;

    let mut attrs: posix_spawnattr_t = unsafe { std::mem::zeroed() };
    // SAFETY: `attrs` is a valid, owned, zeroed spawnattr we destroy below.
    let rc = unsafe { libc::posix_spawnattr_init(&mut attrs) };
    if rc != 0 {
        bail!("posix_spawnattr_init failed: {rc}");
    }
    let result = spawn_disclaimed(&exe_c, &mut argv, environ, &mut attrs);
    // SAFETY: `attrs` was successfully initialized above.
    unsafe { libc::posix_spawnattr_destroy(&mut attrs) };

    let pid = result?;
    forward_exit(pid)
}

fn spawn_disclaimed(
    exe: &CString,
    argv: &mut [*mut c_char],
    environ: *mut *mut c_char,
    attrs: *mut posix_spawnattr_t,
) -> Result<pid_t> {
    // SAFETY: `attrs` is initialized; disclaim takes 0/1.
    let rc = unsafe { responsibility_spawnattrs_setdisclaim(attrs, 1) };
    if rc != 0 {
        bail!("responsibility_spawnattrs_setdisclaim failed: {rc}");
    }

    let mut pid: pid_t = 0;
    // SAFETY: `exe`, `argv` (NUL-terminated), `environ`, and `attrs` are valid;
    // file actions are unused (children inherit our stdio and process group).
    let rc = unsafe {
        libc::posix_spawn(
            &mut pid,
            exe.as_ptr(),
            std::ptr::null(),
            attrs,
            argv.as_mut_ptr(),
            environ,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::from_raw_os_error(rc))
            .context("re-spawning momentum as its own responsible process");
    }
    Ok(pid)
}

/// PID of the disclaimed worker, for the signal-forwarding handler.
static WORKER_PID: AtomicI32 = AtomicI32::new(0);

/// Relay a terminating signal to the worker. `kill` is async-signal-safe.
extern "C" fn forward_signal(sig: c_int) {
    let pid = WORKER_PID.load(Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: `kill` is async-signal-safe; a stale pid just yields ESRCH.
        unsafe { libc::kill(pid, sig) };
    }
}

/// Reap the worker and exit with its status. Terminating signals sent to the
/// launcher are forwarded to the worker (which then exits, ending the wait);
/// the launcher itself installs handlers so it survives to forward the status.
fn forward_exit(pid: pid_t) -> ! {
    WORKER_PID.store(pid, Ordering::SeqCst);
    let handler = forward_signal as extern "C" fn(c_int) as libc::sighandler_t;
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGQUIT, libc::SIGHUP] {
        // SAFETY: installing a valid `extern "C"` handler for a real signal.
        unsafe { libc::signal(sig, handler) };
    }

    let mut status: c_int = 0;
    loop {
        // SAFETY: standard blocking wait on our child.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        eprintln!("momentum: failed to wait for worker process: {err}");
        std::process::exit(1);
    }

    // Decode the BSD wait status (same encoding on macOS).
    let term_sig = status & 0x7f;
    if term_sig == 0 {
        std::process::exit((status >> 8) & 0xff);
    }
    std::process::exit(128 + term_sig);
}
