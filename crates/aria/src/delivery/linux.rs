//! Delivery of completed text to the Linux system clipboard.

use std::sync::Mutex;
use std::time::Duration;

use arboard::{Clipboard, SetExtLinux};

use super::{
    DeliveryError, DeliveryOutcome, OutputMode, TextDelivery, validate_text, validate_timeout,
};

pub(crate) struct LinuxTextDelivery {
    clipboard: Mutex<Clipboard>,
}

impl LinuxTextDelivery {
    // The timeout is validated, then unused. Bounding a write needs the
    // connection arboard keeps to itself.
    pub(crate) fn new(timeout: Duration) -> Result<Self, DeliveryError> {
        validate_timeout(timeout)?;
        let clipboard = Clipboard::new().map_err(|error| {
            tracing::warn!(%error, "failed to reach the clipboard");
            DeliveryError::TargetUnavailable
        })?;
        Ok(Self {
            clipboard: Mutex::new(clipboard),
        })
    }
}

impl TextDelivery for LinuxTextDelivery {
    type Prepared = ();

    const DEFAULT_MODE: OutputMode = OutputMode::Copy;
    const OUTLIVES_PROCESS: bool = false;

    fn prepare(
        &self,
        mode: OutputMode,
        should_cancel: impl Fn() -> bool,
    ) -> Result<Self::Prepared, DeliveryError> {
        if should_cancel() {
            return Err(DeliveryError::Cancelled);
        }
        validate_mode(mode)
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
        let mut clipboard = self.clipboard.lock().unwrap_or_else(|poisoned| {
            // A panicked write leaves no broken state behind, so the handle is
            // safe to keep using.
            tracing::warn!("recovering the clipboard after a panicked write");
            poisoned.into_inner()
        });
        write_transcript(&mut clipboard, text)?;
        Ok(DeliveryOutcome::Copied)
    }
}

fn validate_mode(mode: OutputMode) -> Result<(), DeliveryError> {
    match mode {
        // No insertion here, so insert-or-copy goes straight to the clipboard.
        OutputMode::Copy | OutputMode::InsertOrCopy => Ok(()),
        // Synthetic key events need a compositor-specific helper Wayland does
        // not offer, so there is no destination to insert into.
        OutputMode::Insert | OutputMode::InsertAndSubmit => {
            Err(DeliveryError::InsertionUnsupported)
        }
    }
}

// Excluding from history asks clipboard managers not to record the transcript.
// It is a convention, not a guarantee.
fn write_transcript(clipboard: &mut Clipboard, text: &str) -> Result<(), DeliveryError> {
    clipboard
        .set()
        .exclude_from_history()
        .text(text)
        .map_err(|error| {
            tracing::warn!(%error, "the clipboard rejected the transcript");
            DeliveryError::ClipboardRejected
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clipboard_takes_every_mode_that_names_it() {
        assert!(validate_mode(OutputMode::Copy).is_ok());
        assert!(validate_mode(OutputMode::InsertOrCopy).is_ok());
        assert!(matches!(
            validate_mode(OutputMode::Insert),
            Err(DeliveryError::InsertionUnsupported)
        ));
        assert!(matches!(
            validate_mode(OutputMode::InsertAndSubmit),
            Err(DeliveryError::InsertionUnsupported)
        ));
    }
}
