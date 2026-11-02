//! Pieces the `aria` commands share at the terminal.

use std::sync::{Mutex, MutexGuard};

mod activity;
mod interrupt;
mod keys;

pub(crate) use interrupt::{INTERRUPTED, Interrupt};

/// A poisoned lock means a thread panicked while holding it. Nothing here
/// is left half-written by a panic, so the guard is taken.
fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
