//! Delivery of completed text to user-facing platform destinations.

use std::time::Duration;

#[cfg(target_os = "ios")]
mod ios;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "ios")]
pub(crate) use ios::IosTextDelivery as PlatformTextDelivery;
#[cfg(target_os = "linux")]
pub(crate) use linux::LinuxTextDelivery as PlatformTextDelivery;
#[cfg(target_os = "macos")]
pub(crate) use macos::MacOsTextDelivery as PlatformTextDelivery;

#[cfg(not(any(target_os = "ios", target_os = "linux", target_os = "macos")))]
compile_error!("no text-delivery adapter exists for this target");

pub(crate) use aria_kit::transcription::Caret;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Insert,
    InsertAndSubmit,
    // Insertion that falls back to the clipboard. Typed text leaves the
    // clipboard alone. If insertion is refused before anything is typed, the
    // transcript is copied instead. A platform without insertion copies
    // outright.
    InsertOrCopy,
    Copy,
}

/// Where a delivery ended up. A request may allow either; a delivery did one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Inserted,
    Copied,
}

pub(crate) trait TextDelivery: Send + Sync + 'static {
    type Prepared: Send;

    const DEFAULT_MODE: OutputMode;

    // False where the display server serves the clipboard from the process
    // that set it.
    const OUTLIVES_PROCESS: bool;

    fn prepare(
        &self,
        mode: OutputMode,
        should_cancel: impl Fn() -> bool,
    ) -> Result<Self::Prepared, DeliveryError>;

    fn finish(
        &self,
        prepared: &Self::Prepared,
        should_cancel: impl Fn() -> bool,
    ) -> Result<(), DeliveryError>;

    fn deliver(
        &self,
        prepared: &Self::Prepared,
        text: &str,
        should_cancel: impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError>;

    /// The text beside the caret where `deliver` would type. Unknown unless
    /// the destination shows it.
    fn caret(&self, _prepared: &Self::Prepared) -> Caret {
        Caret::default()
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::frontmost_application;

#[cfg(not(target_os = "macos"))]
pub(crate) fn frontmost_application() -> Option<crate::history::Application> {
    None
}

const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);

fn validate_text(text: &str) -> Result<(), DeliveryError> {
    if text.is_empty() {
        return Err(DeliveryError::EmptyText);
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(DeliveryError::TextTooLarge {
            actual_bytes: text.len(),
            maximum_bytes: MAX_TEXT_BYTES,
        });
    }
    Ok(())
}

fn validate_timeout(timeout: Duration) -> Result<(), DeliveryError> {
    if timeout.is_zero() || timeout > MAX_DELIVERY_TIMEOUT {
        return Err(DeliveryError::InvalidTimeout {
            actual: timeout,
            maximum: MAX_DELIVERY_TIMEOUT,
        });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("delivery text must not be empty")]
    EmptyText,

    #[error("delivery text contains {actual_bytes} bytes; maximum is {maximum_bytes} bytes")]
    TextTooLarge {
        actual_bytes: usize,
        maximum_bytes: usize,
    },

    #[error("delivery timeout must be greater than zero and at most {maximum:?}; got {actual:?}")]
    InvalidTimeout { actual: Duration, maximum: Duration },

    #[error("the clipboard rejected the transcript")]
    ClipboardRejected,

    #[error("text delivery could not identify an available destination")]
    TargetUnavailable,

    #[error("the application, field, or caret position selected for insertion changed")]
    TargetChanged,

    #[error("insertion is not available on {}", std::env::consts::OS)]
    InsertionUnsupported,

    #[error("text delivery was cancelled")]
    Cancelled,

    #[error("text delivery exceeded its timeout of {0:?}")]
    Timeout(Duration),

    #[cfg(target_os = "macos")]
    #[error("could not confirm inserted text in the focused field; Return was not sent")]
    SubmissionUnconfirmed,

    #[cfg(target_os = "macos")]
    #[error(
        "insertion needs the Accessibility grant: System Settings → Privacy & Security \
         → Accessibility → switch on Aria"
    )]
    InsertionNotPermitted,

    #[cfg(target_os = "macos")]
    #[error(
        "a process holds secure keyboard entry, which discards typed text — often a \
         password field or Terminal's Secure Keyboard Entry; turn it off or use copy mode"
    )]
    SecureInput,
}
