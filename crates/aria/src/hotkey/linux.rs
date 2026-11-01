//! The Linux hotkey listener. It reads keyboards and pointers straight from
//! evdev, below the display server, so it works under Wayland and X11 alike.
//!
//! Devices are only read, never grabbed: every application still gets every
//! key. Reading needs access to the device nodes, which a udev rule grants
//! the active session's user; until then the listener waits.
//!
//! Aria's own paste goes through the display server, never evdev, so it
//! cannot reach this listener. Virtual devices are kept: remappers such as
//! keyd send the real keyboard's keys out through one.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Access, Mode, OFlags, access, open};
use rustix::ioctl::{Getter, Setter, ioctl, opcode};
use rustix::time::{ClockId, clock_gettime};

use super::{AriaKey, KeyEvent, ListenerEvent, Modifiers, Press, PressDetector};

#[cfg(not(target_pointer_width = "64"))]
compile_error!("`EVENT_SIZE` assumes the kernel's 64-bit `struct input_event`");

const DEVICE_DIRECTORY: &str = "/dev/input";
const CLASS_DIRECTORY: &str = "/sys/class/input";
/// How often to open new devices and drop ones the user may no longer read.
/// A fast user switch revokes access, but an open descriptor keeps reading.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);
/// One wait on the devices: bounds shutdown latency, not press latency. A
/// pending hold shortens it.
const POLL_SLICE: Duration = Duration::from_millis(100);
/// `struct input_event` on a 64-bit kernel: a 16-byte timestamp, then the
/// type, code and value.
const EVENT_SIZE: usize = 24;
const EVENTS_PER_READ: usize = 64;

// linux/input-event-codes.h
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_CAPSLOCK: u16 = 58;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;
const KEY_FN: u16 = 0x1d0;
const BTN_LEFT: u16 = 0x110;
const BTN_TASK: u16 = 0x117;
/// Touchpad and tablet contacts: touching is not clicking.
const BTN_DIGI_FIRST: u16 = 0x140;
const BTN_DIGI_LAST: u16 = 0x14f;
const KEY_RELEASED: i32 = 0;
const KEY_PRESSED: i32 = 1;

const MODIFIER_KEYS: [(u16, Modifiers); 6] = [
    (KEY_LEFTSHIFT, Modifiers::SHIFT),
    (KEY_RIGHTSHIFT, Modifiers::SHIFT),
    (KEY_LEFTCTRL, Modifiers::CONTROL),
    (KEY_RIGHTCTRL, Modifiers::CONTROL),
    (KEY_LEFTMETA, Modifiers::COMMAND),
    (KEY_RIGHTMETA, Modifiers::COMMAND),
];

/// The udev rule that grants access, as it ships beside the binary.
const INPUT_RULE: &str = "share/udev/70-aria-input.rules";
const RULES_DIRECTORY: &str = "/etc/udev/rules.d";
const APPLY_RULE: &str = "sudo udevadm trigger --subsystem-match=input --action=change";

impl AriaKey {
    /// `None` for a key the listener cannot watch.
    fn key_code(self) -> Option<u16> {
        match self {
            Self::RightOption => Some(KEY_RIGHTALT),
            Self::Fn => None,
        }
    }
}

pub(crate) struct LinuxHotkeyListener;

impl LinuxHotkeyListener {
    pub(crate) const PERMISSION: &str = "access to your keyboard";

    pub(crate) fn unarmed_reason(key: AriaKey) -> String {
        let Some(aria_key) = key.key_code() else {
            return "the dictation hotkey is set to Fn, which Linux cannot see; \
                set `shortcuts.aria_key` to right_option, the Right Alt key"
                .to_owned();
        };
        match Devices::system(aria_key).probe() {
            Readiness::Denied => format!(
                "the dictation hotkey cannot read the keyboard; {}; it arms within ~2s",
                grant_steps()
            ),
            Readiness::NoKeyboard => {
                "the dictation hotkey found no keyboard with a Right Alt key".to_owned()
            }
            Readiness::Reading | Readiness::StateUnavailable => {
                "the keyboard is readable, so the hotkey should arm within ~2s; if it does \
                 not, see `journalctl --user -u aria-daemon`"
                    .to_owned()
            }
        }
    }

    pub(crate) fn listen(
        key: AriaKey,
        events: impl Fn(ListenerEvent),
        should_stop: impl Fn() -> bool,
    ) {
        let Some(aria_key) = key.key_code() else {
            tracing::warn!(trigger = "hotkey", ?key, "this key is invisible to Linux");
            events(ListenerEvent::KeyUnavailable);
            return wait_for_stop(&should_stop);
        };
        watch(&mut Devices::system(aria_key), &events, &should_stop);
    }
}

fn grant_steps() -> String {
    let shipped = std::env::current_exe()
        .and_then(fs::canonicalize)
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join(INPUT_RULE)))
        .filter(|rule| rule.is_file());
    match shipped {
        Some(rule) => format!(
            "run `sudo install -m 0644 {} {RULES_DIRECTORY}/` then `{APPLY_RULE}`",
            shell_quoted(&rule)
        ),
        None => format!(
            "install scripts/linux/70-aria-input.rules from the Aria source into \
             {RULES_DIRECTORY}, then run `{APPLY_RULE}`"
        ),
    }
}

fn shell_quoted(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

fn wait_for_stop(should_stop: &impl Fn() -> bool) {
    while !should_stop() {
        thread::sleep(POLL_SLICE);
    }
}

fn watch(devices: &mut Devices, events: &impl Fn(ListenerEvent), should_stop: &impl Fn() -> bool) {
    let mut detector = PressDetector::new();
    let mut reported = None;
    let mut next_scan = Instant::now();
    let report_press = |press: Option<Press>| {
        if let Some(press) = press {
            events(ListenerEvent::Press(press));
        }
    };
    while !should_stop() {
        let now = Instant::now();
        if now >= next_scan {
            next_scan = now + RESCAN_INTERVAL;
            let readiness = devices.rescan();
            if reported != Some(readiness) {
                reported = Some(readiness);
                report(readiness, devices.open.len(), events);
            }
        }
        let slice = detector.hold_deadline().map_or(POLL_SLICE, |deadline| {
            deadline.saturating_duration_since(now).min(POLL_SLICE)
        });
        // In stream order: a reset must not wipe out a press that came after
        // the loss, nor a press before it survive the reset.
        for input in devices.next_batch(slice) {
            let press = match input {
                DeviceInput::Key(event) => detector.handle(event, Instant::now()),
                DeviceInput::LostKeys => detector.reset(),
            };
            report_press(press);
        }
        report_press(detector.tick(Instant::now()));
    }
}

fn report(readiness: Readiness, devices: usize, events: &impl Fn(ListenerEvent)) {
    match readiness {
        Readiness::Reading => {
            tracing::info!(trigger = "hotkey", devices, "input devices armed");
            events(ListenerEvent::Armed);
        }
        Readiness::Denied => {
            tracing::info!(trigger = "hotkey", "no keyboard is readable");
            events(ListenerEvent::AwaitingPermission);
        }
        Readiness::NoKeyboard => {
            tracing::info!(trigger = "hotkey", "no keyboard has the key");
            events(ListenerEvent::KeyUnavailable);
        }
        Readiness::StateUnavailable => {
            tracing::warn!(trigger = "hotkey", "held keys are unreadable");
            events(ListenerEvent::KeyUnavailable);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Readiness {
    /// A keyboard with the key can be read.
    Reading,
    /// A keyboard with the key exists, but the user may not read it.
    Denied,
    NoKeyboard,
    /// A device opened, but its clock or held keys could not be set up.
    StateUnavailable,
}

/// One step of what the devices reported, in stream order.
#[derive(Debug, Clone, Copy)]
enum DeviceInput {
    Key(KeyEvent),
    /// Events were lost, or a keyboard went away, so a release may be gone.
    LostKeys,
}

struct Devices {
    class_directory: PathBuf,
    device_directory: PathBuf,
    aria_key: u16,
    open: Vec<Device>,
    state_unavailable: bool,
    /// A keyboard went away or a device failed to start; the next batch reports it.
    lost_keyboard: bool,
}

struct Device {
    node: PathBuf,
    file: File,
    kind: Kind,
    held: HeldKeys,
    stream: Stream,
    pending: Vec<InputEvent>,
}

/// Where a device's event stream stands after `SYN_DROPPED`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Live,
    /// Events were dropped; skip to the next report.
    Dropped,
    /// The report arrived, so the held keys can be read again.
    ReportSeen,
}

impl Devices {
    fn system(aria_key: u16) -> Self {
        Self::new(
            PathBuf::from(CLASS_DIRECTORY),
            PathBuf::from(DEVICE_DIRECTORY),
            aria_key,
        )
    }

    fn new(class_directory: PathBuf, device_directory: PathBuf, aria_key: u16) -> Self {
        Self {
            class_directory,
            device_directory,
            aria_key,
            open: Vec::new(),
            state_unavailable: false,
            lost_keyboard: false,
        }
    }

    /// What a rescan should find, judged by access checks alone.
    fn probe(&self) -> Readiness {
        let mut denied = false;
        for (name, kind) in candidates(&self.class_directory, self.aria_key) {
            if kind != Kind::Keyboard {
                continue;
            }
            match access(self.device_directory.join(name), Access::READ_OK) {
                Ok(()) => return Readiness::Reading,
                Err(error) => denied |= error == rustix::io::Errno::ACCESS,
            }
        }
        if denied {
            Readiness::Denied
        } else {
            Readiness::NoKeyboard
        }
    }

    fn rescan(&mut self) -> Readiness {
        self.recover(held_keys);
        self.rescan_with(initialize_device)
    }

    fn rescan_with(&mut self, initialize: impl Fn(&File) -> io::Result<HeldKeys>) -> Readiness {
        self.state_unavailable = false;
        let before = self.open.len();
        let mut lost_keyboard = self.lost_keyboard;
        self.open.retain(|device| {
            let readable = access(&device.node, Access::READ_OK).is_ok();
            if !readable {
                tracing::info!(trigger = "hotkey", node = %device.node.display(), "input device dropped");
                lost_keyboard |= device.kind == Kind::Keyboard;
            }
            readable
        });
        let mut denied = false;
        for (name, kind) in candidates(&self.class_directory, self.aria_key) {
            let node = self.device_directory.join(&name);
            if self.open.iter().any(|device| device.node == node) {
                continue;
            }
            match open_device(&node) {
                Ok(file) => match initialize(&file) {
                    Ok(held) => self.open.push(Device {
                        node,
                        file,
                        kind,
                        held,
                        stream: Stream::Live,
                        pending: Vec::new(),
                    }),
                    Err(error) => {
                        self.state_unavailable = true;
                        lost_keyboard = true;
                        tracing::warn!(trigger = "hotkey", node = %node.display(), %error, "input device state unavailable");
                    }
                },
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    denied |= kind == Kind::Keyboard;
                }
                Err(error) => {
                    tracing::warn!(trigger = "hotkey", node = %node.display(), %error, "input device did not open");
                }
            }
        }
        if self.open.len() != before {
            tracing::debug!(
                trigger = "hotkey",
                devices = self.open.len(),
                "input devices rescanned"
            );
        }
        self.lost_keyboard = lost_keyboard;
        let recovery_failed = self
            .open
            .iter()
            .any(|device| device.stream == Stream::ReportSeen);
        if self.state_unavailable || recovery_failed {
            Readiness::StateUnavailable
        } else if self.open.iter().any(|device| device.kind == Kind::Keyboard) {
            Readiness::Reading
        } else if denied {
            Readiness::Denied
        } else {
            Readiness::NoKeyboard
        }
    }

    fn next_batch(&mut self, timeout: Duration) -> Vec<DeviceInput> {
        let lost_keyboard = std::mem::take(&mut self.lost_keyboard);
        let has_pending = self.open.iter().any(|device| !device.pending.is_empty());
        if !lost_keyboard && !has_pending && !self.any_ready(timeout) {
            return Vec::new();
        }
        let now = clock_gettime(ClockId::Monotonic);
        let cutoff = (now.tv_sec, now.tv_nsec / 1_000);
        let mut records = Vec::new();
        let mut lost = Vec::new();
        for (index, device) in self.open.iter_mut().enumerate() {
            match read_events(&mut device.file) {
                Ok(events) => records.extend(
                    device
                        .take_before(cutoff, events)
                        .into_iter()
                        .map(|event| (index, event)),
                ),
                Err(error) => {
                    tracing::info!(trigger = "hotkey", node = %device.node.display(), %error, "input device lost");
                    lost.push(index);
                }
            }
        }
        let mut batch = Vec::new();
        if lost_keyboard {
            batch.push(DeviceInput::LostKeys);
        }
        batch.extend(self.translate_events(records, held_keys));
        for index in lost.into_iter().rev() {
            if self.open.remove(index).kind == Kind::Keyboard {
                batch.push(DeviceInput::LostKeys);
            }
        }
        batch
    }

    fn translate_events(
        &mut self,
        mut records: Vec<(usize, InputEvent)>,
        query: impl Fn(&File) -> io::Result<HeldKeys>,
    ) -> Vec<DeviceInput> {
        records.sort_by_key(|(_, event)| event.timestamp);
        let mut batch = Vec::new();
        for (index, event) in records {
            let others = self.held_except(index);
            if let Some(input) = self.open[index].feed(event, self.aria_key, others) {
                let recovering = self.state_unavailable
                    || self.open.iter().any(|device| device.stream != Stream::Live);
                if matches!(input, DeviceInput::LostKeys) || !recovering {
                    batch.push(input);
                }
            }
        }
        self.recover(query);
        batch
    }

    /// A failed query stays due: an idle device sends no later report, and
    /// the next rescan tries again.
    fn recover(&mut self, query: impl Fn(&File) -> io::Result<HeldKeys>) {
        for device in &mut self.open {
            if device.stream == Stream::ReportSeen {
                device.pending.clear();
                // EVIOCGKEY flushes the kernel queue; discard already-read events too.
                match query(&device.file) {
                    Ok(held) => {
                        device.held = held;
                        device.stream = Stream::Live;
                    }
                    Err(error) => {
                        tracing::warn!(trigger = "hotkey", node = %device.node.display(), %error, "input device state unavailable");
                    }
                }
            }
        }
    }

    fn any_ready(&self, timeout: Duration) -> bool {
        if self.open.is_empty() {
            thread::sleep(timeout);
            return false;
        }
        let mut fds: Vec<PollFd<'_>> = self
            .open
            .iter()
            .map(|device| PollFd::new(&device.file, PollFlags::IN))
            .collect();
        let timeout = Timespec::try_from(timeout).unwrap_or(Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        });
        match poll(&mut fds, Some(&timeout)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => false,
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(trigger = "hotkey", %error, "waiting on input devices failed");
                thread::sleep(POLL_SLICE);
                false
            }
        }
    }

    fn held_except(&self, index: usize) -> Modifiers {
        self.open
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .fold(Modifiers::NONE, |held, (_, device)| {
                held | device.held.modifiers()
            })
    }
}

impl Device {
    fn take_before(&mut self, cutoff: (i64, i64), events: Vec<InputEvent>) -> Vec<InputEvent> {
        self.pending.extend(events);
        let mut ready = Vec::new();
        // Events arriving during a drain wait until every device can supply their predecessors.
        self.pending.retain(|event| {
            if event.timestamp < cutoff {
                ready.push(*event);
                false
            } else {
                true
            }
        });
        ready
    }

    fn feed(&mut self, event: InputEvent, aria_key: u16, others: Modifiers) -> Option<DeviceInput> {
        if event.kind == EV_SYN && event.code == SYN_DROPPED {
            self.held = HeldKeys::default();
            self.stream = Stream::Dropped;
            return Some(DeviceInput::LostKeys);
        }
        if self.stream != Stream::Live {
            if event.kind == EV_SYN && event.code == SYN_REPORT {
                self.stream = Stream::ReportSeen;
            }
            return None;
        }
        translate(aria_key, event, &mut self.held, others).map(DeviceInput::Key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Has the Aria key, and maybe buttons too.
    Keyboard,
    Pointer,
}

/// The event devices worth reading, by name (`event3`), from sysfs.
fn candidates(class_directory: &Path, aria_key: u16) -> Vec<(String, Kind)> {
    let Ok(entries) = fs::read_dir(class_directory) else {
        return Vec::new();
    };
    let mut found: Vec<(String, Kind)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("event"))
        .filter_map(|name| {
            let capabilities =
                fs::read_to_string(class_directory.join(&name).join("device/capabilities/key"))
                    .ok()?;
            let kind = if has_code(&capabilities, aria_key) {
                Kind::Keyboard
            } else if has_code(&capabilities, BTN_LEFT) {
                Kind::Pointer
            } else {
                return None;
            };
            Some((name, kind))
        })
        .collect();
    found.sort_by(|(left, _), (right, _)| left.cmp(right));
    found
}

/// Reads a sysfs capability bitmap: hex words, most significant first, each
/// as wide as the kernel's `long`.
fn has_code(bitmap: &str, code: u16) -> bool {
    let word_bits = usize::BITS as usize;
    let index = usize::from(code) / word_bits;
    bitmap
        .split_whitespace()
        .rev()
        .nth(index)
        .and_then(|word| u64::from_str_radix(word, 16).ok())
        .is_some_and(|word| word & (1 << (usize::from(code) % word_bits)) != 0)
}

fn open_device(node: &Path) -> io::Result<File> {
    let fd = open(
        node,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

#[allow(
    unsafe_code,
    reason = "evdev ioctls use their kernel-defined buffer types"
)]
fn initialize_device(file: &File) -> io::Result<HeldKeys> {
    const EVIOCSCLOCKID: rustix::ioctl::Opcode = opcode::write::<i32>(b'E', 0xa0);
    const CLOCK_MONOTONIC: i32 = 1;
    // Every descriptor must use the same clock before records can be merged.
    unsafe { ioctl(file, Setter::<EVIOCSCLOCKID, i32>::new(CLOCK_MONOTONIC))? };
    held_keys(file)
}

#[allow(
    unsafe_code,
    reason = "EVIOCGKEY writes the requested 128-bit key bitmap"
)]
fn held_keys(file: &File) -> io::Result<HeldKeys> {
    const EVIOCGKEY: rustix::ioctl::Opcode = opcode::read::<[u8; 16]>(b'E', 0x18);
    // All watched modifiers fit in the first 128 key bits.
    let bitmap = unsafe { ioctl(file, Getter::<EVIOCGKEY, [u8; 16]>::new())? };
    let mut held = HeldKeys::default();
    for (code, _) in MODIFIER_KEYS {
        let code_index = usize::from(code);
        held.record(code, bitmap[code_index / 8] & (1 << (code_index % 8)) != 0);
    }
    Ok(held)
}

fn read_events(file: &mut File) -> io::Result<Vec<InputEvent>> {
    let mut events = Vec::new();
    let mut buffer = [0u8; EVENT_SIZE * EVENTS_PER_READ];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(length) => {
                let (records, _partial) = buffer[..length].as_chunks::<EVENT_SIZE>();
                events.extend(records.iter().map(InputEvent::decode));
                if length < buffer.len() {
                    return Ok(events);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(events),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InputEvent {
    timestamp: (i64, i64),
    kind: u16,
    code: u16,
    value: i32,
}

impl InputEvent {
    fn decode(record: &[u8; EVENT_SIZE]) -> Self {
        let (words, _) = record.as_chunks::<8>();
        let [
            ..,
            kind_low,
            kind_high,
            code_low,
            code_high,
            value_0,
            value_1,
            value_2,
            value_3,
        ] = *record;
        Self {
            timestamp: (i64::from_ne_bytes(words[0]), i64::from_ne_bytes(words[1])),
            kind: u16::from_ne_bytes([kind_low, kind_high]),
            code: u16::from_ne_bytes([code_low, code_high]),
            value: i32::from_ne_bytes([value_0, value_1, value_2, value_3]),
        }
    }
}

/// The modifier keys one device holds, one bit per `MODIFIER_KEYS` entry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct HeldKeys(u8);

impl HeldKeys {
    /// Records a modifier key going down or up; false for any other key.
    fn record(&mut self, code: u16, down: bool) -> bool {
        let Some(index) = MODIFIER_KEYS.iter().position(|(key, _)| *key == code) else {
            return false;
        };
        if down {
            self.0 |= 1 << index;
        } else {
            self.0 &= !(1 << index);
        }
        true
    }

    fn modifiers(self) -> Modifiers {
        MODIFIER_KEYS
            .iter()
            .enumerate()
            .filter(|(index, _)| self.0 & (1 << index) != 0)
            .fold(Modifiers::NONE, |held, (_, (_, modifier))| held | *modifier)
    }
}

fn translate(
    aria_key: u16,
    event: InputEvent,
    device: &mut HeldKeys,
    others: Modifiers,
) -> Option<KeyEvent> {
    // Auto-repeat is neither a press nor a release.
    if event.kind != EV_KEY || !matches!(event.value, KEY_PRESSED | KEY_RELEASED) {
        return None;
    }
    let pressed = event.value == KEY_PRESSED;
    if device.record(event.code, pressed) {
        return Some(KeyEvent::Modifiers {
            held: device.modifiers() | others,
        });
    }
    if event.code == aria_key {
        return Some(if pressed {
            KeyEvent::AriaDown {
                held: device.modifiers() | others,
            }
        } else {
            KeyEvent::AriaUp
        });
    }
    match event.code {
        _ if !pressed => None,
        // Modifiers the Mac leaves unwatched too, and touchpad contacts:
        // they neither join a press nor void it.
        KEY_LEFTALT | KEY_CAPSLOCK | KEY_FN | BTN_DIGI_FIRST..=BTN_DIGI_LAST => None,
        BTN_LEFT..=BTN_TASK => Some(KeyEvent::MouseDown),
        _ => Some(KeyEvent::KeyDown),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    const KEY_A: u16 = 30;
    const BTN_TOUCH: u16 = 0x14a;
    const KEY_REPEATED: i32 = 2;

    fn key(code: u16, value: i32) -> InputEvent {
        InputEvent {
            timestamp: (0, 0),
            kind: EV_KEY,
            code,
            value,
        }
    }

    fn nothing_held(_: &File) -> io::Result<HeldKeys> {
        Ok(HeldKeys::default())
    }

    fn control_held(_: &File) -> io::Result<HeldKeys> {
        let mut held = HeldKeys::default();
        held.record(KEY_LEFTCTRL, true);
        Ok(held)
    }

    fn sync(code: u16) -> InputEvent {
        InputEvent {
            timestamp: (0, 0),
            kind: EV_SYN,
            code,
            value: 0,
        }
    }

    fn record(event: InputEvent) -> [u8; EVENT_SIZE] {
        let mut bytes = [0; EVENT_SIZE];
        bytes[..8].copy_from_slice(&event.timestamp.0.to_ne_bytes());
        bytes[8..16].copy_from_slice(&event.timestamp.1.to_ne_bytes());
        bytes[16..18].copy_from_slice(&event.kind.to_ne_bytes());
        bytes[18..20].copy_from_slice(&event.code.to_ne_bytes());
        bytes[20..24].copy_from_slice(&event.value.to_ne_bytes());
        bytes
    }

    /// A key capability bitmap, as sysfs prints it, with `codes` set.
    fn bitmap(codes: &[u16]) -> String {
        let word_bits = usize::BITS as usize;
        let mut words = [0u64; 8];
        for &code in codes {
            words[usize::from(code) / word_bits] |= 1 << (usize::from(code) % word_bits);
        }
        let printed: Vec<String> = words.iter().rev().map(|word| format!("{word:x}")).collect();
        printed.join(" ")
    }

    /// A fake `/sys/class/input` and `/dev/input` pair.
    struct InputTree {
        root: tempfile::TempDir,
    }

    impl InputTree {
        fn new() -> Self {
            let tree = Self {
                root: tempfile::tempdir().expect("test directory"),
            };
            fs::create_dir_all(tree.class()).expect("class directory");
            fs::create_dir_all(tree.nodes()).expect("device directory");
            tree
        }

        fn class(&self) -> PathBuf {
            self.root.path().join("class")
        }

        fn nodes(&self) -> PathBuf {
            self.root.path().join("dev")
        }

        fn add(&self, name: &str, codes: &[u16]) {
            let device = self.class().join(name).join("device/capabilities");
            fs::create_dir_all(&device).expect("capability directory");
            fs::write(device.join("key"), bitmap(codes) + "\n").expect("capabilities");
            fs::write(self.nodes().join(name), []).expect("device node");
        }

        fn deny(&self, name: &str) {
            fs::set_permissions(self.nodes().join(name), Permissions::from_mode(0o000))
                .expect("permissions");
        }

        fn devices(&self) -> Devices {
            Devices::new(self.class(), self.nodes(), KEY_RIGHTALT)
        }
    }

    #[test]
    fn a_record_decodes_its_type_code_and_value() {
        let event = key(KEY_RIGHTALT, KEY_PRESSED);
        assert_eq!(InputEvent::decode(&record(event)), event);
        let dropped = sync(SYN_DROPPED);
        assert_eq!(InputEvent::decode(&record(dropped)), dropped);
    }

    #[test]
    fn a_capability_bitmap_reads_most_significant_word_first() {
        let printed = bitmap(&[KEY_RIGHTALT, BTN_LEFT, KEY_FN]);
        assert!(has_code(&printed, KEY_RIGHTALT));
        assert!(has_code(&printed, BTN_LEFT));
        assert!(has_code(&printed, KEY_FN));
        assert!(!has_code(&printed, KEY_LEFTALT));
        assert!(!has_code("0", KEY_RIGHTALT));
        assert!(!has_code("", KEY_RIGHTALT));
    }

    #[test]
    fn the_aria_key_goes_down_with_the_modifiers_held_anywhere() {
        let mut keyboard = HeldKeys::default();
        assert!(matches!(
            translate(
                KEY_RIGHTALT,
                key(KEY_LEFTSHIFT, KEY_PRESSED),
                &mut keyboard,
                Modifiers::NONE
            ),
            Some(KeyEvent::Modifiers {
                held: Modifiers::SHIFT
            })
        ));
        assert!(matches!(
            translate(KEY_RIGHTALT, key(KEY_RIGHTALT, KEY_PRESSED), &mut keyboard, Modifiers::COMMAND),
            Some(KeyEvent::AriaDown { held }) if held == Modifiers::SHIFT | Modifiers::COMMAND
        ));
        assert!(matches!(
            translate(
                KEY_RIGHTALT,
                key(KEY_RIGHTALT, KEY_RELEASED),
                &mut keyboard,
                Modifiers::NONE
            ),
            Some(KeyEvent::AriaUp)
        ));
    }

    #[test]
    fn a_modifier_stays_held_until_both_sides_are_up() {
        let mut keyboard = HeldKeys::default();
        translate(
            KEY_RIGHTALT,
            key(KEY_LEFTSHIFT, KEY_PRESSED),
            &mut keyboard,
            Modifiers::NONE,
        );
        translate(
            KEY_RIGHTALT,
            key(KEY_RIGHTSHIFT, KEY_PRESSED),
            &mut keyboard,
            Modifiers::NONE,
        );
        assert!(matches!(
            translate(
                KEY_RIGHTALT,
                key(KEY_LEFTSHIFT, KEY_RELEASED),
                &mut keyboard,
                Modifiers::NONE
            ),
            Some(KeyEvent::Modifiers {
                held: Modifiers::SHIFT
            })
        ));
        assert!(matches!(
            translate(
                KEY_RIGHTALT,
                key(KEY_RIGHTSHIFT, KEY_RELEASED),
                &mut keyboard,
                Modifiers::NONE
            ),
            Some(KeyEvent::Modifiers {
                held: Modifiers::NONE
            })
        ));
    }

    #[test]
    fn typing_and_clicking_count_as_other_input() {
        let mut keyboard = HeldKeys::default();
        assert!(matches!(
            translate(
                KEY_RIGHTALT,
                key(KEY_A, KEY_PRESSED),
                &mut keyboard,
                Modifiers::NONE
            ),
            Some(KeyEvent::KeyDown)
        ));
        assert!(matches!(
            translate(
                KEY_RIGHTALT,
                key(BTN_LEFT, KEY_PRESSED),
                &mut keyboard,
                Modifiers::NONE
            ),
            Some(KeyEvent::MouseDown)
        ));
    }

    #[test]
    fn repeats_releases_touches_and_unwatched_modifiers_are_ignored() {
        let mut keyboard = HeldKeys::default();
        for event in [
            key(KEY_RIGHTALT, KEY_REPEATED),
            key(KEY_A, KEY_REPEATED),
            key(KEY_A, KEY_RELEASED),
            key(BTN_TOUCH, KEY_PRESSED),
            key(KEY_LEFTALT, KEY_PRESSED),
            key(KEY_CAPSLOCK, KEY_PRESSED),
            sync(SYN_REPORT),
        ] {
            assert!(
                translate(KEY_RIGHTALT, event, &mut keyboard, Modifiers::NONE).is_none(),
                "{event:?}"
            );
        }
    }

    #[test]
    fn dropped_events_restore_held_modifiers_at_the_next_report() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        let mut devices = tree.devices();
        devices.rescan_with(nothing_held);
        let mut pending_release = key(KEY_LEFTCTRL, KEY_RELEASED);
        pending_release.timestamp = (2, 0);
        assert!(
            devices.open[0]
                .take_before((1, 0), vec![pending_release])
                .is_empty()
        );
        let inputs = devices.translate_events(
            vec![
                (0, key(KEY_LEFTSHIFT, KEY_PRESSED)),
                (0, sync(SYN_DROPPED)),
                (0, key(KEY_RIGHTALT, KEY_PRESSED)),
                (0, sync(SYN_REPORT)),
                (0, key(KEY_LEFTCTRL, KEY_RELEASED)),
            ],
            control_held,
        );
        assert!(matches!(
            inputs.as_slice(),
            [DeviceInput::Key(_), DeviceInput::LostKeys]
        ));
        assert!(devices.open[0].pending.is_empty());
        let inputs = devices.translate_events(vec![(0, key(KEY_RIGHTALT, KEY_PRESSED))], |_| {
            panic!("only query after a recovery report")
        });
        assert!(matches!(
            inputs.as_slice(),
            [DeviceInput::Key(KeyEvent::AriaDown {
                held: Modifiers::CONTROL
            })]
        ));
    }

    #[test]
    fn opening_a_keyboard_preserves_a_modifier_already_held() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        let mut devices = tree.devices();
        devices.rescan_with(control_held);
        let inputs = devices.translate_events(vec![(0, key(KEY_RIGHTALT, KEY_PRESSED))], |_| {
            panic!("no recovery query before SYN_DROPPED")
        });
        assert!(matches!(
            inputs.as_slice(),
            [DeviceInput::Key(KeyEvent::AriaDown {
                held: Modifiers::CONTROL
            })]
        ));
    }

    #[test]
    fn failed_initialization_keeps_all_hotkeys_unarmed() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        let mut devices = tree.devices();
        let scan = devices.rescan_with(|_| Err(io::ErrorKind::Other.into()));
        assert_eq!(scan, Readiness::StateUnavailable);
        assert!(devices.open.is_empty());
        assert!(devices.state_unavailable);
    }

    #[test]
    fn failed_recovery_blocks_hotkeys_on_other_keyboards() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        tree.add("event1", &[KEY_RIGHTALT]);
        let mut devices = tree.devices();
        devices.rescan_with(nothing_held);
        let inputs = devices.translate_events(
            vec![
                (0, sync(SYN_DROPPED)),
                (0, sync(SYN_REPORT)),
                (1, key(KEY_RIGHTALT, KEY_PRESSED)),
                (1, key(KEY_RIGHTALT, KEY_RELEASED)),
            ],
            |_| Err(io::ErrorKind::Other.into()),
        );
        assert!(matches!(inputs.as_slice(), [DeviceInput::LostKeys]));
        assert_eq!(devices.open[0].stream, Stream::ReportSeen);
        let inputs = devices.translate_events(vec![(0, sync(SYN_REPORT))], control_held);
        assert_eq!(devices.open[0].stream, Stream::Live);
        assert!(inputs.is_empty());
        let inputs = devices.translate_events(vec![(1, key(KEY_RIGHTALT, KEY_PRESSED))], |_| {
            panic!("only query after a recovery report")
        });
        assert!(matches!(
            inputs.as_slice(),
            [DeviceInput::Key(KeyEvent::AriaDown {
                held: Modifiers::CONTROL
            })]
        ));
    }

    #[test]
    fn a_failed_recovery_unarms_until_a_retry_succeeds() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        let mut devices = tree.devices();
        devices.rescan_with(nothing_held);
        devices.translate_events(vec![(0, sync(SYN_DROPPED)), (0, sync(SYN_REPORT))], |_| {
            Err(io::ErrorKind::Other.into())
        });
        assert_eq!(
            devices.rescan_with(nothing_held),
            Readiness::StateUnavailable
        );
        devices.recover(nothing_held);
        assert_eq!(devices.rescan_with(nothing_held), Readiness::Reading);
    }

    #[test]
    fn cross_device_modifiers_follow_the_event_timestamps() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        tree.add("event1", &[KEY_RIGHTALT]);
        let mut devices = tree.devices();
        devices.rescan_with(nothing_held);
        let mut control_down = key(KEY_LEFTCTRL, KEY_PRESSED);
        control_down.timestamp = (1, 100);
        let mut alt_down = key(KEY_RIGHTALT, KEY_PRESSED);
        alt_down.timestamp = (1, 200);
        let mut control_up = key(KEY_LEFTCTRL, KEY_RELEASED);
        control_up.timestamp = (1, 300);
        let inputs = devices.translate_events(
            vec![(0, alt_down), (1, control_down), (1, control_up)],
            |_| panic!("no recovery query before SYN_DROPPED"),
        );
        assert!(matches!(
            inputs[1],
            DeviceInput::Key(KeyEvent::AriaDown {
                held: Modifiers::CONTROL
            })
        ));
    }

    #[test]
    fn events_arriving_during_a_drain_wait_for_other_devices() {
        let tree = InputTree::new();
        tree.add("event0", &[BTN_LEFT]);
        tree.add("event1", &[KEY_RIGHTALT]);
        let mut devices = tree.devices();
        devices.rescan_with(nothing_held);
        let mut down = key(KEY_RIGHTALT, KEY_PRESSED);
        down.timestamp = (1, 100);
        let mut click = key(BTN_LEFT, KEY_PRESSED);
        click.timestamp = (1, 200);
        let mut up = key(KEY_RIGHTALT, KEY_RELEASED);
        up.timestamp = (1, 300);
        let cutoff = (1, 100);
        assert!(devices.open[0].take_before(cutoff, Vec::new()).is_empty());
        assert!(
            devices.open[1]
                .take_before(cutoff, vec![down, up])
                .is_empty()
        );
        let cutoff = (1, 400);
        let mut records: Vec<_> = devices.open[0]
            .take_before(cutoff, vec![click])
            .into_iter()
            .map(|event| (0, event))
            .collect();
        records.extend(
            devices.open[1]
                .take_before(cutoff, Vec::new())
                .into_iter()
                .map(|event| (1, event)),
        );
        let inputs = devices.translate_events(records, |_| panic!("no recovery"));
        let mut detector = PressDetector::new();
        for input in inputs {
            let DeviceInput::Key(event) = input else {
                panic!("no lost events")
            };
            assert!(detector.handle(event, Instant::now()).is_none());
        }
    }

    #[test]
    fn a_click_between_alt_down_and_up_cancels_the_tap() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        tree.add("event1", &[BTN_LEFT]);
        let timed_record = |event, micros: i64| {
            let mut bytes = record(event);
            bytes[..8].copy_from_slice(&0_i64.to_ne_bytes());
            bytes[8..16].copy_from_slice(&micros.to_ne_bytes());
            bytes
        };
        fs::write(
            tree.nodes().join("event0"),
            [
                timed_record(key(KEY_RIGHTALT, KEY_PRESSED), 100),
                timed_record(key(KEY_RIGHTALT, KEY_RELEASED), 300),
            ]
            .concat(),
        )
        .expect("keyboard events");
        fs::write(
            tree.nodes().join("event1"),
            timed_record(key(BTN_LEFT, KEY_PRESSED), 200),
        )
        .expect("pointer events");
        let mut devices = tree.devices();
        devices.rescan_with(nothing_held);
        let mut detector = PressDetector::new();
        let mut presses = Vec::new();
        for input in devices.next_batch(Duration::ZERO) {
            let press = match input {
                DeviceInput::Key(event) => detector.handle(event, Instant::now()),
                DeviceInput::LostKeys => detector.reset(),
            };
            presses.extend(press);
        }
        assert!(presses.is_empty(), "click must cancel the tap: {presses:?}");
    }

    #[test]
    fn a_path_is_quoted_for_the_shell() {
        assert_eq!(
            shell_quoted(Path::new("/opt/aria (1)/it's.rules")),
            r"'/opt/aria (1)/it'\''s.rules'"
        );
    }

    #[test]
    fn fn_gets_a_definite_reason() {
        assert!(LinuxHotkeyListener::unarmed_reason(AriaKey::Fn).contains("Linux cannot see"));
    }

    #[test]
    fn keyboards_with_the_key_and_pointers_are_read() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_A, KEY_RIGHTALT]);
        tree.add("event1", &[BTN_LEFT]);
        tree.add("event2", &[KEY_A]);
        tree.add("mouse0", &[BTN_LEFT]);
        assert_eq!(
            candidates(&tree.class(), KEY_RIGHTALT),
            [
                ("event0".to_owned(), Kind::Keyboard),
                ("event1".to_owned(), Kind::Pointer)
            ]
        );
        let mut devices = tree.devices();
        assert_eq!(devices.rescan_with(nothing_held), Readiness::Reading);
        assert_eq!(devices.open.len(), 2);
        assert_eq!(devices.rescan_with(nothing_held), Readiness::Reading);
        assert_eq!(devices.open.len(), 2, "a rescan keeps devices open once");
    }

    #[test]
    fn a_keyboard_the_user_may_not_read_waits_for_permission() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        tree.deny("event0");
        let mut devices = tree.devices();
        assert_eq!(devices.probe(), Readiness::Denied);
        assert_eq!(devices.rescan_with(nothing_held), Readiness::Denied);
    }

    #[test]
    fn revoked_access_closes_an_open_keyboard() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        let mut devices = tree.devices();
        devices.rescan_with(nothing_held);
        tree.deny("event0");
        let scan = devices.rescan_with(nothing_held);
        assert_eq!(scan, Readiness::Denied);
        assert!(devices.open.is_empty());
        assert!(matches!(
            devices.next_batch(Duration::ZERO).as_slice(),
            [DeviceInput::LostKeys]
        ));
        assert!(devices.next_batch(Duration::ZERO).is_empty());
    }

    #[test]
    fn without_a_keyboard_the_key_is_unavailable() {
        let tree = InputTree::new();
        tree.add("event0", &[BTN_LEFT]);
        assert_eq!(tree.devices().probe(), Readiness::NoKeyboard);
        assert_eq!(
            tree.devices().rescan_with(nothing_held),
            Readiness::NoKeyboard
        );
    }

    #[test]
    fn a_readable_keyboard_probes_as_reading_without_being_opened() {
        let tree = InputTree::new();
        tree.add("event0", &[KEY_RIGHTALT]);
        let devices = tree.devices();
        assert_eq!(devices.probe(), Readiness::Reading);
        assert!(devices.open.is_empty());
    }
}
