//! IOBluetooth RFCOMM channel delegate.
//!
//! IOBluetooth delivers RFCOMM events (open-complete, inbound data, close) to
//! an Objective-C delegate. We define one with `objc2` and funnel every event
//! into the shared [`Shared`] state, which the IO thread and SPP reader observe.

use std::ffi::c_void;
use std::sync::Arc;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};

use super::Shared;

/// Ivars: the shared buffer/open-status the delegate writes into.
pub(super) struct DelegateIvars {
    pub(super) shared: Arc<Shared>,
}

define_class!(
    // SAFETY: `NSObject` is a safe superclass to inherit from, and the class
    // adds no `-dealloc`/`-init` requirements beyond the default.
    #[unsafe(super(NSObject))]
    #[name = "MomentumRfcommDelegate"]
    #[ivars = DelegateIvars]
    pub(super) struct Delegate;

    impl Delegate {
        /// `-rfcommChannelOpenComplete:status:` — the async open finished.
        #[unsafe(method(rfcommChannelOpenComplete:status:))]
        fn open_complete(&self, _channel: *mut AnyObject, status: i32) {
            tracing::debug!(status, "rfcommChannelOpenComplete");
            let shared = &self.ivars().shared;
            let mut state = shared.state.lock().unwrap();
            state.open_status = Some(status);
            drop(state);
            shared.cv.notify_all();
        }

        /// `-rfcommChannelData:data:length:` — inbound bytes from the headset.
        #[unsafe(method(rfcommChannelData:data:length:))]
        fn data(&self, _channel: *mut AnyObject, data: *mut c_void, length: usize) {
            if data.is_null() || length == 0 {
                return;
            }
            tracing::trace!(length, "rfcommChannelData");
            // SAFETY: IOBluetooth guarantees `data` points to `length` valid
            // bytes for the duration of this callback.
            let bytes = unsafe { std::slice::from_raw_parts(data as *const u8, length) };
            let shared = &self.ivars().shared;
            let mut state = shared.state.lock().unwrap();
            state.buf.extend(bytes);
            drop(state);
            shared.cv.notify_all();
        }

        /// `-rfcommChannelClosed:` — the channel went away.
        #[unsafe(method(rfcommChannelClosed:))]
        fn closed(&self, _channel: *mut AnyObject) {
            let shared = &self.ivars().shared;
            let mut state = shared.state.lock().unwrap();
            state.closed = true;
            drop(state);
            shared.cv.notify_all();
        }
    }
);

impl Delegate {
    pub(super) fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(DelegateIvars { shared });
        // SAFETY: standard `-init` on a freshly allocated NSObject subclass.
        unsafe { msg_send![super(this), init] }
    }
}
