//! Delivery of completed text to the iOS system pasteboard.

use std::time::Duration;

use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSArray, NSString};
use objc2_ui_kit::UIPasteboard;

use super::{
    DeliveryError, DeliveryOutcome, Desktop, OutputMode, TextDelivery, validate_text,
    validate_timeout,
};

pub(crate) struct IosTextDelivery;

impl IosTextDelivery {
    // The pasteboard write returns immediately, so nothing here waits on a
    // deadline. The bound is still checked to keep one meaning for the
    // configured timeout across platforms.
    pub(crate) fn new(timeout: Duration, _desktop: &Desktop) -> Result<Self, DeliveryError> {
        validate_timeout(timeout)?;
        Ok(Self)
    }
}

impl TextDelivery for IosTextDelivery {
    type Prepared = ();

    const DEFAULT_MODE: OutputMode = OutputMode::Copy;
    const OUTLIVES_PROCESS: bool = true;

    fn frontmost(&self) -> Option<crate::history::Application> {
        None
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
            // No insertion here, so insert-or-copy goes straight to the
            // pasteboard.
            OutputMode::Copy | OutputMode::InsertOrCopy => Ok(()),
            // No iOS app can type into another app.
            OutputMode::Insert | OutputMode::InsertAndSubmit => {
                Err(DeliveryError::InsertionUnsupported)
            }
        }
    }

    fn finish(
        &self,
        _prepared: &Self::Prepared,
        _should_cancel: impl Fn() -> bool,
    ) -> Result<(), DeliveryError> {
        Ok(())
    }

    fn deliver(
        &self,
        _prepared: &Self::Prepared,
        text: &str,
        should_cancel: impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        validate_text(text)?;
        if should_cancel() {
            return Err(DeliveryError::Cancelled);
        }
        write_transcript(&UIPasteboard::generalPasteboard(), text);
        Ok(DeliveryOutcome::Copied)
    }
}

// Writing the objects replaces everything already on the pasteboard, so no
// earlier representation survives underneath. `local_only` keeps the
// transcript off Universal Clipboard, which is what stops it reaching the
// user's other devices.
fn write_transcript(pasteboard: &UIPasteboard, text: &str) {
    let text = NSString::from_str(text);
    let items = NSArray::from_slice(&[ProtocolObject::from_ref(&*text)]);
    pasteboard.setObjects_localOnly_expirationDate(&items, true, None);
}
