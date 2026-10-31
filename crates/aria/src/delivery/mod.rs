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

    /// The application keystrokes would reach now. `None` where the
    /// platform cannot tell.
    fn frontmost(&self) -> Option<crate::history::Application>;

    /// The text beside the caret where `deliver` would type. Unknown unless
    /// the destination shows it.
    fn caret(&self, _prepared: &Self::Prepared) -> Caret {
        Caret::default()
    }

    /// Where `deliver` would type, on screen, or `None` without a visible
    /// caret.
    fn placement(&self, _prepared: &Self::Prepared) -> Option<Placement> {
        None
    }
}

/// The caret of the field being typed into, so text can be drawn beside it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Placement {
    pub caret: Rect,
    pub field: Option<Rect>,
    pub font: Option<Font>,
}

/// Screen points measured from the primary display's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Font {
    pub name: String,
    pub size: f64,
}

#[cfg(target_os = "macos")]
pub(crate) use macos::frontmost_application;

#[cfg(not(target_os = "macos"))]
pub(crate) fn frontmost_application() -> Option<crate::history::Application> {
    None
}

/// Which apps text may reach: never one with Aria switched off, and only
/// the app whose overrides chose the settings, when one did.
///
/// Check it before `prepare`, which reads the focused field, and again
/// before each write. A pinned target must still be in front to receive
/// text, so a front app that passes here is the one written to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TargetPolicy {
    pub(crate) overridden_for: Option<String>,
    pub(crate) excluded: Vec<String>,
}

impl TargetPolicy {
    pub(crate) fn excludes(&self, application: Option<&crate::history::Application>) -> bool {
        application
            .and_then(|application| application.bundle_id.as_deref())
            .is_some_and(|id| self.excluded.iter().any(|excluded| excluded == id))
    }

    /// Checks the application `delivery` would type into now.
    pub(crate) fn confirm_front(&self, delivery: &impl TextDelivery) -> Result<(), DeliveryError> {
        self.confirm(delivery.frontmost().as_ref())
    }

    fn confirm(&self, front: Option<&crate::history::Application>) -> Result<(), DeliveryError> {
        if self.excludes(front) {
            return Err(DeliveryError::ApplicationExcluded);
        }
        let Some(expected) = self.overridden_for.as_deref() else {
            return Ok(());
        };
        match front.and_then(|application| application.bundle_id.as_deref()) {
            Some(front) if front == expected => Ok(()),
            _ => Err(DeliveryError::TargetChanged),
        }
    }
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
    // Typed text reaches the app in front as keystrokes, where an escape or
    // ^C acts on that app instead of landing as text.
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(DeliveryError::ControlCharacter);
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

    #[error("delivery text contains a control character other than a line break or tab")]
    ControlCharacter,

    #[error("delivery timeout must be greater than zero and at most {maximum:?}; got {actual:?}")]
    InvalidTimeout { actual: Duration, maximum: Duration },

    #[error("the clipboard rejected the transcript")]
    ClipboardRejected,

    #[error("text delivery could not identify an available destination")]
    TargetUnavailable,

    #[error("the application, field, or caret position selected for insertion changed")]
    TargetChanged,

    #[error("Aria is switched off in the app in front")]
    ApplicationExcluded,

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
         → {} → switch on Aria",
        crate::accessibility::settings_pane()
    )]
    InsertionNotPermitted,

    #[cfg(target_os = "macos")]
    #[error(
        "a process holds secure keyboard entry, which discards typed text — often a \
         password field or Terminal's Secure Keyboard Entry; turn it off or use copy mode"
    )]
    SecureInput,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front(bundle_id: &str) -> Option<crate::history::Application> {
        Some(crate::history::Application {
            name: "App".into(),
            bundle_id: Some(bundle_id.into()),
        })
    }

    fn overridden_for(id: &str) -> TargetPolicy {
        TargetPolicy {
            overridden_for: Some(id.into()),
            ..TargetPolicy::default()
        }
    }

    #[test]
    fn text_with_a_control_character_other_than_a_line_break_or_tab_is_refused() {
        assert!(validate_text("one\ttwo\nthree").is_ok());
        for typed in ["rm -rf ~\u{1b}", "stop\u{3}", "back\r", "next\u{85}line"] {
            assert!(
                matches!(validate_text(typed), Err(DeliveryError::ControlCharacter)),
                "{typed:?}"
            );
        }
    }

    #[test]
    fn only_settings_chosen_for_another_app_refuse_delivery() {
        let anywhere = TargetPolicy::default();
        assert!(anywhere.confirm(None).is_ok());
        assert!(anywhere.confirm(front("com.b").as_ref()).is_ok());
        assert!(
            overridden_for("com.a")
                .confirm(front("com.a").as_ref())
                .is_ok()
        );
        assert!(matches!(
            overridden_for("com.a").confirm(front("com.b").as_ref()),
            Err(DeliveryError::TargetChanged)
        ));
        assert!(matches!(
            overridden_for("com.a").confirm(None),
            Err(DeliveryError::TargetChanged)
        ));
    }

    #[test]
    fn an_app_with_aria_off_receives_nothing() {
        let policy = TargetPolicy {
            excluded: vec!["com.secret".into()],
            ..overridden_for("com.secret")
        };

        assert!(matches!(
            policy.confirm(front("com.secret").as_ref()),
            Err(DeliveryError::ApplicationExcluded)
        ));
        assert!(policy.excludes(front("com.secret").as_ref()));
        assert!(!policy.excludes(front("com.other").as_ref()));
        assert!(!policy.excludes(None));
    }
}
