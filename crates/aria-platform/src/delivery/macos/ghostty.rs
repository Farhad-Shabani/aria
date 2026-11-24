//! Text into Ghostty as a paste, through its scripting interface.
//!
//! Ghostty handles `input text` as it handles Cmd-V, without the clipboard
//! and without its paste confirmation: a program that asked for bracketed
//! paste gets the text between paste markers. A modal editor or a vim-mode
//! prompt then reads it as text in any mode, where typed keys would run as
//! its commands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cidre::ax;
use objc2::rc::{Retained, autoreleasepool};
use objc2_core_services::AEDeterminePermissionToAutomateTarget;
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventSendOptions, NSString};

use super::super::DeliveryError;
use super::{NO_ERR, Undelivered, anchor, element};

pub(super) const BUNDLE_ID: &str = "com.mitchellh.ghostty";

/// The budget for one Apple Event to Ghostty, and for one pass of
/// Accessibility reads. Ghostty answers on its main thread, so a busy Ghostty
/// must not hold a press.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);

const fn code(name: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*name)
}

// Ghostty.sdef.
const SUITE: u32 = code(b"Ghst");
const INPUT_TEXT: u32 = code(b"InTx");
const INPUT_TARGET: u32 = code(b"GItT");
const TERMINAL: u32 = code(b"Gtrm");
const FRONT_WINDOW: u32 = code(b"GFWn");
const SELECTED_TAB: u32 = code(b"GWsT");
const FOCUSED_TERMINAL: u32 = code(b"GTfT");
const UNIQUE_ID: u32 = code(b"ID  ");

// The Apple Event core suite and object specifiers.
const CORE_SUITE: u32 = code(b"core");
const GET_DATA: u32 = code(b"getd");
const DIRECT_OBJECT: u32 = code(b"----");
const ERROR_NUMBER: u32 = code(b"errn");
const OBJECT_SPECIFIER: u32 = code(b"obj ");
const DESIRED_CLASS: u32 = code(b"want");
const KEY_FORM: u32 = code(b"form");
const KEY_DATA: u32 = code(b"seld");
const CONTAINER: u32 = code(b"from");
const PROPERTY: u32 = code(b"prop");
const AUTO_RETURN_ID: i16 = -1;
const ANY_TRANSACTION: i32 = 0;

// OSStatus values.
const NO_SUCH_OBJECT: isize = -1728;
const TIMED_OUT: isize = -1712;
const CONNECTION_INVALID: isize = -609;
const WOULD_REQUIRE_CONSENT: i32 = -1744;

/// One consent prompt at a time, however many presses find permission
/// undecided while it shows.
static ASKING: AtomicBool = AtomicBool::new(false);

/// The Ghostty terminal surface a press started in, by the stable ID its
/// scripting interface gives.
pub(super) struct Terminal {
    id: String,
}

impl Terminal {
    /// The terminal holding keyboard focus in Ghostty `process`. Never waits
    /// on the person: undecided permission asks in the background and fails
    /// this call.
    pub(super) fn focused(process: u32) -> Result<Self, &'static str> {
        let target = target(process)?;
        permit(&target, process)?;
        match focused_id(&target, process) {
            Ok(id) => Ok(Self { id }),
            Err(Lookup::Elsewhere(reason) | Lookup::Unanswered(reason)) => Err(reason),
        }
    }

    /// Refuses unless keyboard focus is still in this terminal. Another tab
    /// of the same window counts as elsewhere.
    pub(super) fn check_focus(&self, process: u32) -> Result<(), DeliveryError> {
        let current = target(process)
            .map_err(Lookup::Unanswered)
            .and_then(|target| focused_id(&target, process));
        focus_verdict(&self.id, current)
    }

    pub(super) fn paste(
        &self,
        process: u32,
        text: &str,
        timeout: Duration,
    ) -> Result<(), Undelivered> {
        let refused = || Undelivered::Refused(DeliveryError::TargetUnavailable);
        let target = target(process).map_err(|_| refused())?;
        let terminal = specifier(
            TERMINAL,
            UNIQUE_ID,
            &NSAppleEventDescriptor::descriptorWithString(&NSString::from_str(&self.id)),
            &NSAppleEventDescriptor::nullDescriptor(),
        )
        .ok_or_else(refused)?;
        let event = event(SUITE, INPUT_TEXT, &target);
        event.setParamDescriptor_forKeyword(
            &NSAppleEventDescriptor::descriptorWithString(&NSString::from_str(text)),
            DIRECT_OBJECT,
        );
        event.setParamDescriptor_forKeyword(&terminal, INPUT_TARGET);
        send(&event, timeout).map(drop).map_err(sent_error)
    }
}

/// Why the focused terminal is unknown.
enum Lookup {
    /// Keyboard focus is somewhere text must not go, or Accessibility can't
    /// show where it is.
    Elsewhere(&'static str),
    /// Ghostty's scripting interface didn't answer, or may no longer be
    /// driven.
    Unanswered(&'static str),
}

/// A focus somewhere else waits for the person to come back. A scripting
/// interface that can't be asked won't answer later either, so the press
/// stops.
fn focus_verdict(pinned: &str, current: Result<String, Lookup>) -> Result<(), DeliveryError> {
    match current {
        Ok(id) if id == pinned => Ok(()),
        Ok(_) => Err(anchor::target_changed("another Ghostty terminal has focus")),
        Err(Lookup::Elsewhere(reason)) => Err(anchor::target_changed(reason)),
        Err(Lookup::Unanswered(reason)) => {
            tracing::warn!(reason, "Ghostty can't confirm its focused terminal");
            Err(DeliveryError::TargetUnavailable)
        }
    }
}

/// Without a reply, or with the connection lost, Ghostty may still have
/// pasted the text. Any refusal it answered with pasted none.
fn sent_error(status: isize) -> Undelivered {
    if matches!(status, TIMED_OUT | CONNECTION_INVALID) {
        return Undelivered::Uncertain(DeliveryError::PasteUnconfirmed);
    }
    tracing::warn!(status, "Ghostty refused the text");
    Undelivered::Refused(DeliveryError::TargetUnavailable)
}

/// The terminal keys would reach now. Ghostty's scripting interface only
/// names the focused terminal of its front terminal window, which leaves out
/// its quick terminal and windows that aren't terminals. So Accessibility
/// must agree that this window has keyboard focus.
fn focused_id(target: &NSAppleEventDescriptor, process: u32) -> Result<String, Lookup> {
    let app = NSAppleEventDescriptor::nullDescriptor();
    let terminal = property(FRONT_WINDOW, &app)
        .and_then(|window| property(SELECTED_TAB, &window))
        .and_then(|tab| property(FOCUSED_TERMINAL, &tab))
        .and_then(|terminal| property(UNIQUE_ID, &terminal))
        .ok_or(Lookup::Unanswered("focused terminal unaddressable"))?;
    let id = get_text(target, &terminal)?;
    let key = key_window(process).map_err(Lookup::Elsewhere)?;
    is_front_terminal(&key).map_err(Lookup::Elsewhere)?;
    Ok(id)
}

struct KeyWindow {
    standard: bool,
    frontmost: bool,
    terminal_focused: bool,
}

/// Titles can't identify a window: a quick terminal can share its front
/// window's title.
fn is_front_terminal(key: &KeyWindow) -> Result<(), &'static str> {
    if !key.standard {
        return Err("Ghostty's key window is a panel, such as its quick terminal");
    }
    if !key.frontmost {
        return Err("another Ghostty window is in front of its key window");
    }
    if !key.terminal_focused {
        return Err("Ghostty's key window is not a terminal");
    }
    Ok(())
}

fn key_window(process: u32) -> Result<KeyWindow, &'static str> {
    let deadline = Instant::now() + LOOKUP_TIMEOUT;
    let pid = i32::try_from(process).map_err(|_| "process id out of range")?;
    let app = ax::UiElement::with_app_pid(pid);
    element::limit(&app, deadline)?;
    let window = app
        .attr_value(ax::attr::focused_window())
        .map_err(|_| "no key window")?;
    let frontmost = element::is_first_window(&app, &window, deadline)?;
    let window = element::ui_element(&window).ok_or("key window is not an element")?;
    element::limit(window, deadline)?;
    let standard = window
        .attr_value(ax::attr::subrole())
        .is_ok_and(|subrole| subrole.equal(ax::sub_role::standard_window()));
    let field = element::focused_field(&app, deadline)?;
    element::limit(&field, deadline)?;
    let terminal_focused = field
        .role()
        .is_ok_and(|role| role.equal(ax::role::text_area()));
    Ok(KeyWindow {
        standard,
        frontmost,
        terminal_focused,
    })
}

fn target(process: u32) -> Result<Retained<NSAppleEventDescriptor>, &'static str> {
    let pid = i32::try_from(process).map_err(|_| "process id out of range")?;
    Ok(NSAppleEventDescriptor::descriptorWithProcessIdentifier(pid))
}

fn permit(target: &NSAppleEventDescriptor, process: u32) -> Result<(), &'static str> {
    match permission(target, false) {
        NO_ERR => Ok(()),
        WOULD_REQUIRE_CONSENT => {
            ask_consent(process);
            Err("waiting for permission to drive Ghostty")
        }
        status => {
            tracing::debug!(status, "Ghostty automation unavailable");
            Err("not permitted to drive Ghostty")
        }
    }
}

/// Shows macOS's consent prompt on a detached thread. It holds nothing but
/// the prompt and ends when the person answers. The next press reads the
/// answer.
fn ask_consent(process: u32) {
    if ASKING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("aria-ghostty-consent".into())
        .spawn(move || {
            let status = autoreleasepool(|_| {
                target(process).map_or(NO_ERR, |target| permission(&target, true))
            });
            tracing::info!(status, "Ghostty automation answered");
            ASKING.store(false, Ordering::Release);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not ask to drive Ghostty");
        ASKING.store(false, Ordering::Release);
    }
}

#[allow(
    unsafe_code,
    reason = "a CoreServices call that reads the target through a raw pointer; \
              the invariant lives on the SAFETY comment below"
)]
fn permission(target: &NSAppleEventDescriptor, ask: bool) -> i32 {
    // SAFETY: `aeDesc` points into `target`, which outlives the call.
    unsafe { AEDeterminePermissionToAutomateTarget(target.aeDesc(), SUITE, INPUT_TEXT, ask) }
}

fn get_text(
    target: &NSAppleEventDescriptor,
    specifier: &NSAppleEventDescriptor,
) -> Result<String, Lookup> {
    let event = event(CORE_SUITE, GET_DATA, target);
    event.setParamDescriptor_forKeyword(specifier, DIRECT_OBJECT);
    let reply = send(&event, LOOKUP_TIMEOUT).map_err(|status| {
        tracing::debug!(status, "Ghostty did not answer");
        // No such window, tab, or terminal: focus is on none of Ghostty's
        // terminals.
        if status == NO_SUCH_OBJECT {
            Lookup::Elsewhere("Ghostty has no terminal window")
        } else {
            Lookup::Unanswered("Ghostty did not answer")
        }
    })?;
    reply
        .paramDescriptorForKeyword(DIRECT_OBJECT)
        .and_then(|value| value.stringValue())
        .map(|text| text.to_string())
        .ok_or(Lookup::Unanswered("Ghostty answered without text"))
}

/// Sends `event` and waits for the reply, which may carry an error of its own.
fn send(
    event: &NSAppleEventDescriptor,
    timeout: Duration,
) -> Result<Retained<NSAppleEventDescriptor>, isize> {
    let reply = event
        .sendEventWithOptions_timeout_error(
            NSAppleEventSendOptions::WaitForReply | NSAppleEventSendOptions::NeverInteract,
            timeout.as_secs_f64(),
        )
        .map_err(|error| error.code())?;
    match reply
        .paramDescriptorForKeyword(ERROR_NUMBER)
        .map(|status| status.int32Value())
    {
        Some(status) if status != NO_ERR => Err(status as isize),
        _ => Ok(reply),
    }
}

fn event(class: u32, id: u32, target: &NSAppleEventDescriptor) -> Retained<NSAppleEventDescriptor> {
    NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
        class,
        id,
        Some(target),
        AUTO_RETURN_ID,
        ANY_TRANSACTION,
    )
}

fn property(
    name: u32,
    container: &NSAppleEventDescriptor,
) -> Option<Retained<NSAppleEventDescriptor>> {
    specifier(
        PROPERTY,
        PROPERTY,
        &NSAppleEventDescriptor::descriptorWithTypeCode(name),
        container,
    )
}

fn specifier(
    class: u32,
    form: u32,
    data: &NSAppleEventDescriptor,
    container: &NSAppleEventDescriptor,
) -> Option<Retained<NSAppleEventDescriptor>> {
    let record = NSAppleEventDescriptor::recordDescriptor();
    record.setDescriptor_forKeyword(
        &NSAppleEventDescriptor::descriptorWithTypeCode(class),
        DESIRED_CLASS,
    );
    record.setDescriptor_forKeyword(
        &NSAppleEventDescriptor::descriptorWithEnumCode(form),
        KEY_FORM,
    );
    record.setDescriptor_forKeyword(data, KEY_DATA);
    record.setDescriptor_forKeyword(container, CONTAINER);
    record.coerceToDescriptorType(OBJECT_SPECIFIER)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRONT_TERMINAL: KeyWindow = KeyWindow {
        standard: true,
        frontmost: true,
        terminal_focused: true,
    };

    #[test]
    fn text_goes_to_ghostty_only_when_its_key_window_is_the_front_terminal_window() {
        assert!(is_front_terminal(&FRONT_TERMINAL).is_ok());
        let quick_terminal = KeyWindow {
            standard: false,
            ..FRONT_TERMINAL
        };
        let behind_a_floating_window = KeyWindow {
            frontmost: false,
            ..FRONT_TERMINAL
        };
        let about_window = KeyWindow {
            terminal_focused: false,
            ..FRONT_TERMINAL
        };
        for key in [quick_terminal, behind_a_floating_window, about_window] {
            assert!(is_front_terminal(&key).is_err());
        }
    }

    #[test]
    fn focus_elsewhere_waits_and_a_silent_ghostty_stops_the_press() {
        assert!(focus_verdict("A1", Ok("A1".into())).is_ok());
        assert!(matches!(
            focus_verdict("A1", Ok("B2".into())),
            Err(DeliveryError::TargetChanged)
        ));
        assert!(matches!(
            focus_verdict("A1", Err(Lookup::Elsewhere("quick terminal"))),
            Err(DeliveryError::TargetChanged)
        ));
        assert!(matches!(
            focus_verdict("A1", Err(Lookup::Unanswered("timed out"))),
            Err(DeliveryError::TargetUnavailable)
        ));
    }

    #[test]
    fn only_a_missing_reply_or_lost_connection_may_have_delivered_the_text() {
        for status in [TIMED_OUT, CONNECTION_INVALID] {
            assert!(matches!(
                sent_error(status),
                Undelivered::Uncertain(DeliveryError::PasteUnconfirmed)
            ));
        }
        assert!(matches!(
            sent_error(-10000),
            Undelivered::Refused(DeliveryError::TargetUnavailable)
        ));
    }
}
