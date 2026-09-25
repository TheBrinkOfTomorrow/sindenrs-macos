//! Keeping the tracker responsive while a game loads the machine.
//!
//! On macOS a windowless background app is fair game for App Nap and energy throttling, and
//! plain threads run at default priority, so a busy emulator (plus, say, a backup upload) can
//! starve the tracker: frames seconds late, the aim jumping (docs/macos-notes.md). The fix is
//! what real-time input software does: declare latency-critical user activity for as long as
//! the gun is tracked, and run the tracker and camera threads at user-interactive QoS. On
//! other platforms these are no-ops.

/// Latency-critical user activity, declared while this is alive. Keep it for as long as guns
/// are tracked.
#[must_use = "the activity ends when this is dropped"]
pub struct Activity {
    #[cfg(target_os = "macos")]
    token:
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
}

impl std::fmt::Debug for Activity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Activity")
    }
}

/// Tell the OS that interactive, latency-critical work is under way: on macOS no App Nap, no
/// timer coalescing, and the machine does not idle-sleep.
pub fn begin(reason: &str) -> Activity {
    #[cfg(target_os = "macos")]
    {
        use objc2_foundation::{NSActivityOptions, NSProcessInfo, NSString};
        let token = NSProcessInfo::processInfo().beginActivityWithOptions_reason(
            NSActivityOptions::UserInteractive,
            &NSString::from_str(reason),
        );
        Activity { token }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = reason;
        Activity {}
    }
}

#[cfg(target_os = "macos")]
impl Drop for Activity {
    fn drop(&mut self) {
        // SAFETY: the token beginActivity returned, ended once.
        #[allow(unsafe_code)]
        unsafe {
            objc2_foundation::NSProcessInfo::processInfo().endActivity(&self.token);
        }
    }
}

/// Run the calling thread at user-interactive QoS (macOS), so the scheduler favours it over
/// batch work. Call it at the top of each tracker thread.
pub fn interactive_thread() {
    #[cfg(target_os = "macos")]
    {
        /// `QOS_CLASS_USER_INTERACTIVE`.
        const USER_INTERACTIVE: u32 = 0x21;
        #[allow(unsafe_code)]
        extern "C" {
            fn pthread_set_qos_class_self_np(qos: u32, relative_priority: i32) -> i32;
        }
        // SAFETY: plain values; it only affects the calling thread.
        #[allow(unsafe_code)]
        let rc = unsafe { pthread_set_qos_class_self_np(USER_INTERACTIVE, 0) };
        if rc != 0 {
            tracing::debug!("could not raise the tracker thread's QoS: {rc}");
        }
    }
}
