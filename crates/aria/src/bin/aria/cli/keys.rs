//! Single keys, read while a command is busy.

use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use rustix::termios::{
    OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcgetpgrp, tcsetattr,
};

use super::lock;

const ESCAPE: u8 = 0x1b;
const CTRL_C: u8 = 0x03;

/// The cooked settings, kept where any thread can put them back.
///
/// A `Drop` is not enough on its own: `std::process::exit` runs none, and
/// the press that ends the process can arrive on either the key thread or
/// the signal thread.
static COOKED: Mutex<Option<Termios>> = Mutex::new(None);

/// Puts the terminal back the way the shell had it. Safe to call when it
/// was never taken.
pub(super) fn restore_terminal() {
    if let Some(cooked) = lock(&COOKED).take() {
        let _ = tcsetattr(std::io::stdin(), OptionalActions::Now, &cooked);
    }
}

/// The terminal in raw mode, which is what makes a single key readable
/// without Return. Raw also stops Ctrl-C from raising a signal, which is
/// why the reader below ends the process itself.
struct RawMode;

impl RawMode {
    fn enter() -> Option<Self> {
        let stdin = std::io::stdin();
        let cooked = tcgetattr(&stdin).ok()?;
        let mut raw = cooked.clone();
        raw.make_raw();
        // A read comes back after a tenth of a second with whatever
        // arrived, so the reader notices the phase ending.
        raw.special_codes[SpecialCodeIndex::VMIN] = 0;
        raw.special_codes[SpecialCodeIndex::VTIME] = 1;
        tcsetattr(&stdin, OptionalActions::Now, &raw).ok()?;
        *lock(&COOKED) = Some(cooked);
        Some(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Watches for Esc and Ctrl-C while one phase is open.
pub(super) struct Keys {
    stopped: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Keys {
    /// `None` when there is no terminal to read keys from, or when this
    /// process is not the one the terminal is talking to. Ctrl-C then
    /// arrives as a signal instead.
    pub(super) fn watch(
        escape: impl Fn() + Send + 'static,
        quit: impl FnOnce() + Send + 'static,
    ) -> Option<Self> {
        if !foreground() {
            return None;
        }
        let raw = RawMode::enter()?;
        let stopped = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let stopped = Arc::clone(&stopped);
            move || {
                let mut buffer = [0u8; 8];
                let mut quitting = false;
                while !stopped.load(Ordering::Relaxed) {
                    let Ok(count) = read_stdin(&mut buffer) else {
                        break;
                    };
                    let keys = &buffer[..count];
                    if keys.contains(&CTRL_C) {
                        // The work outlives this process, so leaving it
                        // running would strand a recording or an agent.
                        quitting = true;
                        break;
                    }
                    // An arrow key starts with Escape too, so only a byte
                    // arriving alone is the key itself.
                    if keys == [ESCAPE] {
                        escape();
                    }
                }
                drop(raw);
                if quitting {
                    quit();
                }
            }
        });
        Some(Self {
            stopped,
            thread: Some(thread),
        })
    }
}

impl Drop for Keys {
    /// Joined rather than detached: the terminal is raw until the thread
    /// lets go of it, and the line printed next would step down the page.
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Whether the terminal is talking to this process group.
///
/// A background job shares the terminal but may not touch its settings:
/// `tcsetattr` from a background group raises SIGTTOU, whose default
/// action stops the process. `aria ask … &` would freeze at the prompt.
fn foreground() -> bool {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() || !std::io::stderr().is_terminal() {
        return false;
    }
    tcgetpgrp(&stdin).is_ok_and(|owner| owner == rustix::process::getpgrp())
}

/// Straight from the descriptor, never `std::io::stdin`, whose buffer is
/// shared by the whole process and would hold a keypress back for the next
/// phase to find.
fn read_stdin(buffer: &mut [u8]) -> rustix::io::Result<usize> {
    rustix::io::read(std::io::stdin(), buffer)
}
