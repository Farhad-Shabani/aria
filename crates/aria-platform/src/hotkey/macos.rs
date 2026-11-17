//! The macOS hotkey listener: a listen-only CGEventTap for modifiers and
//! clicks, plus an active one that sees every key. The active one keeps
//! Fn's macOS action from running while Fn is the Aria key, swallows a
//! Return or an Escape a dictation takes, and reports the typing that voids
//! the Aria key's press.
//!
//! Everything runs on the caller's thread. The tap callbacks only ever
//! execute inside [`CFRunLoop::run_in_mode`] on that same thread, so the
//! shared state needs interior mutability but no synchronization. Shutdown
//! and health checks happen between 100 ms run-loop slices.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, CFString};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapCallBack, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGPreflightListenEventAccess,
    CGPreflightPostEventAccess,
};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUserDefaults};

use super::{
    AriaKey, BareKey, KeyEvent, KeyRemap, ListenerEvent, Modifiers, Press, PressDetector, Taken,
    TapRefusal,
};
use crate::accessibility::{self, Checked, Trust};
use crate::delivery::RETURN_VIRTUAL_KEY;

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
/// kCGEventFlagMaskAlternate
const OPTION: u64 = 0x0008_0000;
/// NX_DEVICELALTKEYMASK
const LEFT_OPTION: u64 = 0x20;

const RETURN_KEY_CODE: i64 = RETURN_VIRTUAL_KEY as i64;
/// kVK_ANSI_KeypadEnter, which Fn+Return also sends on Apple keyboards.
const KEYPAD_ENTER_KEY_CODE: i64 = 0x4C;
/// kVK_Escape
const ESCAPE_KEY_CODE: i64 = 0x35;

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

/// One Return or Escape key event, as the tap sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyPhase {
    Down,
    Repeat,
    Up,
}

/// What the tap does with a Return or Escape key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyHandling {
    /// Ask whether the key is taken, and swallow it if so.
    Ask(BareKey),
    /// Swallow it: it repeats a taken key.
    Swallow,
    /// Swallow the taken key's release, which ends it.
    End,
    Pass,
}

/// The main Return or Escape with no modifier on it: Shift+Return adds a
/// line, and Command+Option+Escape is the system's. The Aria key held for
/// a hold doesn't count, though Right Option sets Option and Fn turns
/// Return into keypad Enter.
fn bare_key(key: AriaKey, key_code: i64, flags: u64) -> Option<BareKey> {
    let holding = key.is_down(flags);
    let option = if holding && key == AriaKey::RightOption {
        flags & LEFT_OPTION
    } else {
        flags & OPTION
    };
    if option != 0 || flags & (SHIFT | CONTROL | COMMAND) != 0 {
        return None;
    }
    match key_code {
        RETURN_KEY_CODE => Some(BareKey::Return),
        KEYPAD_ENTER_KEY_CODE if holding && key == AriaKey::Fn => Some(BareKey::Return),
        ESCAPE_KEY_CODE => Some(BareKey::Escape),
        _ => None,
    }
}

/// Typing while the Aria key is down voids its press. Not the keystrokes
/// streaming delivery posts, which carry its process id. Not Fn's own key.
/// Not a key taken to keep the press.
fn voids_the_press(key_code: i64, synthetic: bool, taken: Taken) -> bool {
    !synthetic && key_code != GLOBE_KEY_CODE && taken != Taken::KeepingPress
}

/// Only the person's own key ends a dictation: the Return delivery posts
/// must reach the app.
fn key_handling(
    phase: KeyPhase,
    bare: Option<BareKey>,
    synthetic: bool,
    swallowing: bool,
) -> KeyHandling {
    if synthetic {
        return KeyHandling::Pass;
    }
    match (phase, bare, swallowing) {
        (KeyPhase::Down, Some(key), _) => KeyHandling::Ask(key),
        (KeyPhase::Repeat, _, true) => KeyHandling::Swallow,
        (KeyPhase::Up, _, true) => KeyHandling::End,
        _ => KeyHandling::Pass,
    }
}

pub struct MacOsHotkeyListener;

/// Shared between `listen` and the tap callbacks, same thread only — the
/// callbacks run inside the run-loop slices `listen` drives.
struct TapState<'a> {
    key: AriaKey,
    take: &'a dyn Fn(BareKey) -> Taken,
    /// The key codes of the taken keys still down: their repeats and
    /// releases are swallowed too, and no other key's.
    taken_keys: RefCell<Vec<i64>>,
    detector: RefCell<PressDetector>,
    /// The live taps, so a callback can re-enable them on a disable event.
    taps: RefCell<Vec<CFRetained<CFMachPort>>>,
    /// Press edges seen this slice; drained by `watch`.
    presses: RefCell<Vec<Press>>,
}

impl<'a> TapState<'a> {
    fn new(key: AriaKey, take: &'a dyn Fn(BareKey) -> Taken) -> Self {
        Self {
            key,
            take,
            taken_keys: RefCell::new(Vec::new()),
            detector: RefCell::new(PressDetector::new()),
            taps: RefCell::new(Vec::new()),
            presses: RefCell::new(Vec::new()),
        }
    }

    /// What becomes of a Return, keypad Enter or Escape key event. A taken
    /// key's repeats and release are swallowed with it.
    fn handle(&self, event_type: CGEventType, event: &CGEvent) -> Taken {
        let key_code =
            CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
        let flags = CGEvent::flags(Some(event)).0;
        let synthetic =
            CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID) != 0;
        let bare = bare_key(self.key, key_code, flags);
        let swallowing = self.taken_keys.borrow().contains(&key_code);
        match key_handling(key_phase(event_type, event), bare, synthetic, swallowing) {
            KeyHandling::Ask(key) => {
                let taken = (self.take)(key);
                if taken != Taken::No && !swallowing {
                    self.taken_keys.borrow_mut().push(key_code);
                }
                taken
            }
            KeyHandling::Swallow => Taken::KeepingPress,
            KeyHandling::End => {
                self.taken_keys
                    .borrow_mut()
                    .retain(|taken| *taken != key_code);
                Taken::KeepingPress
            }
            KeyHandling::Pass => Taken::No,
        }
    }

    fn feed(&self, event: KeyEvent) {
        let press = self.detector.borrow_mut().handle(event, Instant::now());
        self.presses.borrow_mut().extend(press);
    }

    fn tick(&self) {
        let press = self.detector.borrow_mut().tick(Instant::now());
        self.presses.borrow_mut().extend(press);
    }

    fn reset(&self) {
        // A taken key's release may be among the lost events.
        self.taken_keys.borrow_mut().clear();
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
impl Drop for TapState<'_> {
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
    pub fn remap(key: AriaKey) -> Option<KeyRemap> {
        remap_of(&modifier_remaps(), key)
    }

    /// `take` runs inside the tap callback for each bare Return or Escape
    /// the person presses. Its answer says whether the key is swallowed, and
    /// whether it voids the Aria key's press.
    /// It must not block: every key event waits on it.
    pub fn listen(
        key: AriaKey,
        take: impl Fn(BareKey) -> Taken,
        events: impl Fn(ListenerEvent),
        should_stop: impl Fn() -> bool,
    ) {
        let state = TapState::new(key, &take);
        loop {
            if should_stop() {
                return;
            }
            if let Err(unarmed) = arm(&state) {
                if unarmed.needs_grant() {
                    Trust::request();
                }
                events(unarmed.into());
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

/// A keyboard remaps the Aria key when it sends the key away, even if it maps
/// another key onto it. The keyboard may lack that other key, as a MacBook
/// lacks Right Control. HID descriptors don't say which keys a keyboard has.
/// Of several such keyboards, the one with the fewest senders is the likeliest
/// to have a dead Aria key.
// ponytail: a list left by an unplugged keyboard still counts; match it
// against connected keyboards if that misleads anyone.
fn remap_of(keyboards: &[Vec<(i64, i64)>], key: AriaKey) -> Option<KeyRemap> {
    let aria = key.hid_usage();
    keyboards
        .iter()
        .filter_map(|remaps| {
            let &(_, destination) = remaps
                .iter()
                .find(|&&(source, destination)| source == aria && destination != aria)?;
            let senders = remaps
                .iter()
                .filter(|&&(source, destination)| source != aria && destination == aria)
                .map(|&(source, _)| key_name(source))
                .collect();
            Some(KeyRemap {
                key: key_name(aria),
                destination: key_name(destination),
                senders,
            })
        })
        .min_by_key(|remap| remap.senders.len())
}

fn key_name(usage: i64) -> &'static str {
    match usage {
        0x7_0000_0029 => "Escape",
        0x7_0000_0039 => "Caps Lock",
        0x7_0000_00E0 => "Left Control",
        0x7_0000_00E1 => "Left Shift",
        0x7_0000_00E2 => "Left Option",
        0x7_0000_00E3 => "Left Command",
        0x7_0000_00E4 => "Right Control",
        0x7_0000_00E5 => "Right Shift",
        0x7_0000_00E6 => "Right Option",
        0x7_0000_00E7 => "Right Command",
        0xFF_0000_0003 => "Fn",
        _ => "another key",
    }
}

/// Each keyboard's Modifier Keys remaps, as (source, destination) usages.
/// The standard search list includes the current host's global domain, where
/// the remaps live. Sorted by defaults key, so the order stays the same
/// between runs. The dictionary itself has no fixed order.
fn modifier_remaps() -> Vec<Vec<(i64, i64)>> {
    let (keys, values) = NSUserDefaults::standardUserDefaults()
        .dictionaryRepresentation()
        .to_vecs();
    let mut keyboards: Vec<_> = keys
        .iter()
        .map(|key| key.to_string())
        .zip(values)
        .filter(|(key, _)| key.starts_with(MODIFIER_MAPPING_PREFIX))
        .filter_map(|(key, remaps)| Some((key, remaps.downcast::<NSArray>().ok()?)))
        .collect();
    keyboards.sort_by(|(left, _), (right, _)| left.cmp(right));
    keyboards
        .into_iter()
        .map(|(_, remaps)| remaps.to_vec().into_iter().filter_map(remap).collect())
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

fn arm(state: &TapState<'_>) -> Result<(), Unarmed> {
    arm_with(Trust::check_now(), || {
        add_taps(state).map_err(|step| {
            state.disarm();
            TapRefusal {
                step: step.name(),
                listen_access: CGPreflightListenEventAccess(),
                post_access: CGPreflightPostEventAccess(),
            }
        })
    })?;
    state.enable_taps();
    tracing::info!(trigger = "hotkey", "event tap armed");
    Ok(())
}

/// Taps are made only for a trusted process, since a tap made before the
/// grant never recovers. Taps fail for an untrusted process too, and a stale
/// answer can miss a revoked grant. So only a fresh answer makes a failed tap
/// a refusal.
fn arm_with(checked: Checked, tap: impl FnOnce() -> Result<(), TapRefusal>) -> Result<(), Unarmed> {
    if !checked.trust().is_trusted() {
        return Err(Unarmed::AwaitingPermission);
    }
    tap().map_err(|refusal| {
        if checked.is_fresh() {
            Unarmed::TapRefused(refusal)
        } else {
            Unarmed::AwaitingPermission
        }
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Unarmed {
    AwaitingPermission,
    TapRefused(TapRefusal),
}

impl Unarmed {
    /// A refused tap's process already holds the grant.
    fn needs_grant(&self) -> bool {
        matches!(self, Self::AwaitingPermission)
    }
}

impl From<Unarmed> for ListenerEvent {
    fn from(unarmed: Unarmed) -> Self {
        match unarmed {
            Unarmed::AwaitingPermission => Self::AwaitingPermission,
            Unarmed::TapRefused(refusal) => Self::TapRefused(refusal),
        }
    }
}

#[derive(Clone, Copy)]
enum TapStep {
    Listener,
    Blocker,
    RunLoop,
    Source,
}

impl TapStep {
    fn name(self) -> &'static str {
        match self {
            Self::Listener => "listener tap",
            Self::Blocker => "blocker tap",
            Self::RunLoop => "run loop",
            Self::Source => "run loop source",
        }
    }
}

/// Each tap joins `state` as soon as it exists, so a later failure still
/// invalidates it.
fn add_taps(state: &TapState<'_>) -> Result<(), TapStep> {
    let listener = create_tap(
        state,
        CGEventTapOptions::ListenOnly,
        LISTENER_EVENTS,
        Some(listener_callback),
    )
    .ok_or(TapStep::Listener)?;
    state.taps.borrow_mut().push(listener);
    // An active tap holds each event until it returns, so this one takes key
    // events only. Revoking the grant stalls them until `watch`'s next trust
    // check disarms it, at most a REARM_INTERVAL later.
    let blocker = create_tap(
        state,
        CGEventTapOptions::Default,
        BLOCKER_EVENTS,
        Some(blocker_callback),
    )
    .ok_or(TapStep::Blocker)?;
    state.taps.borrow_mut().push(blocker);
    let run_loop = CFRunLoop::current().ok_or(TapStep::RunLoop)?;
    for tap in state.taps.borrow().iter() {
        let source = CFMachPort::new_run_loop_source(None, Some(tap), 0).ok_or(TapStep::Source)?;
        run_loop.add_source(Some(&source), Some(&default_mode()));
    }
    Ok(())
}

/// Drives the run loop and reports presses until shutdown or tap death.
/// Returns true when stopping for shutdown.
fn watch(
    state: &TapState<'_>,
    events: &impl Fn(ListenerEvent),
    should_stop: &impl Fn() -> bool,
) -> bool {
    let grant = GrantWatch::default();
    thread::scope(|scope| {
        let checker = thread::Builder::new()
            .name("aria-hotkey-grant".into())
            .spawn_scoped(scope, || grant.run());
        if let Err(error) = checker {
            tracing::error!(trigger = "hotkey", %error, "could not watch the grant; re-arming");
            // Nothing drives the run loop meanwhile, and every keystroke
            // would wait on the active tap.
            state.disarm();
            return wait(REARM_INTERVAL, should_stop);
        }
        // Dropped on unwind too: the scope waits for the checker to end.
        let _stop = CheckerStop(&grant.done);
        let stopped = drive(state, events, should_stop, &grant);
        // Before the scope waits on the checker: with nothing driving the
        // run loop, the active tap would hold every keystroke meanwhile.
        state.disarm();
        stopped
    })
}

/// Ends the grant checker when dropped.
struct CheckerStop<'a>(&'a AtomicBool);

impl Drop for CheckerStop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Checks the grant off the tap thread. The check waits on a subprocess,
/// and while the active tap exists every keystroke waits on the tap thread.
#[derive(Default)]
struct GrantWatch {
    revoked: AtomicBool,
    done: AtomicBool,
}

impl GrantWatch {
    fn run(&self) {
        while !wait(REARM_INTERVAL, &|| self.done.load(Ordering::Acquire)) {
            if !Trust::check_now().trust().is_trusted() {
                self.revoked.store(true, Ordering::Release);
                return;
            }
        }
    }
}

fn drive(
    state: &TapState<'_>,
    events: &impl Fn(ListenerEvent),
    should_stop: &impl Fn() -> bool,
    grant: &GrantWatch,
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
        // macOS leaves a running tap alive after the grant is revoked.
        if grant.revoked.load(Ordering::Acquire) {
            tracing::warn!(trigger = "hotkey", "the grant was revoked; disarming");
            return false;
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

const LISTENER_EVENTS: u64 = (1 << CGEventType::FlagsChanged.0 as u64)
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
    state: &TapState<'_>,
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
    let state = unsafe { &*user_info.cast::<TapState<'_>>() };
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
    let state = unsafe { &*user_info.cast::<TapState<'_>>() };
    // SAFETY: as in `listener_callback`.
    let event = unsafe { event.as_ref() };
    match event_type {
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
            tracing::warn!(trigger = "hotkey", "key blocker disabled; re-enabling");
            // Key downs, and a taken key's release, may be among the lost
            // events.
            state.reset();
            state.enable_taps();
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            let key_code =
                CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventKeycode);
            let taken = match key_code {
                GLOBE_KEY_CODE if state.key == AriaKey::Fn => Taken::KeepingPress,
                RETURN_KEY_CODE | KEYPAD_ENTER_KEY_CODE | ESCAPE_KEY_CODE => {
                    state.handle(event_type, event)
                }
                _ => Taken::No,
            };
            let synthetic =
                CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUnixProcessID)
                    != 0;
            if event_type == CGEventType::KeyDown && voids_the_press(key_code, synthetic, taken) {
                state.feed(KeyEvent::KeyDown);
            }
            if taken != Taken::No {
                return std::ptr::null_mut();
            }
        }
        _ => {}
    }
    std::ptr::from_ref(event).cast_mut()
}

fn key_phase(event_type: CGEventType, event: &CGEvent) -> KeyPhase {
    if event_type == CGEventType::KeyUp {
        KeyPhase::Up
    } else if CGEvent::integer_value_field(Some(event), CGEventField::KeyboardEventAutorepeat) != 0
    {
        KeyPhase::Repeat
    } else {
        KeyPhase::Down
    }
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

    #[test]
    fn only_a_key_taken_to_keep_the_press_leaves_it_whole() {
        assert!(!voids_the_press(
            RETURN_KEY_CODE,
            false,
            Taken::KeepingPress
        ));
        assert!(voids_the_press(ESCAPE_KEY_CODE, false, Taken::VoidingPress));
        assert!(voids_the_press(ESCAPE_KEY_CODE, false, Taken::No));
        assert!(!voids_the_press(ESCAPE_KEY_CODE, true, Taken::No));
        assert!(!voids_the_press(GLOBE_KEY_CODE, false, Taken::No));
    }

    #[test]
    fn only_a_return_or_escape_with_no_modifier_is_bare() {
        const RIGHT_OPTION_HELD: u64 = OPTION | 0x40;
        const FN_HELD: u64 = 0x0080_0000;
        const RETURN: Option<BareKey> = Some(BareKey::Return);
        const ESCAPE: Option<BareKey> = Some(BareKey::Escape);
        let cases = [
            (AriaKey::Fn, RETURN_KEY_CODE, 0, RETURN),
            (AriaKey::Fn, RETURN_KEY_CODE, SHIFT, None),
            (AriaKey::Fn, RETURN_KEY_CODE, OPTION, None),
            (AriaKey::Fn, RETURN_KEY_CODE, COMMAND, None),
            (AriaKey::Fn, RETURN_KEY_CODE, CONTROL, None),
            (AriaKey::Fn, KEYPAD_ENTER_KEY_CODE, 0, None),
            // A hold: the Aria key is down, and only it.
            (AriaKey::Fn, KEYPAD_ENTER_KEY_CODE, FN_HELD, RETURN),
            (
                AriaKey::RightOption,
                RETURN_KEY_CODE,
                RIGHT_OPTION_HELD,
                RETURN,
            ),
            (
                AriaKey::RightOption,
                RETURN_KEY_CODE,
                RIGHT_OPTION_HELD | LEFT_OPTION,
                None,
            ),
            (
                AriaKey::RightOption,
                KEYPAD_ENTER_KEY_CODE,
                RIGHT_OPTION_HELD,
                None,
            ),
            (AriaKey::Fn, ESCAPE_KEY_CODE, 0, ESCAPE),
            (AriaKey::Fn, ESCAPE_KEY_CODE, FN_HELD, ESCAPE),
            (AriaKey::Fn, ESCAPE_KEY_CODE, SHIFT, None),
            // Force Quit.
            (AriaKey::Fn, ESCAPE_KEY_CODE, COMMAND | OPTION, None),
            (
                AriaKey::RightOption,
                ESCAPE_KEY_CODE,
                RIGHT_OPTION_HELD,
                ESCAPE,
            ),
            (
                AriaKey::RightOption,
                ESCAPE_KEY_CODE,
                RIGHT_OPTION_HELD | LEFT_OPTION,
                None,
            ),
        ];
        for (key, key_code, flags, bare) in cases {
            assert_eq!(
                bare_key(key, key_code, flags),
                bare,
                "{key:?}, key code {key_code:#x}, flags {flags:#x}"
            );
        }
    }

    #[test]
    fn only_the_persons_bare_key_is_asked_about() {
        for key in [BareKey::Return, BareKey::Escape] {
            let cases = [
                (Some(key), false, KeyHandling::Ask(key)),
                (None, false, KeyHandling::Pass),
                (Some(key), true, KeyHandling::Pass),
            ];
            for (bare, synthetic, action) in cases {
                assert_eq!(
                    key_handling(KeyPhase::Down, bare, synthetic, false),
                    action,
                    "bare {bare:?}, synthetic {synthetic}"
                );
            }
        }
    }

    #[test]
    fn a_panic_while_driving_still_ends_the_grant_checker() {
        let grant = GrantWatch::default();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            thread::scope(|scope| {
                scope.spawn(|| grant.run());
                let _stop = CheckerStop(&grant.done);
                panic!("drive panicked");
            })
        }));
        assert!(unwound.is_err());
    }

    /// Feeds `state` one hardware key event with no modifier; true when
    /// the tap swallows it.
    fn swallowed(state: &TapState<'_>, key_code: i64, down: bool) -> bool {
        let event =
            CGEvent::new_keyboard_event(None, key_code as u16, down).expect("keyboard event");
        CGEvent::set_flags(Some(&event), objc2_core_graphics::CGEventFlags(0));
        CGEvent::set_integer_value_field(Some(&event), CGEventField::EventSourceUnixProcessID, 0);
        let event_type = if down {
            CGEventType::KeyDown
        } else {
            CGEventType::KeyUp
        };
        state.handle(event_type, &event) != Taken::No
    }

    #[test]
    fn each_taken_key_is_swallowed_until_its_own_release() {
        let take = |_| Taken::KeepingPress;
        let state = TapState::new(AriaKey::RightOption, &take);

        assert!(swallowed(&state, RETURN_KEY_CODE, true), "Return is taken");
        assert!(swallowed(&state, ESCAPE_KEY_CODE, true), "then Escape");

        assert!(swallowed(&state, RETURN_KEY_CODE, false));
        assert!(
            swallowed(&state, ESCAPE_KEY_CODE, false),
            "Return's release left Escape taken"
        );
    }

    #[test]
    fn a_key_nothing_takes_reaches_the_app() {
        let take = |_| Taken::No;
        let state = TapState::new(AriaKey::RightOption, &take);

        assert!(!swallowed(&state, ESCAPE_KEY_CODE, true));
        assert!(!swallowed(&state, ESCAPE_KEY_CODE, false));
    }

    #[test]
    fn only_the_taken_keys_release_is_swallowed() {
        let take = |_| Taken::KeepingPress;
        let state = TapState::new(AriaKey::RightOption, &take);

        assert!(
            swallowed(&state, RETURN_KEY_CODE, true),
            "a bare Return is taken"
        );
        assert!(
            !swallowed(&state, KEYPAD_ENTER_KEY_CODE, true),
            "keypad Enter is not bare"
        );
        assert!(
            !swallowed(&state, KEYPAD_ENTER_KEY_CODE, false),
            "its release is its own"
        );
        assert!(
            swallowed(&state, RETURN_KEY_CODE, false),
            "the taken release"
        );
    }

    #[test]
    fn a_taken_key_is_swallowed_until_released() {
        let bare = Some(BareKey::Return);
        assert_eq!(
            key_handling(KeyPhase::Repeat, bare, false, true),
            KeyHandling::Swallow
        );
        assert_eq!(
            key_handling(KeyPhase::Up, bare, false, true),
            KeyHandling::End
        );
    }

    #[test]
    fn an_untaken_key_repeats_and_releases_untouched() {
        let bare = Some(BareKey::Return);
        assert_eq!(
            key_handling(KeyPhase::Repeat, bare, false, false),
            KeyHandling::Pass
        );
        assert_eq!(
            key_handling(KeyPhase::Up, bare, false, false),
            KeyHandling::Pass
        );
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
        assert_eq!(remap_of(&[], AriaKey::RightOption), None);
        assert_eq!(remap_of(&[], AriaKey::Fn), None);
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
        assert_eq!(remap_of(&keyboards, AriaKey::RightOption), None);
    }

    const REFUSAL: TapRefusal = TapRefusal {
        step: "blocker tap",
        listen_access: false,
        post_access: true,
    };

    #[test]
    fn a_missing_grant_awaits_permission_without_making_taps() {
        let unarmed = arm_with(Checked::fresh(Trust::Untrusted), || {
            panic!("no tap may be made before the grant")
        });

        assert_eq!(unarmed, Err(Unarmed::AwaitingPermission));
    }

    #[test]
    fn a_failed_tap_under_a_fresh_trust_answer_is_a_refusal() {
        let unarmed = arm_with(Checked::fresh(Trust::Trusted), || Err(REFUSAL));

        assert_eq!(unarmed, Err(Unarmed::TapRefused(REFUSAL)));
    }

    #[test]
    fn a_failed_tap_under_a_stale_trust_answer_awaits_permission() {
        let unarmed = arm_with(Checked::stale(Trust::Trusted), || Err(REFUSAL));

        assert_eq!(unarmed, Err(Unarmed::AwaitingPermission));
    }

    #[test]
    fn a_stale_trust_answer_still_arms() {
        assert_eq!(arm_with(Checked::stale(Trust::Trusted), || Ok(())), Ok(()));
    }

    #[test]
    fn only_a_missing_grant_needs_one() {
        assert!(Unarmed::AwaitingPermission.needs_grant());
        assert!(!Unarmed::TapRefused(REFUSAL).needs_grant());
    }

    fn key_remap(
        key: &'static str,
        destination: &'static str,
        senders: &[&'static str],
    ) -> KeyRemap {
        KeyRemap {
            key,
            destination,
            senders: senders.to_vec(),
        }
    }

    #[test]
    fn option_sent_away_counts_though_a_missing_key_sends_it() {
        // A MacBook has no Right Control, so no key on it sends Right Option.
        let keyboards = [vec![
            (LEFT_CONTROL, LEFT_OPTION_USAGE),
            (LEFT_OPTION_USAGE, CAPS_LOCK),
            (RIGHT_CONTROL, RIGHT_OPTION_USAGE),
            (RIGHT_OPTION_USAGE, CAPS_LOCK),
            (CAPS_LOCK, RIGHT_CONTROL),
        ]];
        assert_eq!(
            remap_of(&keyboards, AriaKey::RightOption),
            Some(key_remap("Right Option", "Caps Lock", &["Right Control"]))
        );
    }

    #[test]
    fn an_option_and_command_swap_names_command_as_the_way_in() {
        let keyboards = [vec![
            (RIGHT_OPTION_USAGE, RIGHT_COMMAND),
            (RIGHT_COMMAND, RIGHT_OPTION_USAGE),
        ]];
        assert_eq!(
            remap_of(&keyboards, AriaKey::RightOption),
            Some(key_remap(
                "Right Option",
                "Right Command",
                &["Right Command"]
            ))
        );
    }

    #[test]
    fn one_keyboard_that_sends_right_option_away_is_enough() {
        let keyboards = [
            vec![(RIGHT_OPTION_USAGE, RIGHT_OPTION_USAGE)],
            vec![(RIGHT_OPTION_USAGE, CAPS_LOCK)],
        ];
        assert_eq!(
            remap_of(&keyboards, AriaKey::RightOption),
            Some(key_remap("Right Option", "Caps Lock", &[]))
        );
    }

    #[test]
    fn the_keyboard_with_no_sender_is_reported_over_a_swap() {
        let keyboards = [
            vec![
                (RIGHT_OPTION_USAGE, RIGHT_COMMAND),
                (RIGHT_COMMAND, RIGHT_OPTION_USAGE),
            ],
            vec![(RIGHT_OPTION_USAGE, CAPS_LOCK)],
        ];
        assert_eq!(
            remap_of(&keyboards, AriaKey::RightOption),
            Some(key_remap("Right Option", "Caps Lock", &[]))
        );
    }

    #[test]
    fn fn_turned_into_control_takes_fn_away() {
        let keyboards = [vec![(AriaKey::Fn.hid_usage(), LEFT_CONTROL)]];
        assert_eq!(
            remap_of(&keyboards, AriaKey::Fn),
            Some(key_remap("Fn", "Left Control", &[]))
        );
        assert_eq!(remap_of(&keyboards, AriaKey::RightOption), None);
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
