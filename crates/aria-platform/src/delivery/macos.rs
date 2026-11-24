//! macOS text delivery: insertion posts synthetic keyboard events at the
//! focused application, copy writes the transcript to the pasteboard.

use std::cell::{Cell, RefCell};
use std::ffi::c_ulong;
use std::time::{Duration, Instant};

use objc2::rc::autoreleasepool;
use objc2_app_kit::{NSPasteboard, NSRunningApplication};
use objc2_core_graphics::{CGEvent, CGEventFlags, CGEventTapLocation};
use objc2_foundation::NSString;

use super::{
    Caret, DeliveryError, DeliveryOutcome, Finished, OutputMode, Placement, TextDelivery,
    validate_text, validate_timeout,
};
use crate::accessibility::Trust;

mod anchor;
mod caret;
mod element;
mod ghostty;
mod placement;

use anchor::Anchoring;

const PLAIN_TEXT_TYPE: &str = "public.utf8-plain-text";
// The concealed type asks clipboard managers not to record the transcript in
// their history. It is a convention, not an OS guarantee.
const CONCEALED_TYPE: &str = "org.nspasteboard.ConcealedType";

/// UTF-16 units carried by one posted event. CoreGraphics documents no maximum;
/// this is a conservative slice rather than a measured limit, because long
/// unicode payloads are widely reported to arrive truncated.
const UNITS_PER_EVENT: usize = 20;

/// The virtual key the posted events name. The unicode payload carries the
/// text, but the keycode still has to be an assigned one: measured on macOS 26,
/// `0x7F` and other unassigned codes are dropped before the application sees
/// them. `0x00` is `kVK_ANSI_A`, so an application that ignores the payload and
/// translates the keycode itself types "a" — the price of arriving at all.
const CARRIER_VIRTUAL_KEY: u16 = 0x00;
/// kVK_Return, the main Return key. The hotkey tap watches for it too.
pub(crate) const RETURN_VIRTUAL_KEY: u16 = 0x24;

/// Carbon's `noErr`.
const NO_ERR: i32 = 0;

pub struct MacOsTextDelivery {
    /// How long a paste may wait for the app to answer. Posted keys don't
    /// wait at all.
    timeout: Duration,
}

pub enum MacOsDeliveryTarget {
    Application {
        destination: Destination,
        anchoring: RefCell<Anchoring>,
    },
    ApplicationAndSubmit {
        destination: Destination,
        anchoring: RefCell<Anchoring>,
        submission: Cell<Submission>,
    },
    ApplicationOrClipboard(Destination),
    Clipboard,
}

impl MacOsDeliveryTarget {
    fn application(destination: Destination) -> Self {
        Self::Application {
            destination,
            anchoring: RefCell::new(Anchoring::Pending),
        }
    }

    fn submitting(destination: Destination, anchoring: Anchoring) -> Self {
        let submission = if anchoring.is_unavailable() {
            Submission::Unbound
        } else {
            Submission::Owed
        };
        Self::ApplicationAndSubmit {
            destination,
            anchoring: RefCell::new(anchoring),
            submission: Cell::new(submission),
        }
    }
}

pub struct Destination {
    process: u32,
    route: Route,
}

impl Destination {
    fn choose(process: u32) -> Self {
        Self {
            process,
            route: Route::choose(process),
        }
    }
}

/// How text travels to its app, chosen as the press starts.
enum Route {
    Keys,
    /// A modal editor in the terminal reads a paste as text in any mode,
    /// where keys would run as its commands.
    Paste(ghostty::Terminal),
}

impl Route {
    /// Ghostty takes keys whenever its focused terminal can't be pinned: its
    /// scripting is off or too old, permission is refused or undecided, it
    /// didn't answer, or focus is outside a terminal window.
    fn choose(process: u32) -> Self {
        let is_ghostty = application(process)
            .and_then(|application| application.bundle_id)
            .is_some_and(|id| id == ghostty::BUNDLE_ID);
        if !is_ghostty {
            return Self::Keys;
        }
        match ghostty::Terminal::focused(process) {
            Ok(terminal) => Self::Paste(terminal),
            Err(reason) => {
                tracing::warn!(
                    reason,
                    "Ghostty takes typed keys, which Vim modes may run as commands"
                );
                Self::Keys
            }
        }
    }

    /// Whether the route names the exact place text goes, so text may wait
    /// for it even when the field can't be read.
    fn pins_target(&self) -> bool {
        matches!(self, Self::Paste(_))
    }

    fn check_focus(&self, process: u32) -> Result<(), DeliveryError> {
        match self {
            Self::Keys => Ok(()),
            Self::Paste(terminal) => terminal.check_focus(process),
        }
    }

    fn send(&self, process: u32, text: &str, timeout: Duration) -> Result<(), Undelivered> {
        match self {
            // Posted events can't be recalled, so a failure partway through
            // may have typed some of the text.
            Self::Keys => post(text).map_err(Undelivered::Uncertain),
            Self::Paste(terminal) => terminal.paste(process, text, timeout),
        }
    }
}

enum Undelivered {
    /// None of it reached the app, so it may still go elsewhere.
    Refused(DeliveryError),
    /// Some or all of it may have reached the app.
    Uncertain(DeliveryError),
}

impl Undelivered {
    fn into_error(self) -> DeliveryError {
        match self {
            Self::Refused(error) | Self::Uncertain(error) => error,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submission {
    Owed,
    /// The field couldn't be read as the press started. The text still
    /// types, but no Return follows: nothing shows which field it would reach.
    Unbound,
    Spent,
}

impl MacOsTextDelivery {
    pub(super) fn new(timeout: Duration) -> Result<Self, DeliveryError> {
        validate_timeout(timeout)?;
        Ok(Self { timeout })
    }
}

impl TextDelivery for MacOsTextDelivery {
    type Prepared = MacOsDeliveryTarget;

    const DEFAULT_MODE: OutputMode = OutputMode::Insert;
    const OUTLIVES_PROCESS: bool = true;

    fn frontmost(&self) -> Option<super::Application> {
        frontmost_application()
    }

    fn prepare(
        &self,
        mode: OutputMode,
        should_cancel: impl Fn() -> bool,
    ) -> Result<Self::Prepared, DeliveryError> {
        if should_cancel() {
            return Err(DeliveryError::Cancelled);
        }
        match mode {
            // Asked while there is still nothing to lose: a refusal ends the
            // press before a transcript exists.
            OutputMode::Insert => autoreleasepool(|_| {
                let process = insertable(observe())?;
                Ok(MacOsDeliveryTarget::application(Destination::choose(
                    process,
                )))
            }),
            // Bound as the press starts, so a later read that finds another
            // field refuses the Return.
            OutputMode::InsertAndSubmit => autoreleasepool(|_| {
                let process = insertable(observe())?;
                let destination = Destination::choose(process);
                let mut anchoring = Anchoring::Pending;
                anchoring.start(caret::read(process).1);
                Ok(MacOsDeliveryTarget::submitting(destination, anchoring))
            }),
            // The same question, but a refusal costs nothing here: the
            // clipboard is still a destination, so the press goes ahead.
            OutputMode::InsertOrCopy => autoreleasepool(|_| {
                Ok(match insertable(observe()) {
                    Ok(process) => {
                        MacOsDeliveryTarget::ApplicationOrClipboard(Destination::choose(process))
                    }
                    Err(error) => {
                        tracing::warn!(%error, "no insertion target; delivering to the clipboard");
                        MacOsDeliveryTarget::Clipboard
                    }
                })
            }),
            OutputMode::Copy => Ok(MacOsDeliveryTarget::Clipboard),
        }
    }

    fn finish(
        &self,
        prepared: &Self::Prepared,
        should_cancel: impl Fn() -> bool,
    ) -> Result<Finished, DeliveryError> {
        let MacOsDeliveryTarget::ApplicationAndSubmit {
            destination,
            anchoring,
            submission,
        } = prepared
        else {
            return Ok(Finished::Typed);
        };
        match submission.replace(Submission::Spent) {
            Submission::Spent => Err(DeliveryError::SubmissionSpent),
            Submission::Unbound => Err(DeliveryError::SubmissionUnverified),
            Submission::Owed => autoreleasepool(|_| {
                return_after_text(destination, anchoring, &should_cancel)
                    .map(|()| Finished::Submitted)
            }),
        }
    }

    fn press_return(
        &self,
        prepared: &Self::Prepared,
        should_cancel: impl Fn() -> bool,
    ) -> Result<(), DeliveryError> {
        autoreleasepool(|_| match prepared {
            MacOsDeliveryTarget::Application {
                destination,
                anchoring,
            } => return_after_text(destination, anchoring, &should_cancel),
            MacOsDeliveryTarget::ApplicationAndSubmit { .. } => Err(DeliveryError::SubmissionSpent),
            MacOsDeliveryTarget::ApplicationOrClipboard(destination) => {
                guard(destination, &should_cancel, observe())?;
                post_return()
            }
            MacOsDeliveryTarget::Clipboard => Err(DeliveryError::TargetUnavailable),
        })
    }

    fn deliver(
        &self,
        prepared: &Self::Prepared,
        text: &str,
        should_cancel: impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        validate_text(text)?;
        if should_cancel() {
            return Err(DeliveryError::Cancelled);
        }

        autoreleasepool(|_| match prepared {
            MacOsDeliveryTarget::ApplicationAndSubmit { submission, .. }
                if submission.get() == Submission::Spent =>
            {
                Err(DeliveryError::SubmissionSpent)
            }
            MacOsDeliveryTarget::Application {
                destination,
                anchoring,
            }
            | MacOsDeliveryTarget::ApplicationAndSubmit {
                destination,
                anchoring,
                ..
            } => {
                let mut anchoring = anchoring.borrow_mut();
                anchoring.before_typing()?;
                let outcome = insert(destination, text, self.timeout, &should_cancel, observe())
                    .map_err(|error| match error {
                        DeliveryError::TargetChanged => {
                            focus_lost(&anchoring, destination.route.pins_target())
                        }
                        error => error,
                    })?;
                anchoring.after_typing(text);
                Ok(outcome)
            }
            MacOsDeliveryTarget::ApplicationOrClipboard(destination) => insert_or_copy(
                destination,
                text,
                self.timeout,
                &should_cancel,
                observe(),
                &NSPasteboard::generalPasteboard(),
            ),
            MacOsDeliveryTarget::Clipboard => {
                write_transcript(&NSPasteboard::generalPasteboard(), text)
                    .map(|()| DeliveryOutcome::Copied)
            }
        })
    }

    fn target_process(&self, prepared: &Self::Prepared) -> Option<u32> {
        match prepared {
            MacOsDeliveryTarget::Application { destination, .. }
            | MacOsDeliveryTarget::ApplicationAndSubmit { destination, .. }
            | MacOsDeliveryTarget::ApplicationOrClipboard(destination) => Some(destination.process),
            MacOsDeliveryTarget::Clipboard => None,
        }
    }

    fn caret(&self, prepared: &Self::Prepared) -> Caret {
        match prepared {
            MacOsDeliveryTarget::Application {
                destination,
                anchoring,
            }
            | MacOsDeliveryTarget::ApplicationAndSubmit {
                destination,
                anchoring,
                ..
            } => autoreleasepool(|_| {
                let process = destination.process;
                let (caret, focus) = caret::read(process);
                // Only a read with the target in front binds the field: from
                // behind, it may see another app's secure input, or a field
                // the user hasn't come back to.
                if insertable(observe()).ok() == Some(process) {
                    anchoring.borrow_mut().start(focus);
                }
                caret
            }),
            MacOsDeliveryTarget::ApplicationOrClipboard(destination) => {
                autoreleasepool(|_| caret::read(destination.process).0)
            }
            MacOsDeliveryTarget::Clipboard => Caret::default(),
        }
    }

    fn placement(&self, prepared: &Self::Prepared) -> Option<Placement> {
        let (MacOsDeliveryTarget::Application {
            destination,
            anchoring,
        }
        | MacOsDeliveryTarget::ApplicationAndSubmit {
            destination,
            anchoring,
            ..
        }) = prepared
        else {
            return None;
        };
        let process = destination.process;
        autoreleasepool(|_| {
            if insertable(observe()).ok()? != process {
                return None;
            }
            let deadline = Instant::now() + placement::READ_BUDGET;
            let (field, caret) = anchoring.borrow().resting_caret(process, deadline)?;
            placement::read(&field, caret, deadline)
        })
    }
}

/// Refuses the Return when the field the text went to lost focus, or its
/// caret moved off the text. Another field in the same app may hold a command
/// the person never meant to send. A field that can't be read leaves only the
/// app check.
fn return_after_text(
    destination: &Destination,
    anchoring: &RefCell<Anchoring>,
    should_cancel: &impl Fn() -> bool,
) -> Result<(), DeliveryError> {
    anchoring.borrow_mut().before_return()?;
    guard(destination, should_cancel, observe())?;
    post_return()
}

/// Focus left before this text went in. Text may wait only when the press
/// knows where it goes: the field it started in, or the pinned terminal.
fn focus_lost(anchoring: &Anchoring, route_pins_target: bool) -> DeliveryError {
    if anchoring.is_unavailable() && !route_pins_target {
        return DeliveryError::TargetUnverified;
    }
    DeliveryError::TargetChanged
}

/// What the insertion guards look at, read all at once so `prepare` and the
/// re-check in `insert` judge the same facts, and so a test can hand in values.
/// A route's own focus check reads its app live.
struct Facts {
    trust: Trust,
    secure_input: bool,
    frontmost: Result<u32, DeliveryError>,
}

fn observe() -> Facts {
    Facts {
        trust: Trust::reuse_or_check(),
        secure_input: secure_input_enabled(),
        frontmost: frontmost_process(),
    }
}

/// The process that may receive typed text right now.
///
/// Permission is judged first because its refusal is the only one that tells
/// the user how to fix it.
fn insertable(facts: Facts) -> Result<u32, DeliveryError> {
    if !facts.trust.is_trusted() {
        return Err(DeliveryError::InsertionNotPermitted);
    }
    if facts.secure_input {
        return Err(DeliveryError::SecureInput);
    }
    facts.frontmost
}

fn insert(
    destination: &Destination,
    text: &str,
    timeout: Duration,
    should_cancel: &impl Fn() -> bool,
    facts: Facts,
) -> Result<DeliveryOutcome, DeliveryError> {
    guard(destination, should_cancel, facts)?;
    destination
        .route
        .send(destination.process, text, timeout)
        .map_err(Undelivered::into_error)?;
    Ok(DeliveryOutcome::Inserted)
}

/// Every refusal that precedes sending the first of the text.
fn guard(
    destination: &Destination,
    should_cancel: &impl Fn() -> bool,
    facts: Facts,
) -> Result<(), DeliveryError> {
    // Judged again rather than trusted from `prepare`: a whole recording and
    // transcription sit in between, and the user is free to switch windows,
    // click into a password field, or withdraw the grant during it. App focus is
    // judged before secure input, which is system-wide: a password field in
    // another app means the user left, not that this field refuses.
    if !facts.trust.is_trusted() {
        return Err(DeliveryError::InsertionNotPermitted);
    }
    // A front process that can't be read, as during Mission Control, isn't
    // this one either.
    if facts.frontmost.ok() != Some(destination.process) {
        return Err(anchor::target_changed("another app is frontmost"));
    }
    if facts.secure_input {
        return Err(DeliveryError::SecureInput);
    }
    destination.route.check_focus(destination.process)?;

    // Check before typing: cancelling midway would leave a partial transcript
    // in the document while reporting failure.
    if should_cancel() {
        return Err(DeliveryError::Cancelled);
    }
    Ok(())
}

fn post(text: &str) -> Result<(), DeliveryError> {
    // Every event is routed by focus at the moment the window server handles
    // it, so a window switch during this loop spills the rest into the new one.
    for chunk in unicode_chunks(text) {
        post_chunk(&chunk)?;
    }
    Ok(())
}

// Only text that reached nobody may go to the clipboard: a refusal from the
// guard, or from the app. Keys that failed partway may have typed some of it.
fn insert_or_copy(
    destination: &Destination,
    text: &str,
    timeout: Duration,
    should_cancel: &impl Fn() -> bool,
    facts: Facts,
    pasteboard: &NSPasteboard,
) -> Result<DeliveryOutcome, DeliveryError> {
    let refusal = match guard(destination, should_cancel, facts) {
        Ok(()) => match destination.route.send(destination.process, text, timeout) {
            Ok(()) => return Ok(DeliveryOutcome::Inserted),
            Err(Undelivered::Refused(refusal)) => refusal,
            Err(Undelivered::Uncertain(error)) => return Err(error),
        },
        Err(DeliveryError::Cancelled) => return Err(DeliveryError::Cancelled),
        Err(refusal) => refusal,
    };
    // A guard refusal skips the guard's cancel check, and an app refusal
    // comes after a send, so ask again before anything lands on the clipboard.
    if should_cancel() {
        return Err(DeliveryError::Cancelled);
    }
    // The spaces chosen for the caret don't belong on the clipboard.
    match write_transcript(pasteboard, text.trim_matches(' ')) {
        Ok(()) => {
            tracing::warn!(%refusal, "insertion refused; the transcript is on the clipboard");
            Ok(DeliveryOutcome::Copied)
        }
        // What refused the insertion is the actionable cause, so the clipboard
        // rejection is logged rather than returned in its place.
        Err(rejected) => {
            tracing::warn!(%refusal, %rejected, "insertion refused and the clipboard rejected it");
            Err(refusal)
        }
    }
}

/// Whether any process has secure keyboard entry on — a password field, or
/// Terminal's Secure Keyboard Entry. The window server then drops synthetic
/// keystrokes without a word, so this is the only way to learn it first.
#[allow(
    unsafe_code,
    reason = "a Carbon entry point with no binding in this tree; it takes no \
              arguments and reads one global flag"
)]
fn secure_input_enabled() -> bool {
    #[link(name = "Carbon", kind = "framework")]
    unsafe extern "C-unwind" {
        fn IsSecureEventInputEnabled() -> u8;
    }
    // SAFETY: no arguments, no state.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// The process that owns keyboard focus.
///
/// `GetFrontProcess` is Carbon, deprecated since 10.9, and still the one call
/// that answers live from here. Each replacement was measured and fails:
/// `NSWorkspace.frontmostApplication` refreshes only when the main run loop
/// turns, and no daemon path turns one, so it freezes at its first answer.
/// The Accessibility API reports `kAXErrorCannotComplete` until the process
/// has talked to the window server, which the daemon never does. The window
/// list names the front *window's* owner, which misses Spotlight and every
/// application showing no window.
pub(super) fn frontmost_process() -> Result<u32, DeliveryError> {
    front_process_id()
        .and_then(|process| u32::try_from(process).ok())
        .filter(|process| *process > 0)
        .ok_or(DeliveryError::TargetUnavailable)
}

pub fn frontmost_application() -> Option<super::Application> {
    application(frontmost_process().ok()?)
}

fn application(process: u32) -> Option<super::Application> {
    autoreleasepool(|_| {
        let pid = i32::try_from(process).ok()?;
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        let name = app.localizedName()?.to_string();
        let bundle_id = app.bundleIdentifier().map(|id| id.to_string());
        Some(super::Application { name, bundle_id })
    })
}

/// Carbon's handle on a process.
#[repr(C)]
struct ProcessSerialNumber {
    high: u32,
    low: u32,
}

#[allow(
    unsafe_code,
    reason = "two Carbon entry points with no binding in this tree; the \
              invariants live on the SAFETY comments"
)]
fn front_process_id() -> Option<i32> {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C-unwind" {
        fn GetFrontProcess(serial: *mut ProcessSerialNumber) -> i16;
        fn GetProcessPID(serial: *const ProcessSerialNumber, process: *mut i32) -> i32;
    }

    let mut serial = ProcessSerialNumber { high: 0, low: 0 };
    // SAFETY: `serial` is a live struct with Carbon's layout.
    if i32::from(unsafe { GetFrontProcess(&mut serial) }) != NO_ERR {
        return None;
    }
    let mut process = 0;
    // SAFETY: `serial` was filled in by the call above, and `process` points
    // at a live `i32`.
    let status = unsafe { GetProcessPID(&serial, &mut process) };
    (status == NO_ERR).then_some(process)
}

/// Splits the text into slices of at most [`UNITS_PER_EVENT`] UTF-16 units,
/// never through a character.
fn unicode_chunks(text: &str) -> Vec<Vec<u16>> {
    let mut chunks = Vec::new();
    let mut current: Vec<u16> = Vec::new();
    let mut buffer = [0u16; 2];

    for character in text.chars() {
        let encoded = character.encode_utf16(&mut buffer);
        if current.len() + encoded.len() > UNITS_PER_EVENT {
            chunks.push(std::mem::take(&mut current));
        }
        current.extend_from_slice(encoded);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Posts one slice as a press and a release, both carrying the text.
///
/// Sent to the session rather than to the process the caller checked:
/// `CGEventPostToPid` would bind delivery to that process, but measured on
/// macOS 26 its events never arrive. Passes the daemon's own hotkey tap, which
/// ignores key-downs whose source PID is not 0 (`hotkey/macos.rs`).
#[allow(
    unsafe_code,
    reason = "attaching the unicode string is a raw-pointer C call; the \
              invariants live on the SAFETY comment below"
)]
fn post_chunk(units: &[u16]) -> Result<(), DeliveryError> {
    for key_down in [true, false] {
        let Some(event) = CGEvent::new_keyboard_event(None, CARRIER_VIRTUAL_KEY, key_down) else {
            tracing::warn!("CoreGraphics refused to create a keyboard event");
            return Err(DeliveryError::TargetUnavailable);
        };
        // A modifier still held from the hotkey would otherwise turn the
        // transcript into shortcuts.
        CGEvent::set_flags(Some(&event), CGEventFlags(0));
        // SAFETY: `units` outlives the call, and its length is the count of
        // UTF-16 units it holds. CoreGraphics copies the buffer into the event.
        unsafe {
            CGEvent::keyboard_set_unicode_string(
                Some(&event),
                units.len() as c_ulong,
                units.as_ptr(),
            );
        }
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
    }
    Ok(())
}

fn post_return() -> Result<(), DeliveryError> {
    let down = CGEvent::new_keyboard_event(None, RETURN_VIRTUAL_KEY, true)
        .ok_or(DeliveryError::TargetUnavailable)?;
    let up = CGEvent::new_keyboard_event(None, RETURN_VIRTUAL_KEY, false)
        .ok_or(DeliveryError::TargetUnavailable)?;
    for event in [down, up] {
        CGEvent::set_flags(Some(&event), CGEventFlags(0));
        CGEvent::post(CGEventTapLocation::SessionEventTap, Some(&event));
    }
    Ok(())
}

fn write_transcript(pasteboard: &NSPasteboard, text: &str) -> Result<(), DeliveryError> {
    pasteboard.clearContents();
    // Concealed marker first: the transcript must never sit on the
    // pasteboard without it.
    let concealed =
        pasteboard.setString_forType(&NSString::from_str(""), &NSString::from_str(CONCEALED_TYPE));
    let written = pasteboard.setString_forType(
        &NSString::from_str(text),
        &NSString::from_str(PLAIN_TEXT_TYPE),
    );
    if !(concealed && written) {
        return Err(DeliveryError::ClipboardRejected);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TRANSCRIPT: &str = "private transcript";
    const TEST_TIMEOUT: Duration = Duration::from_secs(2);
    const PREPARED_TARGET: u32 = 4321;

    fn keys() -> Destination {
        Destination {
            process: PREPARED_TARGET,
            route: Route::Keys,
        }
    }

    #[test]
    fn a_field_unread_at_the_start_sends_no_return() {
        let delivery = MacOsTextDelivery::new(TEST_TIMEOUT).unwrap();
        let target = MacOsDeliveryTarget::submitting(keys(), Anchoring::Unavailable);

        assert!(matches!(
            delivery.finish(&target, || false),
            Err(DeliveryError::SubmissionUnverified)
        ));
        assert!(matches!(
            delivery.finish(&target, || false),
            Err(DeliveryError::SubmissionSpent)
        ));
    }

    /// Facts from a machine that permits insertion, with whatever focus the
    /// test wants.
    fn permitted(frontmost: Result<u32, DeliveryError>) -> Facts {
        Facts {
            trust: Trust::Trusted,
            secure_input: false,
            frontmost,
        }
    }

    /// An insertion that is not cancelled, so a test that is about the facts
    /// names only the facts.
    fn insertion(facts: Facts) -> Result<DeliveryOutcome, DeliveryError> {
        insert(&keys(), TEST_TRANSCRIPT, TEST_TIMEOUT, &|| false, facts)
    }

    #[test]
    fn transcript_lands_on_the_pasteboard_with_the_concealed_marker() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();

        write_transcript(&pasteboard, TEST_TRANSCRIPT).expect("pasteboard write should succeed");

        let text = pasteboard.stringForType(&NSString::from_str(PLAIN_TEXT_TYPE));
        assert_eq!(
            text.expect("text should be present").to_string(),
            TEST_TRANSCRIPT
        );
        assert!(
            pasteboard
                .stringForType(&NSString::from_str(CONCEALED_TYPE))
                .is_some()
        );
    }

    fn pasteboard_text(pasteboard: &NSPasteboard) -> Option<String> {
        pasteboard
            .stringForType(&NSString::from_str(PLAIN_TEXT_TYPE))
            .map(|text| text.to_string())
    }

    #[test]
    fn a_refused_guard_sends_the_transcript_to_the_pasteboard() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let secure_input = Facts {
            trust: Trust::Trusted,
            secure_input: true,
            frontmost: Ok(PREPARED_TARGET),
        };

        let outcome = insert_or_copy(
            &keys(),
            TEST_TRANSCRIPT,
            TEST_TIMEOUT,
            &|| false,
            secure_input,
            &pasteboard,
        )
        .expect("the pasteboard should take the transcript");

        assert_eq!(outcome, DeliveryOutcome::Copied);
        assert_eq!(
            pasteboard_text(&pasteboard).as_deref(),
            Some(TEST_TRANSCRIPT)
        );
    }

    #[test]
    fn a_refused_guard_copies_the_transcript_without_caret_spacing() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();
        let spaced = format!(" {TEST_TRANSCRIPT} ");

        insert_or_copy(
            &keys(),
            &spaced,
            TEST_TIMEOUT,
            &|| false,
            permitted(Ok(PREPARED_TARGET + 1)),
            &pasteboard,
        )
        .expect("the pasteboard should take the transcript");

        assert_eq!(
            pasteboard_text(&pasteboard).as_deref(),
            Some(TEST_TRANSCRIPT)
        );
    }

    #[test]
    fn a_cancel_after_the_refusal_leaves_the_pasteboard_empty() {
        let pasteboard = NSPasteboard::pasteboardWithUniqueName();

        let result = insert_or_copy(
            &keys(),
            TEST_TRANSCRIPT,
            TEST_TIMEOUT,
            &|| true,
            permitted(Ok(PREPARED_TARGET + 1)),
            &pasteboard,
        );

        assert!(matches!(result, Err(DeliveryError::Cancelled)));
        assert_eq!(pasteboard_text(&pasteboard), None);
    }

    #[test]
    fn a_withdrawn_grant_beats_every_other_refusal() {
        let result = insertion(Facts {
            trust: Trust::Untrusted,
            secure_input: true,
            frontmost: Err(DeliveryError::TargetUnavailable),
        });

        assert!(matches!(result, Err(DeliveryError::InsertionNotPermitted)));
    }

    #[test]
    fn secure_input_in_the_target_app_is_refused() {
        let result = insertion(Facts {
            trust: Trust::Trusted,
            secure_input: true,
            frontmost: Ok(PREPARED_TARGET),
        });

        assert!(matches!(result, Err(DeliveryError::SecureInput)));
    }

    #[test]
    fn insertion_stops_when_another_application_took_focus() {
        let result = insertion(permitted(Ok(PREPARED_TARGET + 1)));

        assert!(matches!(result, Err(DeliveryError::TargetChanged)));
    }

    #[test]
    fn focus_lost_from_an_unknown_field_cannot_wait_for_it() {
        assert!(matches!(
            focus_lost(&Anchoring::Unavailable, false),
            DeliveryError::TargetUnverified
        ));
        assert!(matches!(
            focus_lost(&Anchoring::Pending, false),
            DeliveryError::TargetChanged
        ));
    }

    #[test]
    fn a_pinned_terminal_lets_text_wait_though_its_field_is_unknown() {
        assert!(matches!(
            focus_lost(&Anchoring::Unavailable, true),
            DeliveryError::TargetChanged
        ));
    }

    #[test]
    fn a_password_field_in_another_app_reads_as_focus_lost() {
        let result = insertion(Facts {
            trust: Trust::Trusted,
            secure_input: true,
            frontmost: Ok(PREPARED_TARGET + 1),
        });

        assert!(matches!(result, Err(DeliveryError::TargetChanged)));
    }

    #[test]
    fn an_unreadable_front_process_reads_as_focus_lost() {
        let result = insertion(permitted(Err(DeliveryError::TargetUnavailable)));

        assert!(matches!(result, Err(DeliveryError::TargetChanged)));
    }

    #[test]
    fn a_cancelled_insertion_types_nothing() {
        let result = insert(
            &keys(),
            TEST_TRANSCRIPT,
            TEST_TIMEOUT,
            &|| true,
            permitted(Ok(PREPARED_TARGET)),
        );

        assert!(matches!(result, Err(DeliveryError::Cancelled)));
    }

    #[test]
    fn deliver_rejects_an_already_cancelled_request() {
        let delivery = MacOsTextDelivery::new(TEST_TIMEOUT).expect("delivery should initialize");
        let target = MacOsDeliveryTarget::application(keys());

        let result = delivery.deliver(&target, TEST_TRANSCRIPT, || true);

        assert!(matches!(result, Err(DeliveryError::Cancelled)));
    }

    #[test]
    fn chunks_hold_at_most_one_event_worth_of_units() {
        let text = "a".repeat(UNITS_PER_EVENT * 2 + 1);

        let chunks = unicode_chunks(&text);

        assert_eq!(chunks.len(), 3);
        assert!(chunks.iter().all(|chunk| chunk.len() <= UNITS_PER_EVENT));
        assert_eq!(chunks.concat(), text.encode_utf16().collect::<Vec<_>>());
    }

    #[test]
    fn chunks_never_split_a_surrogate_pair() {
        // The emoji straddles the boundary: 19 units, then a two-unit character.
        let text = format!("{}🙂", "a".repeat(UNITS_PER_EVENT - 1));

        let chunks = unicode_chunks(&text);

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].len(), UNITS_PER_EVENT - 1);
        assert_eq!(chunks[1], "🙂".encode_utf16().collect::<Vec<_>>());
    }
}
