//! Keeps streamed typing in its field. A later segment types only while the
//! caret sits right after the text typed last.
//!
//! Smart quotes change characters but not length, so the caret's offset
//! decides first. If an app changed the length, the last typed characters
//! before the caret decide instead. They are never logged.

use std::time::{Duration, Instant};

use cidre::{arc, ax, cf};

use super::caret::{self, Focus};
use crate::delivery::DeliveryError;

/// How long a busy app may take to apply the last segment.
const CATCH_UP_BUDGET: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// UTF-16 units of the last segment compared with the text before the caret.
const ENDING_UNITS: usize = 16;

pub(crate) enum Anchoring {
    Pending,
    Anchored(Anchor),
    /// The field or caret was unreadable, so only the frontmost-app check
    /// applies.
    Unavailable,
}

impl Anchoring {
    /// Anchors to the field found by the caret read for the first segment.
    pub(super) fn start(&mut self, focus: Option<Focus>) {
        if matches!(self, Self::Pending) {
            *self = focus.map_or(Self::Unavailable, |focus| {
                Self::Anchored(Anchor::new(focus))
            });
        }
    }

    /// Refuses when the caret left the end of the text typed last.
    pub(super) fn before_typing(&mut self) -> Result<(), DeliveryError> {
        match self {
            Self::Anchored(anchor) => anchor.check(),
            Self::Pending => {
                *self = Self::Unavailable;
                Ok(())
            }
            Self::Unavailable => Ok(()),
        }
    }

    /// The field and its caret offset, while the caret rests where the next
    /// segment types.
    pub(super) fn resting_caret(
        &self,
        process: u32,
        deadline: Instant,
    ) -> Option<(arc::R<ax::UiElement>, cf::Index)> {
        match self {
            Self::Anchored(anchor) => anchor.resting_caret(deadline),
            Self::Pending => caret::read_focus(process, deadline)
                .ok()
                .filter(|focus| focus.selection.len == 0)
                .map(|focus| (focus.field, focus.selection.loc)),
            Self::Unavailable => None,
        }
    }

    pub(super) fn after_typing(&mut self, text: &str) {
        if let Self::Anchored(anchor) = self {
            anchor.expected_caret = advanced(anchor.expected_caret, text);
            anchor.ending = Some(ending_of(text));
        }
    }
}

pub(crate) struct Anchor {
    application: arc::R<ax::UiElement>,
    field: arc::R<ax::UiElement>,
    /// The caret's UTF-16 offset once everything typed so far has landed.
    expected_caret: cf::Index,
    /// The end of the text typed last. `None` before the first segment.
    ending: Option<String>,
}

impl Anchor {
    fn new(focus: Focus) -> Self {
        Self {
            application: focus.application,
            field: focus.field,
            expected_caret: focus.selection.loc,
            ending: None,
        }
    }

    fn check(&mut self) -> Result<(), DeliveryError> {
        let Some(ending) = &self.ending else {
            return Ok(());
        };
        let deadline = Instant::now() + CATCH_UP_BUDGET;
        self.expected_caret = wait_for_caret(self.expected_caret, deadline, || {
            self.observe(ending, deadline)
        })?;
        Ok(())
    }

    fn resting_caret(&self, deadline: Instant) -> Option<(arc::R<ax::UiElement>, cf::Index)> {
        let focused = caret::focused_field(&self.application, deadline).ok()?;
        if !self.field.equal(&focused) {
            return None;
        }
        // A selection, inline completions included, hides the ghost so it
        // never covers highlighted text.
        let selection = caret::selection(&self.field, deadline).ok()?;
        if selection.len != 0 {
            return None;
        }
        if selection.loc == self.expected_caret {
            return Some((focused, selection.loc));
        }
        // If the app changed the typed text's length, its ending still marks
        // where typing continues.
        let ending = self.ending.as_deref()?;
        (self.text_before(ending, selection.loc, deadline).as_deref() == Some(ending))
            .then_some((focused, selection.loc))
    }

    fn observe(&self, ending: &str, deadline: Instant) -> Result<Verdict, &'static str> {
        let focused = caret::focused_field(&self.application, deadline)?;
        if !self.field.equal(&focused) {
            return Ok(Verdict::Moved("another field took focus"));
        }
        let selection = caret::selection(&self.field, deadline)?;
        let at_expected = selection.loc == self.expected_caret;
        let before = (selection.len == 0 && !at_expected)
            .then(|| self.text_before(ending, selection.loc, deadline))
            .flatten();
        let field_length = (selection.len > 0 && at_expected)
            .then(|| caret::field_length(&self.field, deadline).ok())
            .flatten();
        Ok(judge(
            self.expected_caret,
            ending,
            selection,
            before.as_deref(),
            field_length,
        ))
    }

    fn text_before(&self, ending: &str, caret: cf::Index, deadline: Instant) -> Option<String> {
        let units = cf::Index::try_from(ending.encode_utf16().count()).ok()?;
        if caret < units {
            return None;
        }
        caret::limit(&self.field, deadline).ok()?;
        caret::string_for_range(&self.field, caret - units, units)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The caret sits right after the typed text, at this offset.
    Continues(cf::Index),
    /// The app may still be applying the typed text.
    Behind,
    Moved(&'static str),
}

fn judge(
    expected_caret: cf::Index,
    ending: &str,
    selection: cf::Range,
    before: Option<&str>,
    field_length: Option<i64>,
) -> Verdict {
    let at_expected = selection.loc == expected_caret;
    if at_expected && selection.len == 0 {
        return Verdict::Continues(expected_caret);
    }
    // Inline completion selects from the caret to the field's end. The next
    // segment types over it.
    let reaches_field_end = selection
        .loc
        .checked_add(selection.len)
        .and_then(|end| i64::try_from(end).ok())
        .is_some_and(|end| field_length == Some(end));
    if at_expected && reaches_field_end {
        return Verdict::Continues(expected_caret);
    }
    if selection.len == 0 && before == Some(ending) {
        return Verdict::Continues(selection.loc);
    }
    if selection.len == 0 && selection.loc < expected_caret {
        return Verdict::Behind;
    }
    Verdict::Moved("caret moved")
}

/// Typing replaces any selection, so the caret lands after the typed units.
fn advanced(caret: cf::Index, text: &str) -> cf::Index {
    let units = cf::Index::try_from(text.encode_utf16().count()).unwrap_or(cf::Index::MAX);
    caret.saturating_add(units)
}

/// The last units of `text`, decoded like a field read that starts mid-pair.
fn ending_of(text: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let start = units.len().saturating_sub(ENDING_UNITS);
    caret::decode(units[start..].iter().copied())
}

/// Polls until the caret sits after the typed text. A clear move stops typing
/// at once, and a caret still behind at `deadline` stops it too. A field that
/// stays unreadable proves nothing, so typing goes on at `expected_caret`.
fn wait_for_caret(
    expected_caret: cf::Index,
    deadline: Instant,
    mut observe: impl FnMut() -> Result<Verdict, &'static str>,
) -> Result<cf::Index, DeliveryError> {
    loop {
        let unreadable = match observe() {
            Ok(Verdict::Continues(caret)) => return Ok(caret),
            Ok(Verdict::Moved(reason)) => return Err(target_changed(reason)),
            Ok(Verdict::Behind) => None,
            Err(reason) => Some(reason),
        };
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
        else {
            let Some(reason) = unreadable else {
                return Err(target_changed("caret short of the typed text"));
            };
            tracing::debug!(reason, "insertion point unreadable; typing goes on");
            return Ok(expected_caret);
        };
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

fn target_changed(reason: &'static str) -> DeliveryError {
    tracing::debug!(reason, "insertion point changed; typing stops");
    DeliveryError::TargetChanged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caret(loc: cf::Index) -> cf::Range {
        cf::Range { loc, len: 0 }
    }

    #[test]
    fn a_bare_caret_at_the_expected_offset_continues() {
        assert_eq!(judge(5, "", caret(5), None, None), Verdict::Continues(5));
    }

    #[test]
    fn inline_completion_to_the_field_end_continues() {
        let suggestion = cf::Range { loc: 5, len: 3 };
        assert_eq!(
            judge(5, "", suggestion, None, Some(8)),
            Verdict::Continues(5)
        );
        assert_eq!(
            judge(5, "", suggestion, None, Some(9)),
            Verdict::Moved("caret moved")
        );
    }

    #[test]
    fn a_length_change_resyncs_on_the_typed_ending() {
        assert_eq!(
            judge(9, "lo there", caret(10), Some("lo there"), None),
            Verdict::Continues(10)
        );
        assert_eq!(
            judge(9, "lo there", caret(10), Some("o therex"), None),
            Verdict::Moved("caret moved")
        );
    }

    #[test]
    fn a_caret_short_of_the_text_may_still_catch_up() {
        assert_eq!(judge(9, "there", caret(4), None, None), Verdict::Behind);
    }

    #[test]
    fn a_user_selection_stops_typing() {
        assert_eq!(
            judge(5, "", cf::Range { loc: 2, len: 3 }, None, None),
            Verdict::Moved("caret moved")
        );
    }

    #[test]
    fn the_first_segment_types_without_a_check() {
        // Nothing reads Accessibility before the first segment.
        let mut anchor = Anchor {
            application: ax::UiElement::with_app_pid(1),
            field: ax::UiElement::with_app_pid(1),
            expected_caret: 5,
            ending: None,
        };

        assert!(anchor.check().is_ok());
        assert_eq!(anchor.expected_caret, 5);
    }

    #[test]
    fn typed_text_advances_by_utf16_units() {
        assert_eq!(advanced(3, " hello"), 9);
        assert_eq!(advanced(0, "🙂"), 2);
        assert_eq!(advanced(0, "سلام"), 4);
    }

    #[test]
    fn the_ending_keeps_the_last_units_as_a_split_read_would() {
        assert_eq!(ending_of("hello"), "hello");
        let long = format!("{}🙂", "a".repeat(ENDING_UNITS - 1));
        assert_eq!(
            ending_of(&long),
            format!("{}🙂", "a".repeat(ENDING_UNITS - 2))
        );
        let split = format!("🙂{}", "a".repeat(ENDING_UNITS - 1));
        assert_eq!(ending_of(&split), "a".repeat(ENDING_UNITS - 1));
    }

    #[test]
    fn a_caret_that_catches_up_continues() {
        let mut reads = [
            Ok(Verdict::Behind),
            Err("no selection"),
            Ok(Verdict::Continues(9)),
        ]
        .into_iter();
        let deadline = Instant::now() + Duration::from_secs(1);

        let result = wait_for_caret(9, deadline, || {
            reads.next().expect("must stop at the caught-up caret")
        });

        assert_eq!(result.unwrap(), 9);
        assert!(reads.next().is_none());
    }

    #[test]
    fn a_clear_move_stops_typing_without_waiting() {
        let mut reads = [Ok(Verdict::Moved("caret moved"))].into_iter();
        let deadline = Instant::now() + Duration::from_secs(60);

        let result = wait_for_caret(9, deadline, || {
            reads.next().expect("must stop at the first clear move")
        });

        assert!(matches!(result, Err(DeliveryError::TargetChanged)));
    }

    #[test]
    fn a_caret_that_never_catches_up_stops_typing() {
        let deadline = Instant::now() + Duration::from_millis(20);

        let result = wait_for_caret(9, deadline, || Ok(Verdict::Behind));

        assert!(matches!(result, Err(DeliveryError::TargetChanged)));
    }

    #[test]
    fn an_unreadable_field_keeps_typing_at_the_expected_offset() {
        let deadline = Instant::now() + Duration::from_millis(20);

        let result = wait_for_caret(9, deadline, || Err("no selection"));

        assert_eq!(result.unwrap(), 9);
    }
}
