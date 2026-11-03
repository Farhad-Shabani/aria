//! The macOS hotkey listener: a listen-only CGEventTap, plus, while Fn is
//! the Aria key, an active one that keeps Fn's macOS action from running.
//!
//! Everything runs on the caller's thread. The tap callbacks only ever
//! execute inside [`CFRunLoop::run_in_mode`] on that same thread, so the
//! shared state needs interior mutability but no synchronization. Shutdown
//! and health checks happen between 100 ms run-loop slices.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFString};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapCallBack, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUserDefaults};

use super::{AriaKey, KeyEvent, ListenerEvent, Modifiers, Press, PressDetector};
use crate::accessibility::{self, Trust};

const PERMISSION_RETRY: Duration = Duration::from_secs(3);
/// One run-loop slice: bounds shutdown latency, not press latency — the
/// slice returns early whenever the tap handles an event, and a pending
/// hold shortens it.
const RUN_LOOP_SLICE_SECONDS: f64 = 0.1;
/// How often to confirm the tap is still enabled. macOS silently disables
/// taps it suspects are stalled, and a disabled tap looks healthy from here.
const REARM_INTERVAL: Duration = Duration::from_secs(3);
const STOP_POLL: Duration = Duration::from_millis(100);

// Generic flag masks, so either side's key counts.
/// kCGEventFlagMaskShift
const SHIFT: u64 = 0x0002_0000;
/// kCGEventFlagMaskControl
const CONTROL: u64 = 0x0004_0000;
/// kCGEventFlagMaskCommand
const COMMAND: u64 = 0x0010_0000;

/// The key code of the key down and up macOS sends after Fn comes up alone.
/// They run the action set in "Press fn key to".
const GLOBE_KEY_CODE: i64 = 179;

/// Modifier Keys keeps one remap list per keyboard, under this prefix plus
/// the keyboard's vendor and product ids, in the current host's global domain.
const MODIFIER_MAPPING_PREFIX: &str = "com.apple.keyboard.modifiermapping.";
const MAPPING_SOURCE: &str = "HIDKeyboardModifierMappingSrc";
const MAPPING_DESTINATION: &str = "HIDKeyboardModifierMappingDst";

impl AriaKey {
    /// Virtual keycode carried in the key's flags-changed event.
    fn key_code(self) -> i64 {
        match self {
            Self::RightOption => 61,
            Self::Fn => 63,
        }
    }

    /// Whether this key is DOWN per the event's flags. Sided keys use the raw
    /// device-specific bit — the generic mask stays set while the
    /// opposite-side twin is held, which would swallow the release.
    fn is_down(self, flags: u64) -> bool {
        match self {
            // NX_DEVICERALTKEYMASK
            Self::RightOption => flags & 0x40 != 0,
            // kCGEventFlagMaskSecondaryFn
            Self::Fn => flags & 0x0080_0000 != 0,
        }
    }

    /// The HID usage Modifier Keys stores: the usage page in the high 32
    /// bits, the usage in the low ones.
    fn hid_usage(self) -> i64 {
        match self {
            // Keyboard page, Right Alt.
            Self::RightOption => 0x7_0000_00E6,
            // Apple's vendor page, Fn.
            Self::Fn => 0xFF_0000_0003,
        }
    }
}

fn held_modifiers(flags: u64) -> Modifiers {
    [
        (SHIFT, Modifiers::SHIFT),
        (CONTROL, Modifiers::CONTROL),
        (COMMAND, Modifiers::COMMAND),
    ]
    .into_iter()
    .filter(|(mask, _)| flags & mask != 0)
    .fold(Modifiers::NONE, |held, (_, modifier)| held | modifier)
}

/// A software-generated modifier change is dropped.
fn translate(key: AriaKey, key_code: i64, flags: u64, synthetic: bool) -> Option<KeyEvent> {
    let held = held_modifiers(flags);
    if key_code == key.key_code() {
        return Some(if key.is_down(flags) {
            KeyEvent::AriaDown { held }
        } else {
            KeyEvent::AriaUp
        });
    }
    (!synthetic).then_some(KeyEvent::Modifiers { held })
}

pub struct MacOsHotkeyListener;

/// Shared between `listen` and the tap callbacks, same thread only — the
/// callbacks run inside the run-loop slices `listen` drives.
struct TapState {
    key: AriaKey,
    detector: RefCell<PressDetector>,
    /// The live taps, so a callback can re-enable them on a disable event.
    taps: RefCell<Vec<CFRetained<CFMachPort>>>,
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

    fn enable_taps(&self) {
        for tap in self.taps.borrow().iter() {
            CGEvent::tap_enable(tap, true);
        }
    }

    fn disarm(&self) {
        for tap in self.taps.take() {
            tap.invalidate();
        }
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

/// A panic out of `listen` still invalidates the taps, so no callback ever
/// sees a dropped `TapState`.
impl Drop for TapState {
    fn drop(&mut self) {
        self.disarm();
    }
}

impl MacOsHotkeyListener {
    pub const PERMISSION: &str = "Accessibility access";

    pub fn unarmed_reason(_key: AriaKey) -> String {
        format!(
            "the dictation hotkey is waiting for permission: System Settings → \
             Privacy & Security → {} → + → {}; it arms within ~3s",
            accessibility::settings_pane(),
            accessibility::target_label()
        )
    }

    /// Modifier Keys turns the key into another one before any tap sees it.
    pub fn is_remapped(key: AriaKey) -> bool {
        remaps_away(&modifier_remaps(), key)
    }

    pub fn listen(key: AriaKey, events: impl Fn(ListenerEvent), should_stop: impl Fn() -> bool) {
        let state = TapState {
            key,
            detector: RefCell::new(PressDetector::new()),
            taps: RefCell::new(Vec::new()),
            presses: RefCell::new(Vec::new()),
        };
        let mut grant_requested = false;
        loop {
            if should_stop() {
                return;
            }
            if !arm(&state) {
                if !grant_requested {
                    Trust::request();
                    grant_requested = true;
                }
                events(ListenerEvent::AwaitingPermission);
                if wait(PERMISSION_RETRY, &should_stop) {
                    return;
                }
                continue;
            }
            events(ListenerEvent::Armed);
            let stopped = watch(&state, &events, &should_stop);
            state.disarm();
            // A release may have gone by while the port was dead.
            state.reset();
            for press in state.presses.take() {
                events(ListenerEvent::Press(press));
            }
            if stopped {
                return;
            }
        }
    }
}

/// A keyboard still sends the Aria key when it maps the key onto itself, or
/// maps another key onto it, as an Option and Command swap does.
// ponytail: a list left by an unplugged keyboard still counts; match it
// against connected keyboards if that misleads anyone.
fn remaps_away(keyboards: &[Vec<(i64, i64)>], key: AriaKey) -> bool {
    let aria = key.hid_usage();
    keyboards.iter().any(|remaps| {
        let sends_it_away = remaps
            .iter()
            .any(|&(source, destination)| source == aria && destination != aria);
        let another_sends_it = remaps
            .iter()
            .any(|&(source, destination)| source != aria && destination == aria);
        sends_it_away && !another_sends_it
    })
}

/// Each keyboard's Modifier Keys remaps, as (source, destination) usages.
/// Empty when no keyboard has any. The standard search list includes the
/// current host's global domain, where the remaps live.
fn modifier_remaps() -> Vec<Vec<(i64, i64)>> {
    let (keys, values) = NSUserDefaults::standardUserDefaults()
        .dictionaryRepresentation()
        .to_vecs();
    keys.iter()
        .zip(values)
        .filter(|(key, _)| key.to_string().starts_with(MODIFIER_MAPPING_PREFIX))
        .filter_map(|(_, remaps)| remaps.downcast::<NSArray>().ok())
        .map(|remaps| remaps.to_vec().into_iter().filter_map(remap).collect())
        .collect()
}

fn remap(entry: Retained<AnyObject>) -> Option<(i64, i64)> {
    let entry = entry.downcast::<NSDictionary>().ok()?;
    Some((
        hid_usage_in(&entry, MAPPING_SOURCE)?,
        hid_usage_in(&entry, MAPPING_DESTINATION)?,
    ))
}

fn hid_usage_in(remap: &NSDictionary, field: &str) -> Option<i64> {
    let value = remap.objectForKey(&NSString::from_str(field))?;
    Some(value.downcast::<NSNumber>().ok()?.longLongValue())
}

/// Creates the taps and hooks them into this thread's run loop. A tap
/// created before the grant never recovers.
fn arm(state: &TapState) -> bool {
    if !Trust::check_now().is_trusted() {
        return false;
    }
    if add_taps(state).is_none() {
        state.disarm();
        return false;
    }
    state.enable_taps();
    tracing::info!(trigger = "hotkey", "event tap armed");
    true
}

/// Each tap joins `state` as soon as it exists, so a later failure still
/// invalidates it.
fn add_taps(state: &TapState) -> Option<()> {
    let listener = create_tap(
        state,
        CGEventTapOptions::ListenOnly,
        LISTENER_EVENTS,
        Some(listener_callback),
    )?;
    state.taps.borrow_mut().push(listener);
    match state.key {
        AriaKey::Fn => {
            // An active tap holds each event until it returns, so this one
            // takes key events only, and only while Fn is the Aria key.
            // Revoking the grant stalls those events until `watch`'s next
            // trust check disarms it, at most a REARM_INTERVAL later.
            let blocker = create_tap(
                state,
                CGEventTapOptions::Default,
                BLOCKER_EVENTS,
                Some(blocker_callback),
            )?;
            state.taps.borrow_mut().push(blocker);
        }
        AriaKey::RightOption => {}
    }
    let run_loop = CFRunLoop::current()?;
    for tap in state.taps.borrow().iter() {
        let source = CFMachPort::new_run_loop_source(None, Some(tap), 0)?;
        run_loop.add_source(Some(&source), Some(&default_mode()));
    }
    Some(())
}

/// Drives the run loop and reports presses until shutdown or tap death.
/// Returns true when stopping for shutdown.
fn watch(
    state: &TapState,
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
            let taps = state.taps.borrow();
            if !taps.iter().all(|tap| tap.is_valid()) {
                tracing::warn!(trigger = "hotkey", "event tap port died; re-arming");
                return false;
            }
            // Re-enabling a tap disabled for a missing grant never works.
            if !taps.iter().all(|tap| CGEvent::tap_is_enabled(tap)) {
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
/// and a named constant keeps the unsafe surface at the tap items.
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

const LISTENER_EVENTS: u64 = (1 << CGEventType::KeyDown.0 as u64)
    | (1 << CGEventType::FlagsChanged.0 as u64)
    | (1 << CGEventType::LeftMouseDown.0 as u64)
    | (1 << CGEventType::RightMouseDown.0 as u64)
    | (1 << CGEventType::OtherMouseDown.0 as u64);

const BLOCKER_EVENTS: u64 =
    (1 << CGEventType::KeyDown.0 as u64) | (1 << CGEventType::KeyUp.0 as u64);

#[allow(
    unsafe_code,
    reason = "CGEventTap is a C callback API: creating the tap requires \
              raw-pointer FFI; the invariants live on the SAFETY comment and \
              nothing outside this file's tap items is unsafe"
)]
fn create_tap(
    state: &TapState,
    options: CGEventTapOptions,
    events: u64,
    callback: CGEventTapCallBack,
) -> Option<CFRetained<CFMachPort>> {
    let user_info = std::ptr::from_ref(state).cast_mut().cast::<c_void>();
    // SAFETY: `user_info` points at the `TapState` owned by `listen`'s stack
    // frame, which outlives the taps — `TapState::disarm`, also run by its
    // `Drop`, invalidates every port before the frame can end. The callbacks
    // run only inside
    // `CFRunLoop::run_in_mode` on this same thread, so the pointer is always
    // valid, never accessed concurrently, and only ever reborrowed shared.
    unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            options,
            events,
            callback,
            user_info,
        )
    }
}

#[allow(
    unsafe_code,
    reason = "the CGEventTap callback receives raw pointers by contract; the \
              invariants live on the SAFETY comments inside"
)]
unsafe extern "C-unwind" fn listener_callback(
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
            state.enable_taps();
        }
        CGEventType::KeyDown => {
            // Streaming delivery types transcript segments while recording;
            // those synthetic keystrokes must not read as a chord and void
            // the stop tap. Hardware events carry source PID 0.
            let source_pid =
                CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID);
            // Fn's own key down is never typing and must not read as a chord
            // either.
            let key_code =
                CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
            if source_pid == 0 && key_code != GLOBE_KEY_CODE {
                state.feed(KeyEvent::KeyDown);
            }
        }
        CGEventType::FlagsChanged => {
            let key_code =
                CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
            let flags = CGEvent::flags(Some(event)).0;
            let source_pid =
                CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID);
            if let Some(event) = translate(state.key, key_code, flags, source_pid != 0) {
                state.feed(event);
            }
        }
        CGEventType::LeftMouseDown | CGEventType::RightMouseDown | CGEventType::OtherMouseDown => {
            state.feed(KeyEvent::MouseDown);
        }
        _ => {}
    }
    std::ptr::from_ref(event).cast_mut()
}

#[allow(
    unsafe_code,
    reason = "the CGEventTap callback receives raw pointers by contract; the \
              invariants live on the SAFETY comments inside"
)]
unsafe extern "C-unwind" fn blocker_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: NonNull<CGEvent>,
    user_info: *mut c_void,
) -> *mut CGEvent {
    // SAFETY: as in `listener_callback`.
    let state = unsafe { &*user_info.cast::<TapState>() };
    // SAFETY: as in `listener_callback`.
    let event = unsafe { event.as_ref() };
    match event_type {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            tracing::warn!(trigger = "hotkey", "fn blocker disabled; re-enabling");
            state.enable_taps();
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            let key_code =
                CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
            if key_code == GLOBE_KEY_CODE {
                return std::ptr::null_mut();
            }
        }
        _ => {}
    }
    std::ptr::from_ref(event).cast_mut()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RIGHT_OPTION: u64 = 0x40;
    const LEFT_OPTION: u64 = 0x0008_0020;
    const SECONDARY_FN: u64 = 0x0080_0000;
    const RIGHT_OPTION_KEY: i64 = 61;
    const LEFT_OPTION_KEY: i64 = 58;
    const LEFT_SHIFT_KEY: i64 = 56;
    const FN_KEY: i64 = 63;

    fn hardware(key: AriaKey, key_code: i64, flags: u64) -> Option<KeyEvent> {
        translate(key, key_code, flags, false)
    }

    #[test]
    fn left_option_held_across_the_release_still_counts() {
        let key = AriaKey::RightOption;
        assert!(matches!(
            hardware(key, RIGHT_OPTION_KEY, RIGHT_OPTION | LEFT_OPTION),
            Some(KeyEvent::AriaDown {
                held: Modifiers::NONE
            })
        ));
        assert!(matches!(
            hardware(key, RIGHT_OPTION_KEY, LEFT_OPTION),
            Some(KeyEvent::AriaUp)
        ));
    }

    #[test]
    fn the_aria_key_carries_the_modifiers_held_with_it() {
        assert!(matches!(
            hardware(AriaKey::RightOption, RIGHT_OPTION_KEY, RIGHT_OPTION | SHIFT | COMMAND),
            Some(KeyEvent::AriaDown { held }) if held == Modifiers::SHIFT | Modifiers::COMMAND
        ));
    }

    #[test]
    fn option_and_fn_are_not_watched_modifiers() {
        assert!(matches!(
            hardware(
                AriaKey::RightOption,
                LEFT_OPTION_KEY,
                LEFT_OPTION | SECONDARY_FN
            ),
            Some(KeyEvent::Modifiers {
                held: Modifiers::NONE
            })
        ));
    }

    #[test]
    fn a_synthetic_modifier_is_dropped() {
        let posted_shift = translate(
            AriaKey::RightOption,
            LEFT_SHIFT_KEY,
            RIGHT_OPTION | SHIFT,
            true,
        );
        assert!(posted_shift.is_none());
    }

    const KEYBOARD_PAGE: i64 = 0x7_0000_0000;
    const CAPS_LOCK: i64 = KEYBOARD_PAGE | 0x39;
    const LEFT_CONTROL: i64 = KEYBOARD_PAGE | 0xE0;
    const LEFT_OPTION_USAGE: i64 = KEYBOARD_PAGE | 0xE2;
    const RIGHT_CONTROL: i64 = KEYBOARD_PAGE | 0xE4;
    const RIGHT_OPTION_USAGE: i64 = KEYBOARD_PAGE | 0xE6;
    const RIGHT_COMMAND: i64 = KEYBOARD_PAGE | 0xE7;

    #[test]
    fn a_keyboard_with_no_remaps_keeps_both_keys() {
        assert!(!remaps_away(&[], AriaKey::RightOption));
        assert!(!remaps_away(&[], AriaKey::Fn));
    }

    #[test]
    fn mapping_keys_onto_option_or_option_onto_itself_keeps_it() {
        let keyboards = [vec![
            (RIGHT_CONTROL, RIGHT_OPTION_USAGE),
            (RIGHT_OPTION_USAGE, RIGHT_OPTION_USAGE),
            (CAPS_LOCK, RIGHT_CONTROL),
            (LEFT_CONTROL, LEFT_OPTION_USAGE),
            (LEFT_OPTION_USAGE, LEFT_OPTION_USAGE),
        ]];
        assert!(!remaps_away(&keyboards, AriaKey::RightOption));
    }

    #[test]
    fn an_option_and_command_swap_still_sends_right_option() {
        let keyboards = [vec![
            (RIGHT_OPTION_USAGE, RIGHT_COMMAND),
            (RIGHT_COMMAND, RIGHT_OPTION_USAGE),
        ]];
        assert!(!remaps_away(&keyboards, AriaKey::RightOption));
    }

    #[test]
    fn one_keyboard_that_sends_right_option_away_is_enough() {
        let keyboards = [
            vec![(RIGHT_OPTION_USAGE, RIGHT_OPTION_USAGE)],
            vec![(RIGHT_OPTION_USAGE, CAPS_LOCK)],
        ];
        assert!(remaps_away(&keyboards, AriaKey::RightOption));
    }

    #[test]
    fn option_turned_into_caps_lock_takes_right_option_away() {
        let keyboards = [vec![
            (LEFT_OPTION_USAGE, CAPS_LOCK),
            (RIGHT_OPTION_USAGE, CAPS_LOCK),
        ]];
        assert!(remaps_away(&keyboards, AriaKey::RightOption));
        assert!(!remaps_away(&keyboards, AriaKey::Fn));
    }

    #[test]
    fn fn_turned_into_control_takes_fn_away() {
        let keyboards = [vec![(AriaKey::Fn.hid_usage(), LEFT_CONTROL)]];
        assert!(remaps_away(&keyboards, AriaKey::Fn));
        assert!(!remaps_away(&keyboards, AriaKey::RightOption));
    }

    #[test]
    fn the_fn_binding_is_honored() {
        let key = AriaKey::Fn;
        assert!(matches!(
            hardware(key, FN_KEY, SECONDARY_FN),
            Some(KeyEvent::AriaDown { .. })
        ));
        assert!(matches!(hardware(key, FN_KEY, 0), Some(KeyEvent::AriaUp)));
        assert!(matches!(
            hardware(key, RIGHT_OPTION_KEY, RIGHT_OPTION),
            Some(KeyEvent::Modifiers { .. })
        ));
    }
}
