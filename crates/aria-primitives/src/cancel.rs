//! Cooperative cancellation and operation deadlines.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A time budget, fixed when the operation is admitted.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    timeout: Duration,
    expires_at: Instant,
}

impl Deadline {
    pub fn after(timeout: Duration) -> Self {
        Self {
            timeout,
            expires_at: Instant::now() + timeout,
        }
    }

    pub fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// The budget as granted, for error reporting.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

/// A cancellation signal polled by long-running work.
#[derive(Clone)]
pub struct CancelToken(Inner);

#[derive(Clone)]
enum Inner {
    Never,
    Flag(Arc<AtomicBool>),
    Fn(Arc<dyn Fn() -> bool + Send + Sync>),
}

impl CancelToken {
    pub fn new() -> Self {
        Self(Inner::Flag(Arc::new(AtomicBool::new(false))))
    }

    /// A token that never cancels, for callers without an abort path.
    pub const fn never() -> Self {
        Self(Inner::Never)
    }

    /// Reads cancellation from an existing condition.
    pub fn from_fn(should_cancel: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self(Inner::Fn(Arc::new(should_cancel)))
    }

    /// Cancels every clone of this token. A `never()` token ignores it, and a
    /// `from_fn` token answers from its closure.
    pub fn cancel(&self) {
        if let Inner::Flag(flag) = &self.0 {
            flag.store(true, Ordering::Release);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        match &self.0 {
            Inner::Never => false,
            Inner::Flag(flag) => flag.load(Ordering::Acquire),
            Inner::Fn(should_cancel) => should_cancel(),
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Inner::Never => formatter.write_str("CancelToken::Never"),
            Inner::Flag(flag) => formatter
                .debug_tuple("CancelToken::Flag")
                .field(&flag.load(Ordering::Acquire))
                .finish(),
            Inner::Fn(_) => formatter.write_str("CancelToken::Fn"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_token_is_not_cancelled() {
        assert!(!CancelToken::new().is_cancelled());
        assert!(!CancelToken::never().is_cancelled());
    }

    #[test]
    fn cancellation_reaches_every_clone() {
        let token = CancelToken::new();
        let observer = token.clone();

        token.cancel();

        assert!(observer.is_cancelled());
    }

    #[test]
    fn never_ignores_cancel() {
        let token = CancelToken::never();

        token.cancel();

        assert!(!token.is_cancelled());
    }

    #[test]
    fn a_deadline_expires_once_its_budget_is_spent() {
        let deadline = Deadline::after(Duration::from_millis(10));

        assert!(!deadline.expired());
        assert_eq!(deadline.timeout(), Duration::from_millis(10));
        std::thread::sleep(Duration::from_millis(15));
        assert!(deadline.expired());
    }

    #[test]
    fn from_fn_answers_from_the_closure() {
        let flag = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&flag);
        let token = CancelToken::from_fn(move || observed.load(Ordering::Acquire));

        assert!(!token.is_cancelled());
        flag.store(true, Ordering::Release);
        assert!(token.is_cancelled());
    }
}
