//! A build-once cell for a value whose construction can fail.

use std::sync::{Mutex, OnceLock};

/// Losers of the build race wait, and a failure leaves the slot empty for the
/// next caller to retry.
pub struct Slot<T> {
    initialization: Mutex<()>,
    value: OnceLock<T>,
}

impl<T> Slot<T> {
    pub fn empty() -> Self {
        Self {
            initialization: Mutex::new(()),
            value: OnceLock::new(),
        }
    }

    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    pub fn get_or_try_init<E>(&self, build: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        if let Some(value) = self.value.get() {
            return Ok(value);
        }
        let _initialization = self
            .initialization
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(value) = self.value.get() {
            return Ok(value);
        }
        let value = build()?;
        Ok(self.value.get_or_init(|| value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_build_leaves_the_slot_open_for_a_retry() {
        let slot: Slot<u32> = Slot::empty();

        let failed: Result<&u32, &str> = slot.get_or_try_init(|| Err("no model"));
        assert_eq!(failed.unwrap_err(), "no model");

        let built: Result<&u32, &str> = slot.get_or_try_init(|| Ok(7));
        assert_eq!(built.unwrap(), &7);
    }

    #[test]
    fn a_built_slot_never_builds_again() {
        let slot: Slot<u32> = Slot::empty();
        let _first: Result<&u32, &str> = slot.get_or_try_init(|| Ok(1));

        let second: Result<&u32, &str> = slot.get_or_try_init(|| panic!("rebuilt a full slot"));
        assert_eq!(second.unwrap(), &1);
    }
}
