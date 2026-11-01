//! Linux delivery puts the transcript on the clipboard and primary selection,
//! then sends Shift+Insert to paste into the focused app. This file decides
//! what each mode does when that fails; `desktop` does the work.

mod desktop;
mod portal;
mod x11;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use self::desktop::{AfterFailure, Budget, Desktop, JobError, KeyboardState, Refusal};
use super::{
    DeliveryError, DeliveryOutcome, OutputMode, TextDelivery, validate_text, validate_timeout,
};

/// The portal uses this ID to find the desktop entry installed by `aria service install`.
pub(crate) const APP_ID: &str = "io.github.farhad_shabani.aria";

/// Shares one desktop across deliveries.
#[derive(Clone)]
pub(super) struct Session {
    desktop: Arc<Desktop>,
}

impl Session {
    pub(super) fn detect(home: &Path) -> Self {
        // Unit tests must not connect to the developer's desktop.
        let server = if cfg!(test) {
            DisplayServer::Absent
        } else {
            DisplayServer::capture()
        };
        Self {
            desktop: Arc::new(Desktop::new(home, server)),
        }
    }

    pub(super) fn create(&self, timeout: Duration) -> Result<LinuxTextDelivery, DeliveryError> {
        validate_timeout(timeout)?;
        Ok(LinuxTextDelivery {
            desktop: Arc::clone(&self.desktop),
            timeout,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayServer {
    Wayland,
    X11,
    Absent,
}

impl DisplayServer {
    fn capture() -> Self {
        let set = |name| std::env::var_os(name).is_some_and(|value| !value.is_empty());
        Self::named(set("WAYLAND_DISPLAY"), set("DISPLAY"))
    }

    /// Wayland wins: XWayland sets `DISPLAY` beside it.
    fn named(wayland: bool, x11: bool) -> Self {
        if wayland {
            Self::Wayland
        } else if x11 {
            Self::X11
        } else {
            Self::Absent
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinuxDeliveryTarget {
    Application,
    ApplicationOrClipboard,
    Clipboard,
}

pub(crate) struct LinuxTextDelivery {
    desktop: Arc<Desktop>,
    timeout: Duration,
}

impl LinuxTextDelivery {
    fn copy(&self, text: &str, budget: &Budget<'_>) -> Result<DeliveryOutcome, DeliveryError> {
        self.desktop
            .copy(text, budget)
            .map(|()| DeliveryOutcome::Copied)
            .map_err(|failure| delivery_error(failure, self.timeout))
    }

    /// What refused the insertion is the actionable cause, so a clipboard
    /// rejection is logged rather than returned in its place.
    fn copy_instead(
        &self,
        text: &str,
        refusal: Refusal,
        budget: &Budget<'_>,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        tracing::info!(?refusal, "the transcript is copied instead of pasted");
        self.copy(text, budget).map_err(|error| match error {
            DeliveryError::ClipboardRejected => refusal_error(refusal),
            error => error,
        })
    }

    /// Waits out a slow connection, since nothing else can deliver the text.
    fn insert(&self, text: &str, budget: &Budget<'_>) -> Result<DeliveryOutcome, DeliveryError> {
        self.desktop
            .await_keyboard(budget)
            .and_then(|()| self.desktop.paste(text, AfterFailure::Withdraw, budget))
            .map(|()| DeliveryOutcome::Inserted)
            .map_err(|failure| delivery_error(failure, self.timeout))
    }

    /// Copying after paste keys are sent could deliver the text twice.
    fn insert_or_copy(
        &self,
        text: &str,
        budget: &Budget<'_>,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        match self.desktop.keyboard() {
            KeyboardState::Ready => {}
            KeyboardState::Connecting => {
                tracing::info!("the transcript is copied while the keyboard connects");
                return self.copy(text, budget);
            }
            KeyboardState::Refused(refusal) => return self.copy_instead(text, refusal, budget),
        }
        let Err(failure) = self.desktop.paste(text, AfterFailure::Keep, budget) else {
            return Ok(DeliveryOutcome::Inserted);
        };
        match failed_paste(failure) {
            FailedPaste::AlreadyCopied => Ok(DeliveryOutcome::Copied),
            FailedPaste::CopyInstead(refusal) => self.copy_instead(text, refusal, budget),
            FailedPaste::Fail(failure) => Err(delivery_error(failure, self.timeout)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailedPaste {
    /// As far as Aria can tell, the text is on the clipboard and no paste went out.
    AlreadyCopied,
    /// Nothing was written yet.
    CopyInstead(Refusal),
    /// A copy now could deliver the text twice, could not succeed, or is unwanted.
    Fail(JobError),
}

fn failed_paste(failure: JobError) -> FailedPaste {
    match failure {
        JobError::PrimaryRejected | JobError::NotSent => FailedPaste::AlreadyCopied,
        JobError::Refused(refusal) => FailedPaste::CopyInstead(refusal),
        JobError::ClipboardRejected
        | JobError::TimedOut
        | JobError::Cancelled
        | JobError::Unconfirmed => FailedPaste::Fail(failure),
    }
}

impl TextDelivery for LinuxTextDelivery {
    type Prepared = LinuxDeliveryTarget;

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
            OutputMode::Copy => Ok(LinuxDeliveryTarget::Clipboard),
            // Nothing confirms that the paste landed before Return would.
            OutputMode::InsertAndSubmit => Err(DeliveryError::SubmissionUnsupported),
            OutputMode::Insert => match self.desktop.keyboard() {
                KeyboardState::Refused(refusal) => Err(refusal_error(refusal)),
                KeyboardState::Ready | KeyboardState::Connecting => {
                    Ok(LinuxDeliveryTarget::Application)
                }
            },
            OutputMode::InsertOrCopy => Ok(match self.desktop.keyboard() {
                KeyboardState::Refused(refusal) => {
                    tracing::info!(?refusal, "the transcript is copied instead of pasted");
                    LinuxDeliveryTarget::Clipboard
                }
                KeyboardState::Ready | KeyboardState::Connecting => {
                    LinuxDeliveryTarget::ApplicationOrClipboard
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
        let budget = Budget::new(self.timeout, &should_cancel);
        match prepared {
            LinuxDeliveryTarget::Clipboard => self.copy(text, &budget),
            LinuxDeliveryTarget::Application => self.insert(text, &budget),
            LinuxDeliveryTarget::ApplicationOrClipboard => self.insert_or_copy(text, &budget),
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
        JobError::NotSent => DeliveryError::TargetUnavailable,
        JobError::TimedOut => DeliveryError::Timeout(timeout),
        JobError::Cancelled => DeliveryError::Cancelled,
        JobError::Unconfirmed => DeliveryError::PasteUnconfirmed,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{DeliveryFactory, MAX_DELIVERY_TIMEOUT};
    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(5);

    fn delivery() -> LinuxTextDelivery {
        DeliveryFactory::detect(Path::new("/nonexistent"))
            .create(TIMEOUT)
            .expect("a valid timeout")
    }

    #[test]
    fn wayland_wins_over_the_xwayland_display() {
        assert_eq!(DisplayServer::named(true, true), DisplayServer::Wayland);
        assert_eq!(DisplayServer::named(false, true), DisplayServer::X11);
        assert_eq!(DisplayServer::named(false, false), DisplayServer::Absent);
    }

    #[test]
    fn factory_clones_share_the_session_until_the_last_delivery_drops() {
        let factory = DeliveryFactory::detect(Path::new("/nonexistent"));
        let cloned = factory.clone();
        let first_timeout = Duration::from_secs(1);
        let first = factory.create(first_timeout).expect("a valid timeout");
        let second = cloned.create(TIMEOUT).expect("a valid timeout");
        assert!(Arc::ptr_eq(&first.desktop, &second.desktop));
        assert_eq!(first.timeout, first_timeout);
        assert_eq!(second.timeout, TIMEOUT);
        for timeout in [
            Duration::ZERO,
            MAX_DELIVERY_TIMEOUT + Duration::from_secs(1),
        ] {
            assert!(matches!(
                cloned.create(timeout),
                Err(DeliveryError::InvalidTimeout { .. })
            ));
        }

        let session = Arc::downgrade(&first.desktop);
        drop(factory);
        drop(cloned);
        drop(first);
        assert!(session.upgrade().is_some());
        drop(second);
        assert!(session.upgrade().is_none());
    }

    #[test]
    fn without_a_keyboard_insert_or_copy_copies_and_insert_fails() {
        let delivery = delivery();
        assert_eq!(
            delivery
                .prepare(OutputMode::InsertOrCopy, || false)
                .unwrap(),
            LinuxDeliveryTarget::Clipboard
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
            delivery().deliver(&LinuxDeliveryTarget::Clipboard, "text", || false),
            Err(DeliveryError::TargetUnavailable)
        ));
    }

    #[test]
    fn a_paste_that_sent_no_keys_counts_as_copied() {
        for failure in [JobError::NotSent, JobError::PrimaryRejected] {
            assert_eq!(failed_paste(failure), FailedPaste::AlreadyCopied);
        }
    }

    #[test]
    fn a_refused_paste_copies_because_nothing_was_written() {
        assert_eq!(
            failed_paste(JobError::Refused(Refusal::Failed)),
            FailedPaste::CopyInstead(Refusal::Failed)
        );
    }

    #[test]
    fn a_paste_that_may_have_landed_is_never_copied_again() {
        for failure in [
            JobError::Unconfirmed,
            JobError::TimedOut,
            JobError::Cancelled,
            JobError::ClipboardRejected,
        ] {
            assert_eq!(failed_paste(failure), FailedPaste::Fail(failure));
        }
    }

    #[test]
    fn each_failure_keeps_its_own_error() {
        assert!(matches!(
            delivery_error(JobError::NotSent, TIMEOUT),
            DeliveryError::TargetUnavailable
        ));
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
