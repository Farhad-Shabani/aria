//! Linux delivery puts the transcript on the clipboard and primary selection,
//! then sends Shift+Insert to paste into the focused app.

mod display;
mod portal;
mod x11;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use self::display::{DisplayThread, JobError, Refusal};
use super::{
    DeliveryError, DeliveryOutcome, DisplayServer, OutputMode, TextDelivery, validate_text,
    validate_timeout,
};

/// The portal uses this ID to find the desktop entry installed by `aria service install`.
pub(crate) const APP_ID: &str = "io.github.farhad_shabani.aria";

/// Shares one display connection across deliveries and connects on first use.
/// Missing display variables require a daemon restart before delivery can work.
#[derive(Clone)]
pub(crate) struct Desktop {
    display: Arc<DisplayThread>,
}

impl Desktop {
    /// `home` keeps the portal's restore token.
    pub(crate) fn new(home: &Path, server: DisplayServer) -> Self {
        tracing::info!(?server, "display server");
        Self {
            display: Arc::new(DisplayThread::new(server, home)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinuxTarget {
    Clipboard,
    Insert,
    InsertOrCopy,
}

pub(crate) struct LinuxTextDelivery {
    display: Arc<DisplayThread>,
    timeout: Duration,
}

impl LinuxTextDelivery {
    pub(crate) fn new(timeout: Duration, desktop: &Desktop) -> Result<Self, DeliveryError> {
        validate_timeout(timeout)?;
        Ok(Self {
            display: Arc::clone(&desktop.display),
            timeout,
        })
    }

    fn copy(
        &self,
        text: &str,
        timeout: Duration,
        should_cancel: &impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        self.display
            .copy(text, timeout, should_cancel)
            .map(|()| DeliveryOutcome::Copied)
            .map_err(|failure| delivery_error(failure, self.timeout))
    }

    /// Waits out a slow connection, since nothing else can deliver the text.
    fn insert(
        &self,
        text: &str,
        should_cancel: &impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        let deadline = Instant::now() + self.timeout;
        let pasted = self
            .display
            .await_keyboard(self.timeout, should_cancel)
            .and_then(|()| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                self.display.paste(text, remaining, false, should_cancel)
            });
        pasted
            .map(|()| DeliveryOutcome::Inserted)
            .map_err(|failure| delivery_error(failure, self.timeout))
    }

    /// Copying after paste keys are sent could deliver the text twice.
    fn insert_or_copy(
        &self,
        text: &str,
        should_cancel: &impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        let deadline = Instant::now() + self.timeout;
        let remaining = || deadline.saturating_duration_since(Instant::now());
        match self.display.await_keyboard(Duration::ZERO, should_cancel) {
            Err(JobError::Cancelled) => return Err(DeliveryError::Cancelled),
            Err(failure) => {
                tracing::info!(?failure, "the transcript is copied instead of pasted");
                return self.copy(text, remaining(), should_cancel);
            }
            Ok(()) => {}
        }
        match self.display.paste(text, remaining(), true, should_cancel) {
            Ok(()) => Ok(DeliveryOutcome::Inserted),
            Err(JobError::PrimaryRejected) => Ok(DeliveryOutcome::Copied),
            Err(JobError::Refused(refusal)) => {
                tracing::info!(?refusal, "the transcript is copied instead of pasted");
                self.copy(text, remaining(), should_cancel)
            }
            Err(failure) => Err(delivery_error(failure, self.timeout)),
        }
    }
}

impl TextDelivery for LinuxTextDelivery {
    type Prepared = LinuxTarget;

    const DEFAULT_MODE: OutputMode = OutputMode::InsertOrCopy;
    const OUTLIVES_PROCESS: bool = false;
    const STREAMS: bool = false;

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
            OutputMode::Copy => Ok(LinuxTarget::Clipboard),
            // Nothing confirms that the paste landed before Return would.
            OutputMode::InsertAndSubmit => Err(DeliveryError::SubmissionUnsupported),
            OutputMode::Insert => self
                .display
                .start_connecting()
                .map(|()| LinuxTarget::Insert)
                .map_err(refusal_error),
            OutputMode::InsertOrCopy => Ok(match self.display.start_connecting() {
                Ok(()) => LinuxTarget::InsertOrCopy,
                Err(refusal) => {
                    tracing::info!(?refusal, "the transcript is copied instead of pasted");
                    LinuxTarget::Clipboard
                }
            }),
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
        prepared: &Self::Prepared,
        text: &str,
        should_cancel: impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        validate_text(text)?;
        if should_cancel() {
            return Err(DeliveryError::Cancelled);
        }
        match prepared {
            LinuxTarget::Clipboard => self.copy(text, self.timeout, &should_cancel),
            LinuxTarget::Insert => self.insert(text, &should_cancel),
            LinuxTarget::InsertOrCopy => self.insert_or_copy(text, &should_cancel),
        }
    }
}

fn refusal_error(refusal: Refusal) -> DeliveryError {
    match refusal {
        Refusal::NoDisplay | Refusal::Failed => DeliveryError::TargetUnavailable,
        Refusal::Unsupported => DeliveryError::InsertionUnsupported,
        Refusal::NotPermitted => DeliveryError::InsertionNotPermitted,
    }
}

fn delivery_error(failure: JobError, timeout: Duration) -> DeliveryError {
    match failure {
        JobError::Refused(refusal) => refusal_error(refusal),
        JobError::ClipboardRejected | JobError::PrimaryRejected => DeliveryError::ClipboardRejected,
        JobError::TimedOut => DeliveryError::Timeout(timeout),
        JobError::Cancelled => DeliveryError::Cancelled,
        JobError::Unconfirmed => DeliveryError::PasteUnconfirmed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn delivery() -> LinuxTextDelivery {
        let desktop = Desktop::new(Path::new("/nonexistent"), DisplayServer::Absent);
        LinuxTextDelivery::new(TIMEOUT, &desktop).expect("a valid timeout")
    }

    #[test]
    fn without_a_keyboard_insert_or_copy_copies_and_insert_fails() {
        let delivery = delivery();
        assert_eq!(
            delivery
                .prepare(OutputMode::InsertOrCopy, || false)
                .unwrap(),
            LinuxTarget::Clipboard
        );
        assert!(matches!(
            delivery.prepare(OutputMode::Insert, || false),
            Err(DeliveryError::TargetUnavailable)
        ));
    }

    #[test]
    fn submitting_is_refused_as_submission_not_insertion() {
        assert!(matches!(
            delivery().prepare(OutputMode::InsertAndSubmit, || false),
            Err(DeliveryError::SubmissionUnsupported)
        ));
    }

    #[test]
    fn with_no_display_a_delivery_fails_alone() {
        assert!(matches!(
            delivery().deliver(&LinuxTarget::Clipboard, "text", || false),
            Err(DeliveryError::TargetUnavailable)
        ));
    }

    #[test]
    fn each_failure_keeps_its_own_error() {
        assert!(matches!(
            delivery_error(JobError::TimedOut, TIMEOUT),
            DeliveryError::Timeout(TIMEOUT)
        ));
        assert!(matches!(
            delivery_error(JobError::Unconfirmed, TIMEOUT),
            DeliveryError::PasteUnconfirmed
        ));
        assert!(matches!(
            delivery_error(JobError::Refused(Refusal::NotPermitted), TIMEOUT),
            DeliveryError::InsertionNotPermitted
        ));
    }
}
