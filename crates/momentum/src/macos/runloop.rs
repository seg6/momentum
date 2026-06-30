//! Drive app logic on a worker thread while the **main** thread runs a
//! `CFRunLoop`.
//!
//! IOBluetooth delivers its RFCOMM delegate callbacks (open-complete, inbound
//! data, close) to the main thread's run loop, regardless of which thread opened
//! the channel. A plain CLI never runs that loop, so the callbacks never fire.
//! We therefore move the actual command logic onto a worker thread and dedicate
//! the main thread to pumping the run loop until the worker finishes.

use std::ffi::c_void;
use std::sync::mpsc;
use std::thread;

use anyhow::Result;

#[allow(non_camel_case_types)]
type CFRunLoopRef = *mut c_void;
#[allow(non_camel_case_types)]
type CFStringRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source_handled: u8) -> i32;
    fn CFRunLoopStop(rl: CFRunLoopRef);
    static kCFRunLoopDefaultMode: CFStringRef;
}

struct SendPtr(CFRunLoopRef);
// SAFETY: the pointer is only used for `CFRunLoopStop`, which is thread-safe.
unsafe impl Send for SendPtr {}

impl SendPtr {
    fn stop(&self) {
        // SAFETY: `self.0` is the live main run loop.
        unsafe { CFRunLoopStop(self.0) };
    }
}

/// Run `f` on a worker thread; pump this (main) thread's run loop until it
/// finishes, then return its result.
pub fn drive<F>(f: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    // SAFETY: called on the main thread; returns the main run loop.
    let main_runloop = SendPtr(unsafe { CFRunLoopGetCurrent() });
    let (tx, rx) = mpsc::channel::<Result<()>>();

    let worker = thread::Builder::new()
        .name("momentum-app".into())
        .spawn(move || {
            let result = f();
            let _ = tx.send(result);
            // Wake the main run loop so it stops pumping promptly.
            main_runloop.stop();
        })
        .expect("spawn momentum app thread");

    loop {
        // SAFETY: `kCFRunLoopDefaultMode` is a valid CF constant. Returns on a
        // serviced source, the timeout, or `CFRunLoopStop`.
        unsafe { CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.1, 0) };
        if let Ok(result) = rx.try_recv() {
            let _ = worker.join();
            return result;
        }
    }
}
