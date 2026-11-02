//! The one line a command shows while it is busy.

use std::io::IsTerminal;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::lock;

/// One status line: what is happening, how long it has run, and the key
/// that ends it. On a terminal it rewrites itself every second. Elsewhere
/// each change is its own line and the key is left unsaid, because a pipe
/// wants the output and not the advice.
pub(super) struct Activity {
    line: Arc<Mutex<Line>>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct Line {
    text: Option<String>,
    /// When the work started: the first line, not the call that opened it.
    since: Option<Instant>,
    /// The key that ends the work, while it still does.
    key: Option<String>,
}

impl Line {
    fn seconds(&self) -> u64 {
        self.since.map_or(0, |since| since.elapsed().as_secs())
    }

    fn redraw(&self) {
        let Some(text) = &self.text else { return };
        eprint!("\r\x1b[2K{text} · {}s", self.seconds());
        if let Some(key) = &self.key {
            // Dim, so the key reads as a note beside the work.
            eprint!(" \x1b[2m({key})\x1b[0m");
        }
    }
}

impl Activity {
    /// Silent until the first `show`, so a command can arm itself before
    /// it has anything to say.
    pub(super) fn start(key: String) -> Self {
        let line = Arc::new(Mutex::new(Line::default()));
        if !std::io::stderr().is_terminal() {
            return Self {
                line,
                stop: None,
                thread: None,
            };
        }
        lock(&line).key = Some(key);
        let (stop, stopped) = mpsc::channel();
        let thread = thread::spawn({
            let line = Arc::clone(&line);
            // A closed channel is the stop; a timeout is the next tick.
            move || {
                while let Err(mpsc::RecvTimeoutError::Timeout) =
                    stopped.recv_timeout(Duration::from_secs(1))
                {
                    lock(&line).redraw();
                }
            }
        });
        Self {
            line,
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    pub(super) fn show(&self, text: impl Into<String>) {
        let mut line = lock(&self.line);
        line.since.get_or_insert_with(Instant::now);
        if self.thread.is_none() {
            eprintln!("{}", text.into());
            return;
        }
        line.text = Some(text.into());
        line.redraw();
    }

    /// Zero before the first `show`: that wait belonged to something else.
    pub(super) fn seconds(&self) -> u64 {
        lock(&self.line).seconds()
    }

    /// The key no longer ends anything; the line stays without it.
    pub(super) fn drop_key(&self) {
        let mut line = lock(&self.line);
        line.key = None;
        if self.on_terminal() {
            line.redraw();
        }
    }

    /// The line was never true. Wipes it rather than leaving it standing.
    pub(super) fn erase(&mut self) {
        lock(&self.line).text = None;
        self.finish();
        if self.on_terminal() {
            eprint!("\r\x1b[2K");
        }
    }

    fn on_terminal(&self) -> bool {
        self.thread.is_some()
    }

    pub(super) fn finish(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
            let mut line = lock(&self.line);
            if line.text.is_some() {
                line.key = None;
                line.redraw();
                eprintln!();
            }
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        self.finish();
    }
}
