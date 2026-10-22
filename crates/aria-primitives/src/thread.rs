//! A thread that must keep up with a realtime producer.
//!
//! An audio callback fills a bounded ring from a realtime thread. At the
//! default class, a saturated CPU leaves the thread draining it unscheduled
//! for longer than the ring holds.

/// Lifts the calling thread to the user-interactive class. Failure is
/// logged, not returned: a quiet machine keeps up anyway.
#[cfg(target_vendor = "apple")]
pub fn raise_scheduling_class() {
    if let Err(error) = set_user_interactive() {
        tracing::warn!(%error, "could not raise the thread's scheduling class");
    }
}

/// This platform has no unprivileged way to lift a thread.
#[cfg(not(target_vendor = "apple"))]
pub fn raise_scheduling_class() {}

/// A QoS band, not a time-constraint policy: the thread may also run
/// inference and disk writes, which a realtime budget would punish.
#[cfg(target_vendor = "apple")]
#[allow(unsafe_code, reason = "pthread QoS has no safe binding")]
fn set_user_interactive() -> std::io::Result<()> {
    use libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE;

    /// Relative priority runs from 0 (top of band) down to
    /// `QOS_MIN_RELATIVE_PRIORITY` (-15).
    const TOP_OF_BAND: libc::c_int = 0;

    // SAFETY: the call takes its arguments by value and changes only the
    // calling thread's QoS.
    let errno =
        unsafe { libc::pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, TOP_OF_BAND) };
    if errno == 0 {
        Ok(())
    } else {
        Err(std::io::Error::from_raw_os_error(errno))
    }
}

#[cfg(all(test, target_vendor = "apple"))]
mod tests {
    use libc::qos_class_t::{QOS_CLASS_UNSPECIFIED, QOS_CLASS_USER_INTERACTIVE};

    use super::*;

    #[test]
    #[allow(unsafe_code, reason = "reading the QoS back has no safe binding")]
    fn sets_the_calling_thread_to_user_interactive() {
        std::thread::spawn(|| {
            set_user_interactive().expect("set the QoS");
            let mut class = QOS_CLASS_UNSPECIFIED;
            let mut relative_priority = 0;
            // SAFETY: the out-pointers address locals that outlive the call,
            // and the thread reports the class it just set, a `qos_class_t`
            // variant.
            let errno = unsafe {
                libc::pthread_get_qos_class_np(
                    libc::pthread_self(),
                    &mut class,
                    &mut relative_priority,
                )
            };
            assert_eq!(errno, 0);
            assert_eq!(class as u32, QOS_CLASS_USER_INTERACTIVE as u32);
        })
        .join()
        .expect("thread panicked");
    }
}
