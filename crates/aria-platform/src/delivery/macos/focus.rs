//! Checks that keyboard focus is still where a route pinned it, and how long
//! one check may take.

use std::time::Duration;

use super::super::DeliveryError;
use super::anchor;

/// The budget for one lookup: an Apple Event, a pass of Accessibility
/// reads, or a tmux query. A terminal answers on its main thread, so a busy
/// one must not hold a press.
pub(super) const LOOKUP_TIMEOUT: Duration = Duration::from_secs(1);

/// Why the focused place in a terminal is unknown.
pub(super) enum Lookup {
    /// Keyboard focus is somewhere text must not go, or Accessibility can't
    /// show where it is.
    Elsewhere(&'static str),
    /// The terminal or tmux didn't answer, or may no longer be driven.
    Unanswered(&'static str),
}

impl Lookup {
    pub(super) fn reason(&self) -> &'static str {
        match self {
            Self::Elsewhere(reason) | Self::Unanswered(reason) => reason,
        }
    }
}

/// A focus somewhere else reads as a target change, which may wait for the
/// person to come back. A terminal or
/// tmux server that can't be asked won't answer later either, so the press
/// stops.
pub(super) fn verdict<T: PartialEq>(
    pinned: &T,
    current: Result<T, Lookup>,
) -> Result<(), DeliveryError> {
    match current {
        Ok(place) if place == *pinned => Ok(()),
        Ok(_) => Err(anchor::target_changed("focus moved to another terminal")),
        Err(Lookup::Elsewhere(reason)) => Err(anchor::target_changed(reason)),
        Err(Lookup::Unanswered(reason)) => {
            tracing::warn!(reason, "the terminal can't confirm its focus");
            Err(DeliveryError::TargetUnavailable)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_elsewhere_waits_and_a_silent_terminal_stops_the_press() {
        assert!(verdict(&"A1".to_owned(), Ok("A1".into())).is_ok());
        assert!(matches!(
            verdict(&"A1".to_owned(), Ok("B2".into())),
            Err(DeliveryError::TargetChanged)
        ));
        assert!(matches!(
            verdict(&"A1".to_owned(), Err(Lookup::Elsewhere("quick terminal"))),
            Err(DeliveryError::TargetChanged)
        ));
        assert!(matches!(
            verdict(&"A1".to_owned(), Err(Lookup::Unanswered("timed out"))),
            Err(DeliveryError::TargetUnavailable)
        ));
    }
}
