//! A build-once cell for a value whose construction can fail.

use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// How often a waiter that may give up checks whether it should. A finished
/// build wakes waiters at once; only giving up waits on this.
const STOP_POLL: Duration = Duration::from_millis(20);

/// Losers of the build race wait for the build to end. A failed or panicking
/// build leaves the slot empty and wakes them, so the next caller retries.
pub struct Slot<T> {
    building: Mutex<bool>,
    ended: Condvar,
    value: OnceLock<T>,
}

impl<T> Slot<T> {
    pub fn empty() -> Self {
        Self {
            building: Mutex::new(false),
            ended: Condvar::new(),
            value: OnceLock::new(),
        }
    }

    pub fn get(&self) -> Option<&T> {
        self.value.get()
    }

    pub fn get_or_try_init<E>(&self, build: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        self.init(None::<fn() -> Option<E>>, build)
    }

    /// As `get_or_try_init`, but a caller waiting on another's build gives up
    /// once `stopped` returns an error.
    pub fn get_or_try_init_unless<E>(
        &self,
        stopped: impl Fn() -> Option<E>,
        build: impl FnOnce() -> Result<T, E>,
    ) -> Result<&T, E> {
        self.init(Some(stopped), build)
    }

    fn init<E>(
        &self,
        stopped: Option<impl Fn() -> Option<E>>,
        build: impl FnOnce() -> Result<T, E>,
    ) -> Result<&T, E> {
        if let Some(value) = self.value.get() {
            return Ok(value);
        }
        let mut building = self.lock();
        loop {
            if let Some(value) = self.value.get() {
                return Ok(value);
            }
            if !*building {
                break;
            }
            building = match &stopped {
                None => self.ended.wait(building).unwrap_or_else(|p| p.into_inner()),
                Some(stopped) => {
                    if let Some(error) = stopped() {
                        return Err(error);
                    }
                    self.ended
                        .wait_timeout(building, STOP_POLL)
                        .unwrap_or_else(|p| p.into_inner())
                        .0
                }
            };
        }
        *building = true;
        drop(building);
        // Declared before the value is set, so it drops after: a woken
        // waiter sees the value, or an empty slot it may build itself.
        let _turn = Turn { slot: self };
        let value = build()?;
        Ok(self.value.get_or_init(|| value))
    }

    /// The flag is only set and read under the lock, never across a build,
    /// so a poisoned lock still holds a valid flag.
    fn lock(&self) -> MutexGuard<'_, bool> {
        self.building.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// One caller's build. Ending it, by return or by panic, frees the slot for
/// the next build and wakes every waiter.
struct Turn<'a, T> {
    slot: &'a Slot<T>,
}

impl<T> Drop for Turn<'_, T> {
    fn drop(&mut self) {
        *self.slot.lock() = false;
        self.slot.ended.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    fn hold_build<'scope>(
        scope: &'scope thread::Scope<'scope, '_>,
        slot: &'scope Slot<u32>,
        result: Result<u32, &'static str>,
    ) -> mpsc::Sender<()> {
        let (building, built) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        scope.spawn(move || {
            let _ = slot.get_or_try_init(|| {
                building.send(()).unwrap();
                released.recv().unwrap();
                result
            });
        });
        built.recv().unwrap();
        release
    }

    #[test]
    fn a_waiter_takes_the_value_another_caller_built() {
        let slot: Slot<u32> = Slot::empty();

        thread::scope(|scope| {
            let release = hold_build(scope, &slot, Ok(1));
            let waiter = scope.spawn(|| {
                slot.get_or_try_init(|| -> Result<u32, &str> { panic!("built twice") })
                    .copied()
            });
            thread::sleep(Duration::from_millis(50));
            assert!(
                !waiter.is_finished(),
                "the waiter parks until the build ends"
            );
            release.send(()).unwrap();

            assert_eq!(waiter.join().unwrap(), Ok(1));
        });
    }

    #[test]
    fn a_waiter_builds_after_another_callers_build_fails() {
        let slot: Slot<u32> = Slot::empty();

        thread::scope(|scope| {
            let release = hold_build(scope, &slot, Err("no model"));
            let waiter = scope.spawn(|| {
                slot.get_or_try_init(|| -> Result<u32, &str> { Ok(2) })
                    .copied()
            });
            thread::sleep(Duration::from_millis(50));
            assert!(
                !waiter.is_finished(),
                "the waiter parks until the build ends"
            );
            release.send(()).unwrap();

            assert_eq!(waiter.join().unwrap(), Ok(2));
        });
    }

    #[test]
    fn a_panicking_build_leaves_the_slot_open() {
        let slot: Slot<u32> = Slot::empty();

        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _ = slot.get_or_try_init(|| -> Result<u32, &str> { panic!("load crashed") });
        }));

        assert!(panicked.is_err());
        assert_eq!(
            slot.get_or_try_init(|| -> Result<u32, &str> { Ok(3) }),
            Ok(&3)
        );
    }

    #[test]
    fn a_waiter_gives_up_while_another_caller_builds() {
        let slot: Slot<u32> = Slot::empty();

        thread::scope(|scope| {
            let release = hold_build(scope, &slot, Ok(1));

            let waited: Result<&u32, &str> =
                slot.get_or_try_init_unless(|| Some("cancelled"), || panic!("built twice"));

            assert_eq!(waited.unwrap_err(), "cancelled");
            release.send(()).unwrap();
        });
        assert_eq!(slot.get(), Some(&1));
    }

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
