//! A value built on first use.

use super::slot::Slot;

type Build<T, E> = dyn Fn() -> Result<T, E> + Send + Sync;

/// A failed build stores nothing, so the next caller retries.
pub struct Lazy<T, E> {
    build: Box<Build<T, E>>,
    value: Slot<T>,
}

impl<T, E> Lazy<T, E> {
    pub fn new(build: impl Fn() -> Result<T, E> + Send + Sync + 'static) -> Self {
        Self {
            build: Box::new(build),
            value: Slot::empty(),
        }
    }

    pub fn get(&self) -> Result<&T, E> {
        self.value.get_or_try_init(|| (self.build)())
    }

    /// The value only if it was already built. For a caller with nothing to
    /// do when it was not, and no reason to build it now.
    pub fn built(&self) -> Option<&T> {
        self.value.get()
    }
}
