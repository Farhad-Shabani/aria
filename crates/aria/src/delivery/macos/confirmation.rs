use std::time::{Duration, Instant};

use cidre::{arc, ax};

use crate::delivery::DeliveryError;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_FIELD_UNITS: usize = 1024 * 1024;

pub(crate) struct Confirmation {
    application: arc::R<ax::UiElement>,
    element: arc::R<ax::UiElement>,
    timeout: Duration,
    expected: Option<Snapshot>,
}

impl Confirmation {
    pub(super) fn capture(process: u32, timeout: Duration) -> Result<Self, DeliveryError> {
        let deadline = Instant::now() + timeout;
        let process = i32::try_from(process).map_err(|_| DeliveryError::TargetUnavailable)?;
        let application = ax::UiElement::with_app_pid(process);
        set_timeout(&application, deadline, timeout)?;
        let element = application
            .focused_ui_element()
            .map_err(|_| DeliveryError::SubmissionUnconfirmed)?;
        let confirmation = Self {
            application,
            element,
            timeout,
            expected: None,
        };
        confirmation.snapshot(deadline)?;
        Ok(confirmation)
    }

    pub(super) fn insert(
        &mut self,
        text: &str,
        check: impl Fn() -> Result<(), DeliveryError>,
    ) -> Result<(), DeliveryError> {
        let deadline = Instant::now() + self.timeout;
        check()?;
        let before = self.snapshot(deadline)?;
        if self.expected.as_ref().is_some_and(|last| last != &before) {
            return Err(DeliveryError::SubmissionUnconfirmed);
        }
        let expected = before.inserting(text)?;
        self.expected = None;
        check()?;
        remaining(deadline, self.timeout)?;
        super::post(text)?;
        wait_for_confirmation(&expected, deadline, self.timeout, check, || {
            self.snapshot(deadline)
        })?;
        self.expected = Some(expected);
        Ok(())
    }

    pub(super) fn confirm(
        &self,
        check: impl Fn() -> Result<(), DeliveryError>,
    ) -> Result<(), DeliveryError> {
        let expected = self
            .expected
            .as_ref()
            .ok_or(DeliveryError::SubmissionUnconfirmed)?;
        let deadline = Instant::now() + self.timeout;
        check()?;
        let current = self.snapshot(deadline)?;
        check()?;
        remaining(deadline, self.timeout)?;
        if &current != expected {
            return Err(DeliveryError::SubmissionUnconfirmed);
        }
        Ok(())
    }

    fn snapshot(&self, deadline: Instant) -> Result<Snapshot, DeliveryError> {
        self.check_focus(deadline)?;
        set_timeout(&self.element, deadline, self.timeout)?;
        let value = self
            .element
            .attr_value(ax::attr::value())
            .map_err(|_| DeliveryError::SubmissionUnconfirmed)?;
        let value = value
            .try_as_string()
            .ok_or(DeliveryError::SubmissionUnconfirmed)?;
        let length =
            usize::try_from(value.len()).map_err(|_| DeliveryError::SubmissionUnconfirmed)?;
        if length > MAX_FIELD_UNITS {
            return Err(DeliveryError::SubmissionUnconfirmed);
        }
        let units = (0..value.len())
            .map(|index| value.character_at_index(index))
            .collect();
        set_timeout(&self.element, deadline, self.timeout)?;
        let selection = self
            .element
            .attr_value(ax::attr::selected_text_range())
            .map_err(|_| DeliveryError::SubmissionUnconfirmed)?;
        let selection =
            super::selection_range(&selection).ok_or(DeliveryError::SubmissionUnconfirmed)?;
        self.check_focus(deadline)?;
        remaining(deadline, self.timeout)?;
        Ok(Snapshot {
            units,
            selection_start: usize::try_from(selection.loc)
                .map_err(|_| DeliveryError::SubmissionUnconfirmed)?,
            selection_length: usize::try_from(selection.len)
                .map_err(|_| DeliveryError::SubmissionUnconfirmed)?,
        })
    }

    fn check_focus(&self, deadline: Instant) -> Result<(), DeliveryError> {
        set_timeout(&self.application, deadline, self.timeout)?;
        let focused = self
            .application
            .focused_ui_element()
            .map_err(|_| DeliveryError::SubmissionUnconfirmed)?;
        if !self.element.equal(&focused) {
            return Err(DeliveryError::TargetChanged);
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    units: Vec<u16>,
    selection_start: usize,
    selection_length: usize,
}

impl Snapshot {
    fn inserting(&self, text: &str) -> Result<Self, DeliveryError> {
        let end = self
            .selection_start
            .checked_add(self.selection_length)
            .filter(|end| *end <= self.units.len())
            .ok_or(DeliveryError::SubmissionUnconfirmed)?;
        let inserted: Vec<_> = text.encode_utf16().collect();
        let mut units = self.units.clone();
        units.splice(self.selection_start..end, inserted.iter().copied());
        // Identical replacement cannot prove the queued text events were consumed.
        if units == self.units || units.len() > MAX_FIELD_UNITS {
            return Err(DeliveryError::SubmissionUnconfirmed);
        }
        Ok(Self {
            units,
            selection_start: self.selection_start + inserted.len(),
            selection_length: 0,
        })
    }
}

fn wait_for_confirmation(
    expected: &Snapshot,
    deadline: Instant,
    timeout: Duration,
    check: impl Fn() -> Result<(), DeliveryError>,
    mut read: impl FnMut() -> Result<Snapshot, DeliveryError>,
) -> Result<(), DeliveryError> {
    loop {
        check()?;
        remaining(deadline, timeout)?;
        let current = read()?;
        check()?;
        let remaining = remaining(deadline, timeout)?;
        if &current == expected {
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

fn remaining(deadline: Instant, timeout: Duration) -> Result<Duration, DeliveryError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(DeliveryError::Timeout(timeout))
}

fn set_timeout(
    element: &ax::UiElement,
    deadline: Instant,
    timeout: Duration,
) -> Result<(), DeliveryError> {
    element
        .set_messaging_timeout_secs(remaining(deadline, timeout)?.as_secs_f32())
        .map_err(|_| DeliveryError::SubmissionUnconfirmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacement_requires_exact_text_and_utf16_caret() {
        let before = Snapshot {
            units: "a🙂b".encode_utf16().collect(),
            selection_start: 1,
            selection_length: 2,
        };
        let expected = before.inserting("سلام🙂").expect("valid selection");
        assert_eq!(expected.units, "aسلام🙂b".encode_utf16().collect::<Vec<_>>());
        assert_eq!(expected.selection_start, 7);
        assert_eq!(expected.selection_length, 0);
        assert!(before.inserting("🙂").is_err());
        let invalid_selection = Snapshot {
            selection_length: usize::MAX,
            ..before
        };
        assert!(invalid_selection.inserting("x").is_err());
    }

    fn snapshot(text: &str) -> Snapshot {
        let units: Vec<_> = text.encode_utf16().collect();
        Snapshot {
            selection_start: units.len(),
            selection_length: 0,
            units,
        }
    }

    #[test]
    fn confirmation_waits_for_all_text_and_the_caret() {
        let expected = snapshot("hello");
        let mut observations = [
            snapshot(""),
            snapshot("hel"),
            Snapshot {
                selection_start: 0,
                ..snapshot("hello")
            },
            snapshot("hello"),
        ]
        .into_iter();
        let timeout = Duration::from_secs(1);
        wait_for_confirmation(
            &expected,
            Instant::now() + timeout,
            timeout,
            || Ok(()),
            || Ok(observations.next().expect("must stop at confirmed text")),
        )
        .unwrap();
        assert!(observations.next().is_none());
    }

    #[test]
    fn stale_text_times_out_without_confirmation() {
        let timeout = Duration::from_millis(20);
        let result = wait_for_confirmation(
            &snapshot("hello"),
            Instant::now() + timeout,
            timeout,
            || Ok(()),
            || Ok(snapshot("hel")),
        );
        assert!(matches!(result, Err(DeliveryError::Timeout(value)) if value == timeout));
    }

    #[test]
    fn guards_still_refuse_after_matching_readback() {
        for refusal in [
            DeliveryError::Cancelled,
            DeliveryError::SecureInput,
            DeliveryError::TargetChanged,
        ] {
            let read_completed = std::cell::Cell::new(false);
            let refusal = std::cell::Cell::new(Some(refusal));
            let timeout = Duration::from_secs(1);
            let result = wait_for_confirmation(
                &snapshot("hello"),
                Instant::now() + timeout,
                timeout,
                || {
                    if read_completed.get() {
                        Err(refusal.take().unwrap())
                    } else {
                        Ok(())
                    }
                },
                || {
                    read_completed.set(true);
                    Ok(snapshot("hello"))
                },
            );
            assert!(matches!(
                result,
                Err(DeliveryError::Cancelled
                    | DeliveryError::SecureInput
                    | DeliveryError::TargetChanged)
            ));
            assert!(refusal.into_inner().is_none());
        }
    }
}
