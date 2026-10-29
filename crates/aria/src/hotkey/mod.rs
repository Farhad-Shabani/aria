//! The Aria key: one bare modifier, read as a tap, a hold, or a combo with
//! Shift or Command.
//!
//! Bare modifiers are exactly what hotkey-registration APIs cannot capture,
//! so each platform watches the raw key event stream instead.
//!
//! Linux has no listener, so the press grammar below is unreachable there
//! while its tests still run.
#![cfg_attr(target_os = "linux", allow(dead_code))]

use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub(crate) use linux::LinuxHotkeyListener as PlatformHotkeyListener;
#[cfg(target_os = "macos")]
pub(crate) use macos::MacOsHotkeyListener as PlatformHotkeyListener;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("no hotkey listener exists for this target");

/// A press held this long with no other input is a hold; released sooner,
/// it is a tap.
const HOLD_THRESHOLD: Duration = Duration::from_millis(500);

// Generic flag masks, so either side's key counts.
/// kCGEventFlagMaskShift
const SHIFT: u64 = 0x0002_0000;
/// kCGEventFlagMaskControl
const CONTROL: u64 = 0x0004_0000;
/// kCGEventFlagMaskCommand
const COMMAND: u64 = 0x0010_0000;
/// The modifiers that can join a press. Control makes no combo; it is
/// watched so that it voids one. Option and Fn are not watched, so they
/// neither join nor void.
const JOINING: u64 = SHIFT | CONTROL | COMMAND;

/// The Aria key: the bare modifier the daemon watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyKey {
    RightOption,
    Fn,
}

impl HotkeyKey {
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
}

/// The Aria key held with Shift or Command alone. What a combo runs is the
/// client's choice, read off the preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combo {
    Shift,
    Command,
}

impl Combo {
    /// None for Control, or for two modifiers at once.
    fn from_modifiers(modifiers: u64) -> Option<Self> {
        match modifiers {
            SHIFT => Some(Self::Shift),
            COMMAND => Some(Self::Command),
            _ => None,
        }
    }
}

/// One monitored input event.
#[derive(Debug, Clone, Copy)]
pub(crate) enum KeyEvent {
    FlagsChanged {
        key_code: i64,
        flags: u64,
        /// Posted by software rather than a keyboard. Only a hardware
        /// modifier can join a press.
        synthetic: bool,
    },
    /// A hardware key going down; synthetic keystrokes are filtered out by
    /// the platform listener.
    KeyDown,
    MouseDown,
}

/// What the key's press stream means. Other input while the key is down
/// makes the press a shortcut, never a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Press {
    /// A clean press shorter than the hold threshold.
    Tap,
    /// The key has been held past the threshold with no other input.
    /// Exactly one of `HoldEnded` and `HoldCancelled` follows.
    HoldStarted,
    /// The held key came up.
    HoldEnded,
    /// Other input arrived, or the release was lost, during a hold.
    HoldCancelled,
    /// The key came up after a combo, with no other input in between.
    Combo(Combo),
}

#[derive(Debug, Clone, Copy)]
enum State {
    Up,
    /// A clean press: a tap, or a hold once the threshold passes.
    Down {
        since: Instant,
    },
    /// A modifier joined before the threshold, so the press can no longer
    /// become a hold. `modifiers` gathers every one that joins.
    Joined {
        modifiers: u64,
    },
    Holding,
}

/// Press grammar over the raw key stream. Time passes for it only through
/// `tick`, so a hold starts when the caller looks, never sooner.
pub(crate) struct PressDetector {
    key: HotkeyKey,
    state: State,
}

impl PressDetector {
    pub(crate) fn new(key: HotkeyKey) -> Self {
        Self {
            key,
            state: State::Up,
        }
    }

    /// Feeds one monitored event, stamped with when it arrived.
    pub(crate) fn handle(&mut self, event: KeyEvent, at: Instant) -> Option<Press> {
        match event {
            KeyEvent::FlagsChanged {
                key_code,
                flags,
                synthetic,
            } => {
                if key_code != self.key.key_code() {
                    if !synthetic {
                        self.join(flags, at);
                    }
                    return None;
                }
                if self.key.is_down(flags) {
                    self.press(flags, at)
                } else {
                    self.release(at)
                }
            }
            KeyEvent::KeyDown | KeyEvent::MouseDown => self.other_input(event),
        }
    }

    /// Promotes a long enough press to a hold. Call whenever `hold_deadline`
    /// has passed.
    pub(crate) fn tick(&mut self, at: Instant) -> Option<Press> {
        match self.state {
            State::Down { since } if at.duration_since(since) >= HOLD_THRESHOLD => {
                self.state = State::Holding;
                Some(Press::HoldStarted)
            }
            State::Up | State::Down { .. } | State::Joined { .. } | State::Holding => None,
        }
    }

    /// When the current press becomes a hold; `None` unless one is pending.
    pub(crate) fn hold_deadline(&self) -> Option<Instant> {
        match self.state {
            State::Down { since } => Some(since + HOLD_THRESHOLD),
            State::Up | State::Joined { .. } | State::Holding => None,
        }
    }

    /// Forgets the key's state, for when the platform may have dropped its
    /// release. A hold in progress is cancelled rather than left running.
    pub(crate) fn reset(&mut self) -> Option<Press> {
        let lost_hold = matches!(self.state, State::Holding);
        self.state = State::Up;
        lost_hold.then_some(Press::HoldCancelled)
    }

    /// A down edge while already down means the release was lost. A
    /// modifier already held joins the press, so either order makes a combo.
    fn press(&mut self, flags: u64, at: Instant) -> Option<Press> {
        let lost_hold = matches!(self.state, State::Holding);
        self.state = match flags & JOINING {
            0 => State::Down { since: at },
            modifiers => State::Joined { modifiers },
        };
        lost_hold.then_some(Press::HoldCancelled)
    }

    /// A modifier pressed after the threshold leaves a clean press to become
    /// a hold, even when `tick` has not looked yet. Once joined, every later
    /// modifier counts, so a second one voids the combo.
    fn join(&mut self, flags: u64, at: Instant) {
        let joining = flags & JOINING;
        match &mut self.state {
            State::Down { since } if joining != 0 && at.duration_since(*since) < HOLD_THRESHOLD => {
                self.state = State::Joined { modifiers: joining };
            }
            State::Joined { modifiers } => *modifiers |= joining,
            State::Up | State::Down { .. } | State::Holding => {}
        }
    }

    fn release(&mut self, at: Instant) -> Option<Press> {
        let press = match self.state {
            State::Down { since } if at.duration_since(since) < HOLD_THRESHOLD => Some(Press::Tap),
            State::Down { .. } => {
                // The press outgrew a tap before `tick` looked, so no hold
                // began; there is nothing to end.
                tracing::info!(
                    trigger = "hotkey",
                    "press dropped: released as a hold began"
                );
                None
            }
            State::Joined { modifiers } => {
                let combo = Combo::from_modifiers(modifiers);
                if combo.is_none() {
                    tracing::info!(
                        trigger = "hotkey",
                        modifiers,
                        "press voided: the modifiers held make no combo"
                    );
                }
                combo.map(Press::Combo)
            }
            State::Holding => Some(Press::HoldEnded),
            State::Up => None,
        };
        self.state = State::Up;
        press
    }

    /// Other input while the key is down spends the press: it was a
    /// shortcut, not a trigger.
    fn other_input(&mut self, event: KeyEvent) -> Option<Press> {
        let press = match self.state {
            State::Down { .. } | State::Joined { .. } => None,
            State::Holding => Some(Press::HoldCancelled),
            State::Up => return None,
        };
        self.state = State::Up;
        tracing::info!(
            trigger = "hotkey",
            ?event,
            "press voided: other input while the hotkey was held"
        );
        press
    }
}

/// What a platform listener reports while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListenerEvent {
    /// The key is being watched.
    Armed,
    /// The platform refused the watch until the user grants a permission.
    AwaitingPermission,
    Press(Press),
}

/// Why a platform listener returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListenerExit {
    Shutdown,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Unsupported,
}

#[cfg(test)]
mod tests {
    use super::*;

    const RIGHT_OPTION: u64 = 0x40;
    const LEFT_OPTION: u64 = 0x0008_0020;
    const SECONDARY_FN: u64 = 0x0080_0000;
    const LEFT_SHIFT_KEY: i64 = 56;
    const LEFT_COMMAND_KEY: i64 = 55;
    const CONTROL_KEY: i64 = 59;
    const A_TAP: Duration = Duration::from_millis(120);

    /// A detector plus a clock the test advances by hand.
    struct Keyboard {
        detector: PressDetector,
        now: Instant,
    }

    impl Keyboard {
        fn new(key: HotkeyKey) -> Self {
            Self {
                detector: PressDetector::new(key),
                now: Instant::now(),
            }
        }

        fn flags(&mut self, key_code: i64, flags: u64) -> Option<Press> {
            let event = KeyEvent::FlagsChanged {
                key_code,
                flags,
                synthetic: false,
            };
            self.detector.handle(event, self.now)
        }

        fn down(&mut self, extra: u64) -> Option<Press> {
            self.flags(61, RIGHT_OPTION | extra)
        }

        fn up_after(&mut self, held: Duration, extra: u64) -> Option<Press> {
            self.now += held;
            self.flags(61, extra)
        }

        fn tick_after(&mut self, held: Duration) -> Option<Press> {
            self.now += held;
            self.detector.tick(self.now)
        }

        fn other_key(&mut self) -> Option<Press> {
            self.detector.handle(KeyEvent::KeyDown, self.now)
        }

        fn mouse(&mut self) -> Option<Press> {
            self.detector.handle(KeyEvent::MouseDown, self.now)
        }
    }

    #[test]
    fn a_short_press_is_a_tap() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        assert_eq!(keyboard.down(0), None);
        assert_eq!(keyboard.tick_after(A_TAP), None);
        assert_eq!(keyboard.up_after(Duration::ZERO, 0), Some(Press::Tap));
    }

    #[test]
    fn a_press_becomes_a_hold_at_the_threshold_and_ends_on_release() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        assert_eq!(keyboard.down(0), None);
        assert_eq!(
            keyboard.detector.hold_deadline(),
            Some(keyboard.now + HOLD_THRESHOLD)
        );
        assert_eq!(
            keyboard.tick_after(HOLD_THRESHOLD),
            Some(Press::HoldStarted)
        );
        assert_eq!(keyboard.detector.hold_deadline(), None);
        assert_eq!(keyboard.tick_after(A_TAP), None);
        assert_eq!(keyboard.up_after(A_TAP, 0), Some(Press::HoldEnded));
    }

    #[test]
    fn a_release_past_the_threshold_before_any_tick_reports_nothing() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        assert_eq!(keyboard.up_after(HOLD_THRESHOLD, 0), None);
    }

    #[test]
    fn other_input_before_the_threshold_spends_the_press_silently() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        assert_eq!(keyboard.other_key(), None);
        assert_eq!(keyboard.detector.hold_deadline(), None);
        assert_eq!(keyboard.tick_after(HOLD_THRESHOLD), None);
        assert_eq!(keyboard.up_after(A_TAP, 0), None);
    }

    #[test]
    fn other_input_during_a_hold_cancels_it() {
        for other_input in [Keyboard::other_key, Keyboard::mouse] {
            let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
            keyboard.down(0);
            keyboard.tick_after(HOLD_THRESHOLD);
            assert_eq!(other_input(&mut keyboard), Some(Press::HoldCancelled));
            assert_eq!(keyboard.up_after(A_TAP, 0), None);
        }
    }

    #[test]
    fn a_lost_release_cancels_the_hold_and_starts_a_fresh_press() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        keyboard.tick_after(HOLD_THRESHOLD);
        assert_eq!(keyboard.down(0), Some(Press::HoldCancelled));
        assert_eq!(keyboard.up_after(A_TAP, 0), Some(Press::Tap));
    }

    #[test]
    fn a_reset_cancels_a_hold_and_forgets_a_press() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        keyboard.tick_after(HOLD_THRESHOLD);
        assert_eq!(keyboard.detector.reset(), Some(Press::HoldCancelled));
        assert_eq!(keyboard.up_after(A_TAP, 0), None);
        keyboard.down(0);
        assert_eq!(keyboard.detector.reset(), None);
        assert_eq!(keyboard.detector.hold_deadline(), None);
    }

    #[test]
    fn a_spent_press_does_not_poison_the_next_one() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        keyboard.other_key();
        keyboard.up_after(A_TAP, 0);
        assert_eq!(keyboard.down(0), None);
        assert_eq!(keyboard.up_after(A_TAP, 0), Some(Press::Tap));
    }

    #[test]
    fn left_option_held_across_the_release_still_counts() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        assert_eq!(keyboard.down(LEFT_OPTION), None);
        assert_eq!(keyboard.up_after(A_TAP, LEFT_OPTION), Some(Press::Tap));
    }

    #[test]
    fn other_modifier_keys_are_ignored() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        for (key_code, flags) in [(58, LEFT_OPTION), (56, 0x0002_0000)] {
            assert_eq!(keyboard.flags(key_code, flags), None);
            assert_eq!(keyboard.flags(key_code, 0), None);
        }
        assert_eq!(keyboard.detector.hold_deadline(), None);
    }

    #[test]
    fn input_while_the_key_is_up_is_ignored() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        assert_eq!(keyboard.other_key(), None);
        assert_eq!(keyboard.mouse(), None);
        assert_eq!(keyboard.down(0), None);
        assert_eq!(keyboard.up_after(A_TAP, 0), Some(Press::Tap));
    }

    #[test]
    fn a_modifier_tapped_during_the_press_makes_its_combo() {
        for (key_code, mask, combo) in [
            (LEFT_SHIFT_KEY, SHIFT, Combo::Shift),
            (LEFT_COMMAND_KEY, COMMAND, Combo::Command),
        ] {
            let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
            keyboard.down(0);
            assert_eq!(keyboard.flags(key_code, RIGHT_OPTION | mask), None);
            assert_eq!(keyboard.flags(key_code, RIGHT_OPTION), None);
            assert_eq!(
                keyboard.up_after(A_TAP, 0),
                Some(Press::Combo(combo)),
                "{combo:?}"
            );
        }
    }

    #[test]
    fn a_modifier_held_before_the_press_makes_a_combo() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        assert_eq!(keyboard.down(SHIFT), None);
        assert_eq!(
            keyboard.up_after(A_TAP, SHIFT),
            Some(Press::Combo(Combo::Shift))
        );
    }

    #[test]
    fn a_combo_never_becomes_a_hold() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        keyboard.flags(LEFT_SHIFT_KEY, RIGHT_OPTION | SHIFT);
        assert_eq!(keyboard.detector.hold_deadline(), None);
        assert_eq!(keyboard.tick_after(HOLD_THRESHOLD * 2), None);
        assert_eq!(
            keyboard.up_after(A_TAP, 0),
            Some(Press::Combo(Combo::Shift))
        );
    }

    #[test]
    fn a_key_typed_during_a_combo_voids_it() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        keyboard.flags(LEFT_COMMAND_KEY, RIGHT_OPTION | COMMAND);
        assert_eq!(keyboard.other_key(), None);
        assert_eq!(keyboard.up_after(A_TAP, COMMAND), None);
    }

    #[test]
    fn a_second_modifier_voids_a_combo_even_past_the_threshold() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        keyboard.flags(LEFT_SHIFT_KEY, RIGHT_OPTION | SHIFT);
        keyboard.now += HOLD_THRESHOLD;
        keyboard.flags(CONTROL_KEY, RIGHT_OPTION | SHIFT | CONTROL);
        assert_eq!(keyboard.up_after(A_TAP, 0), None);
    }

    #[test]
    fn a_synthetic_modifier_does_not_join_the_press() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        let posted_shift = KeyEvent::FlagsChanged {
            key_code: LEFT_SHIFT_KEY,
            flags: RIGHT_OPTION | SHIFT,
            synthetic: true,
        };
        assert_eq!(keyboard.detector.handle(posted_shift, keyboard.now), None);
        assert_eq!(keyboard.up_after(A_TAP, 0), Some(Press::Tap));
    }

    #[test]
    fn control_or_two_modifiers_make_no_combo() {
        for held in [CONTROL, SHIFT | COMMAND, SHIFT | CONTROL] {
            let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
            keyboard.down(held);
            assert_eq!(keyboard.up_after(A_TAP, held), None, "{held:#x}");
        }
    }

    #[test]
    fn a_modifier_past_the_threshold_leaves_the_hold_to_start() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        keyboard.now += HOLD_THRESHOLD;
        assert_eq!(keyboard.flags(LEFT_SHIFT_KEY, RIGHT_OPTION | SHIFT), None);
        assert_eq!(
            keyboard.tick_after(Duration::ZERO),
            Some(Press::HoldStarted)
        );
        assert_eq!(keyboard.up_after(A_TAP, SHIFT), Some(Press::HoldEnded));
    }

    #[test]
    fn a_modifier_during_a_hold_is_ignored() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(0);
        keyboard.tick_after(HOLD_THRESHOLD);
        assert_eq!(keyboard.flags(LEFT_SHIFT_KEY, RIGHT_OPTION | SHIFT), None);
        assert_eq!(keyboard.up_after(A_TAP, SHIFT), Some(Press::HoldEnded));
    }

    #[test]
    fn a_reset_forgets_a_combo() {
        let mut keyboard = Keyboard::new(HotkeyKey::RightOption);
        keyboard.down(SHIFT);
        assert_eq!(keyboard.detector.reset(), None);
        assert_eq!(keyboard.up_after(A_TAP, 0), None);
    }

    #[test]
    fn the_fn_binding_is_honored() {
        let mut keyboard = Keyboard::new(HotkeyKey::Fn);
        assert_eq!(keyboard.flags(63, SECONDARY_FN), None);
        assert_eq!(keyboard.flags(63, 0), Some(Press::Tap));
    }
}
