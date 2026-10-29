//! The macOS hotkey listener: a listen-only CGEventTap.
//!
//! Everything runs on the caller's thread. The tap callback only ever
//! executes inside [`CFRunLoop::run_in_mode`] on that same thread, so the
//! shared state needs interior mutability but no synchronization. Shutdown
//! and health checks happen between 100 ms run-loop slices.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::thread;
use std::time::{Duration, Instant};

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFString};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType,
};

use super::{HotkeyKey, KeyEvent, ListenerEvent, ListenerExit, Press, PressDetector};
use crate::accessibility::Trust;

const PERMISSION_RETRY: Duration = Duration::from_secs(3);
/// One run-loop slice: bounds shutdown latency, not press latency — the
/// slice returns early whenever the tap handles an event, and a pending
/// hold shortens it.
const RUN_LOOP_SLICE_SECONDS: f64 = 0.1;
/// How often to confirm the tap is still enabled. macOS silently disables
/// taps it suspects are stalled, and a disabled tap looks healthy from here.
const REARM_INTERVAL: Duration = Duration::from_secs(3);
const STOP_POLL: Duration = Duration::from_millis(100);

pub(crate) struct MacOsHotkeyListener;

/// Shared between `listen` and the tap callback, same thread only — the
/// callback runs inside the run-loop slices `listen` drives.
struct TapState {
    detector: RefCell<PressDetector>,
    /// The live tap, so the callback can re-enable it on a disable event.
    tap: RefCell<Option<CFRetained<CFMachPort>>>,
    /// Press edges seen this slice; drained by `watch`.
    presses: RefCell<Vec<Press>>,
}

impl TapState {
    fn feed(&self, event: KeyEvent) {
        let press = self.detector.borrow_mut().handle(event, Instant::now());
        self.presses.borrow_mut().extend(press);
    }

    fn tick(&self) {
        let press = self.detector.borrow_mut().tick(Instant::now());
        self.presses.borrow_mut().extend(press);
    }

    fn reset(&self) {
        let press = self.detector.borrow_mut().reset();
        self.presses.borrow_mut().extend(press);
    }

    /// How long the next run-loop slice may last: a pending hold cuts it
    /// short so the hold starts on time.
    fn slice_seconds(&self) -> f64 {
        let Some(deadline) = self.detector.borrow().hold_deadline() else {
            return RUN_LOOP_SLICE_SECONDS;
        };
        deadline
            .saturating_duration_since(Instant::now())
            .as_secs_f64()
            .min(RUN_LOOP_SLICE_SECONDS)
    }
}

impl MacOsHotkeyListener {
    pub(crate) fn listen(
        key: HotkeyKey,
        events: impl Fn(ListenerEvent),
        should_stop: impl Fn() -> bool,
    ) -> ListenerExit {
        let state = TapState {
            detector: RefCell::new(PressDetector::new(key)),
            tap: RefCell::new(None),
            presses: RefCell::new(Vec::new()),
        };
        let mut grant_requested = false;
        loop {
            if should_stop() {
                return ListenerExit::Shutdown;
            }
            let Some(tap) = arm(&state) else {
                if !grant_requested {
                    Trust::request();
                    grant_requested = true;
                }
                events(ListenerEvent::AwaitingPermission);
                if wait(PERMISSION_RETRY, &should_stop) {
                    return ListenerExit::Shutdown;
                }
                continue;
            };
            events(ListenerEvent::Armed);
            let stopped = watch(&state, &tap, &events, &should_stop);
            tap.invalidate();
            *state.tap.borrow_mut() = None;
            // A release may have gone by while the port was dead.
            state.reset();
            for press in state.presses.take() {
                events(ListenerEvent::Press(press));
            }
            if stopped {
                return ListenerExit::Shutdown;
            }
        }
    }
}

/// Creates the tap and hooks it into this thread's run loop. A tap created
/// before the grant never recovers.
fn arm(state: &TapState) -> Option<CFRetained<CFMachPort>> {
    if !Trust::check_now().is_trusted() {
        return None;
    }
    let tap = create_tap(state)?;
    let source = CFMachPort::new_run_loop_source(None, Some(&tap), 0)?;
    let run_loop = CFRunLoop::current()?;
    run_loop.add_source(Some(&source), Some(&default_mode()));
    CGEvent::tap_enable(&tap, true);
    *state.tap.borrow_mut() = Some(tap.clone());
    tracing::info!(trigger = "hotkey", "event tap armed");
    Some(tap)
}

/// Drives the run loop and reports presses until shutdown or tap death.
/// Returns true when stopping for shutdown.
fn watch(
    state: &TapState,
    tap: &CFMachPort,
    events: &impl Fn(ListenerEvent),
    should_stop: &impl Fn() -> bool,
) -> bool {
    let mode = default_mode();
    let mut last_health_check = Instant::now();
    loop {
        CFRunLoop::run_in_mode(Some(&mode), state.slice_seconds(), true);
        state.tick();
        for press in state.presses.take() {
            events(ListenerEvent::Press(press));
        }
        if should_stop() {
            return true;
        }
        if last_health_check.elapsed() >= REARM_INTERVAL {
            last_health_check = Instant::now();
            if !tap.is_valid() {
                tracing::warn!(trigger = "hotkey", "event tap port died; re-arming");
                return false;
            }
            // Re-enabling a tap disabled for a missing grant never works.
            if !CGEvent::tap_is_enabled(tap) {
                tracing::warn!(trigger = "hotkey", "event tap disabled; re-arming");
                return false;
            }
            // macOS leaves a running tap alive after the grant is revoked.
            if !Trust::check_now().is_trusted() {
                tracing::warn!(trigger = "hotkey", "the grant was revoked; disarming");
                return false;
            }
        }
    }
}

/// The default run-loop mode, named rather than read from the
/// `kCFRunLoopDefaultMode` extern static — run-loop modes compare by name,
/// and a named constant keeps the unsafe surface at the two tap items.
fn default_mode() -> CFRetained<CFString> {
    CFString::from_static_str("kCFRunLoopDefaultMode")
}

/// Sleeps in stop-poll slices; returns true when shutdown was requested.
fn wait(total: Duration, should_stop: &impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if should_stop() {
            return true;
        }
        thread::sleep(STOP_POLL);
    }
    should_stop()
}

const EVENT_MASK: u64 = (1 << CGEventType::KeyDown.0 as u64)
    | (1 << CGEventType::FlagsChanged.0 as u64)
    | (1 << CGEventType::LeftMouseDown.0 as u64)
    | (1 << CGEventType::RightMouseDown.0 as u64)
    | (1 << CGEventType::OtherMouseDown.0 as u64);

#[allow(
    unsafe_code,
    reason = "CGEventTap is a C callback API: creating the tap requires \
              raw-pointer FFI; the invariants live on the SAFETY comment and \
              nothing outside this file's two tap items is unsafe"
)]
fn create_tap(state: &TapState) -> Option<CFRetained<CFMachPort>> {
    let user_info = std::ptr::from_ref(state).cast_mut().cast::<c_void>();
    // SAFETY: `user_info` points at the `TapState` owned by `listen`'s stack
    // frame, which outlives the tap — `listen` invalidates the port before
    // the frame can return. The callback runs only inside
    // `CFRunLoop::run_in_mode` on this same thread, so the pointer is always
    // valid, never accessed concurrently, and only ever reborrowed shared.
    // Listen-only: the tap sees copies of events and never holds up input.
    // An active tap freezes all input once the Accessibility grant is revoked.
    unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::ListenOnly,
            EVENT_MASK,
            Some(tap_callback),
            user_info,
        )
    }
}

#[allow(
    unsafe_code,
    reason = "the CGEventTap callback receives raw pointers by contract; the \
              invariants live on the SAFETY comments inside"
)]
unsafe extern "C-unwind" fn tap_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: `user_info` is the `TapState` pointer `create_tap` registered;
    // see the invariants there. Shared reborrow only — interior mutability
    // handles the writes.
    let state = unsafe { &*user_info.cast::<TapState>() };
    // SAFETY: CGEventTap guarantees `event` is a valid event for the duration
    // of the callback; it is only read here.
    let event = unsafe { event.as_ref() };

    match event_type {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            // Presses during the disabled window are lost; the log is the
            // only trace of them, and a release among them would otherwise
            // leave a hold running on the clock alone.
            tracing::warn!(
                trigger = "hotkey",
                reason = if event_type == CGEventType::TapDisabledByTimeout {
                    "timeout"
                } else {
                    "user_input"
                },
                "event tap disabled; re-enabling"
            );
            state.reset();
            if let Some(tap) = state.tap.borrow().as_ref() {
                CGEvent::tap_enable(tap, true);
            }
        }
        CGEventType::KeyDown => {
            // Streaming delivery types transcript segments while recording;
            // those synthetic keystrokes must not read as a chord and void
            // the stop tap. Hardware events carry source PID 0.
            let source_pid =
                CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID);
            // The fn key emits its own keyDown (179) beside its flags change;
            // it is never typing and must not read as a chord either.
            let key_code =
                CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
            if source_pid == 0 && key_code != 179 {
                state.feed(KeyEvent::KeyDown);
            }
        }
        CGEventType::FlagsChanged => {
            let key_code =
                CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
            let flags = CGEvent::flags(Some(event)).0;
            let source_pid =
                CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID);
            state.feed(KeyEvent::FlagsChanged {
                key_code,
                flags,
                synthetic: source_pid != 0,
            });
        }
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
            state.feed(KeyEvent::MouseDown);
        }
        _ => {}
    }
    std::ptr::from_ref(event).cast_mut()
}
