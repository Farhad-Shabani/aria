//! How a running capture learns it should end.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::cancel::CancelToken;

/// The capture-side stop check. Owned by the capture worker, which drops it
/// the moment recording ends — owners can rely on that drop for cleanup.
pub struct StopSignal(Box<dyn Fn() -> bool + Send>);

impl StopSignal {
    /// A capture that only its mode or timeout ends.
    pub fn never() -> Self {
        Self(Box::new(|| false))
    }

    pub fn from_fn(should_stop: impl Fn() -> bool + Send + 'static) -> Self {
        Self(Box::new(should_stop))
    }

    pub fn should_stop(&self) -> bool {
        (self.0)()
    }
}

impl From<CancelToken> for StopSignal {
    fn from(cancel: CancelToken) -> Self {
        Self::from_fn(move || cancel.is_cancelled())
    }
}

impl From<Stopper> for StopSignal {
    fn from(stopper: Stopper) -> Self {
        Self::from_fn(move || stopper.is_stopped())
    }
}

/// A handle that ends a recording from anywhere: clone it, hand it to a
/// hotkey or UI, call `stop()`.
#[derive(Clone, Debug, Default)]
pub struct Stopper(Arc<AtomicBool>);

impl Stopper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_stays_quiet() {
        assert!(!StopSignal::never().should_stop());
    }

    #[test]
    fn a_stopper_reaches_the_signal_through_a_clone() {
        let stopper = Stopper::new();
        let signal = StopSignal::from(stopper.clone());

        assert!(!signal.should_stop());
        stopper.stop();
        assert!(signal.should_stop());
    }

    #[test]
    fn a_cancel_token_reads_through() {
        let cancel = CancelToken::new();
        let signal = StopSignal::from(cancel.clone());

        assert!(!signal.should_stop());
        cancel.cancel();
        assert!(signal.should_stop());
    }
}
