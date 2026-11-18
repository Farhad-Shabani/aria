//! How one chat job ends early. A cancel lands only while the job is open:
//! once it starts typing, it types its text whole, since stopping midway
//! would leave part of it in the document.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use aria_kit::cancel::CancelToken;
use tokio_util::sync::CancellationToken;

use super::super::operation::operation_cancellation;

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Settlement {
    Open,
    Cancelled,
    Typing,
}

/// The one owner of a chat job's cancel. The actor and the hotkey tap both
/// end a job through it.
pub(in crate::daemon) struct JobControl {
    settlement: Arc<AtomicU8>,
    /// What the job's blocking stages check: this control's cancel, the
    /// caller's, or the daemon's shutdown.
    cancellation: CancelToken,
}

impl JobControl {
    pub(in crate::daemon) fn new(caller: CancelToken, shutdown: CancellationToken) -> Self {
        let settlement = Arc::new(AtomicU8::new(Settlement::Open as u8));
        let cancelled = {
            let settlement = Arc::clone(&settlement);
            CancelToken::from_fn(move || {
                settlement.load(Ordering::Acquire) == Settlement::Cancelled as u8
                    || caller.is_cancelled()
            })
        };
        Self {
            settlement,
            cancellation: operation_cancellation(cancelled, shutdown),
        }
    }

    #[cfg(test)]
    pub(in crate::daemon) fn never() -> Self {
        Self::new(CancelToken::never(), CancellationToken::new())
    }

    pub(in crate::daemon) fn cancellation(&self) -> &CancelToken {
        &self.cancellation
    }

    /// Whether a cancel can still end the job.
    pub(in crate::daemon) fn is_open(&self) -> bool {
        self.settlement.load(Ordering::Acquire) == Settlement::Open as u8
    }

    /// Ends the job. `true` only when this call ended it: `false` once it
    /// was cancelled or started typing.
    pub(in crate::daemon) fn cancel(&self) -> bool {
        self.settle(Settlement::Cancelled)
    }

    /// Closes the job to a cancel before it types. `false` when it was
    /// cancelled first.
    pub(in crate::daemon) fn start_typing(&self) -> bool {
        self.settle(Settlement::Typing)
            || self.settlement.load(Ordering::Acquire) == Settlement::Typing as u8
    }

    fn settle(&self, settlement: Settlement) -> bool {
        self.settlement
            .compare_exchange(
                Settlement::Open as u8,
                settlement as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_settles_once() {
        let cancelled = JobControl::never();
        assert!(cancelled.cancel());
        assert!(!cancelled.cancel(), "it was ended already");
        assert!(!cancelled.start_typing(), "a cancelled job types nothing");
        assert!(cancelled.cancellation().is_cancelled());

        let typing = JobControl::never();
        assert!(typing.start_typing());
        assert!(typing.start_typing(), "it may check again before each step");
        assert!(!typing.cancel(), "typing goes on to the end");
        assert!(!typing.is_open());
        assert!(!typing.cancellation().is_cancelled());
    }

    #[test]
    fn the_caller_still_stops_a_typing_job() {
        let caller = CancelToken::new();
        let control = JobControl::new(caller.clone(), CancellationToken::new());
        assert!(control.start_typing());

        caller.cancel();

        assert!(control.cancellation().is_cancelled());
    }

    #[test]
    fn shutdown_still_stops_a_typing_job() {
        let shutdown = CancellationToken::new();
        let control = JobControl::new(CancelToken::never(), shutdown.clone());
        assert!(control.start_typing());

        shutdown.cancel();

        assert!(control.cancellation().is_cancelled());
    }
}
