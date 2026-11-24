//! The Apple Events that drive a scriptable terminal and ask it about its
//! focus.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use objc2::rc::{Retained, autoreleasepool};
use objc2_core_services::AEDeterminePermissionToAutomateTarget;
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventSendOptions};

use super::super::DeliveryError;
use super::focus::{LOOKUP_TIMEOUT, Lookup};
use super::{NO_ERR, Undelivered};

pub(super) const fn code(name: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*name)
}

pub(super) const DIRECT_OBJECT: u32 = code(b"----");
pub(super) const CORE_SUITE: u32 = code(b"core");
pub(super) const GET_DATA: u32 = code(b"getd");
const ERROR_NUMBER: u32 = code(b"errn");
const OBJECT_SPECIFIER: u32 = code(b"obj ");
const DESIRED_CLASS: u32 = code(b"want");
const KEY_FORM: u32 = code(b"form");
const KEY_DATA: u32 = code(b"seld");
const CONTAINER: u32 = code(b"from");
const PROPERTY: u32 = code(b"prop");
const ABSOLUTE_POSITION: u32 = code(b"indx");
const AUTO_RETURN_ID: i16 = -1;
const ANY_TRANSACTION: i32 = 0;

// OSStatus values.
const NO_SUCH_OBJECT: isize = -1728;
const TIMED_OUT: isize = -1712;
const WAIT_CANCELED: isize = -1711;
const CONNECTION_INVALID: isize = -609;
const WOULD_REQUIRE_CONSENT: i32 = -1744;

/// One consent prompt at a time, however many presses find permission
/// undecided while it shows.
static ASKING: AtomicBool = AtomicBool::new(false);

/// Without a reply, with the wait cut short, or with the connection lost, the
/// terminal may still have taken the text. Any refusal it answered with took
/// none.
pub(super) fn sent_error(status: isize) -> Undelivered {
    if matches!(status, TIMED_OUT | WAIT_CANCELED | CONNECTION_INVALID) {
        return Undelivered::Uncertain(DeliveryError::PasteUnconfirmed);
    }
    tracing::warn!(status, "the terminal refused the text");
    Undelivered::Refused(DeliveryError::TargetUnavailable)
}

pub(super) fn target(process: u32) -> Result<Retained<NSAppleEventDescriptor>, &'static str> {
    let pid = i32::try_from(process).map_err(|_| "process id out of range")?;
    Ok(NSAppleEventDescriptor::descriptorWithProcessIdentifier(pid))
}

/// Whether Aria may send `event`, a class and ID, to `target`. Never waits on
/// the person: undecided permission asks in the background and fails this
/// call.
pub(super) fn permit(
    target: &NSAppleEventDescriptor,
    process: u32,
    event: (u32, u32),
) -> Result<(), &'static str> {
    match permission(target, event, false) {
        NO_ERR => Ok(()),
        WOULD_REQUIRE_CONSENT => {
            ask_consent(process, event);
            Err("waiting for permission to drive the terminal")
        }
        status => {
            tracing::debug!(status, "terminal automation unavailable");
            Err("not permitted to drive the terminal")
        }
    }
}

/// Shows macOS's consent prompt on a detached thread. It holds nothing but
/// the prompt and ends when the person answers. The next press reads the
/// answer.
fn ask_consent(process: u32, event: (u32, u32)) {
    if ASKING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("aria-automation-consent".into())
        .spawn(move || {
            let status = autoreleasepool(|_| {
                target(process).map_or(NO_ERR, |target| permission(&target, event, true))
            });
            tracing::info!(status, "terminal automation answered");
            ASKING.store(false, Ordering::Release);
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not ask to drive the terminal");
        ASKING.store(false, Ordering::Release);
    }
}

#[allow(
    unsafe_code,
    reason = "a CoreServices call that reads the target through a raw pointer; \
              the invariant lives on the SAFETY comment below"
)]
fn permission(target: &NSAppleEventDescriptor, (class, id): (u32, u32), ask: bool) -> i32 {
    // SAFETY: `aeDesc` points into `target`, which outlives the call.
    unsafe { AEDeterminePermissionToAutomateTarget(target.aeDesc(), class, id, ask) }
}

pub(super) fn get_text(
    target: &NSAppleEventDescriptor,
    specifier: &NSAppleEventDescriptor,
) -> Result<String, Lookup> {
    let event = event(CORE_SUITE, GET_DATA, target);
    event.setParamDescriptor_forKeyword(specifier, DIRECT_OBJECT);
    let reply = send(&event, LOOKUP_TIMEOUT).map_err(|status| {
        tracing::debug!(status, "the terminal did not answer");
        // No such window, tab, or terminal: focus is on none of its
        // terminals.
        if status == NO_SUCH_OBJECT {
            Lookup::Elsewhere("the terminal has no window")
        } else {
            Lookup::Unanswered("the terminal did not answer")
        }
    })?;
    reply
        .paramDescriptorForKeyword(DIRECT_OBJECT)
        .and_then(|value| value.stringValue())
        .map(|text| text.to_string())
        .ok_or(Lookup::Unanswered("the terminal answered without text"))
}

/// Sends `event` and waits for the reply, which may carry an error of its own.
pub(super) fn send(
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

pub(super) fn event(
    class: u32,
    id: u32,
    target: &NSAppleEventDescriptor,
) -> Retained<NSAppleEventDescriptor> {
    NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
        class,
        id,
        Some(target),
        AUTO_RETURN_ID,
        ANY_TRANSACTION,
    )
}

pub(super) fn property(
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

pub(super) fn specifier(
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

/// The element at a 1-based position, front to back for windows.
pub(super) fn element_at(
    class: u32,
    position: i32,
    container: &NSAppleEventDescriptor,
) -> Option<Retained<NSAppleEventDescriptor>> {
    specifier(
        class,
        ABSOLUTE_POSITION,
        &NSAppleEventDescriptor::descriptorWithInt32(position),
        container,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_missing_reply_or_lost_connection_may_have_delivered_the_text() {
        for status in [TIMED_OUT, WAIT_CANCELED, CONNECTION_INVALID] {
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
