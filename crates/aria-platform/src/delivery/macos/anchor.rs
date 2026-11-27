//! Keeps streamed typing in its field. A later segment types only while the
//! caret sits right after the text typed last.
//!
//! Smart quotes change characters but not length, so the caret's offset
//! decides first. If an app changed the length, the last typed characters
//! before the caret decide instead. They are never logged.
//!
//! Some fields, terminals among them, report one caret offset however much
//! text lands. When the second segment finds the caret still where the first
//! began, the anchor checks only the field from then on. It waits for a slow
//! app to move the caret, unless Accessibility cannot set the field's
//! selection. Terminals answer so, and one look then decides.
//!
//! The closing space types last, before a word already after the caret:
//! "open src" streamed before `notes` → `open src notes`. Outside a
//! terminal, it needs the caret at its expected offset with nothing selected.

use std::time::{Duration, Instant};

use cidre::{arc, ax, cf};

use super::caret::{self, Focus};
use super::element;
use crate::delivery::DeliveryError;

/// How long a busy app may take to apply the last segment.
const CATCH_UP_BUDGET: Duration = Duration::from_secs(1);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// UTF-16 units of the last segment compared with the text before the caret.
const ENDING_UNITS: usize = 16;

pub enum Anchoring {
    Pending,
    Anchored(Anchor),
    /// The field or caret was unreadable, so only the frontmost-app check
    /// applies.
    Unavailable,
}

impl Anchoring {
    /// Anchors to the field found by the first read that succeeds. A later
    /// read before any text only moves the expected caret, and only within
    /// that field. A busy app that misses one read can still anchor later.
    pub(super) fn start(&mut self, focus: Result<Focus, &'static str>) {
        match (&mut *self, focus) {
            (Self::Anchored(anchor), Ok(focus)) => anchor.refresh(&focus),
            (Self::Pending | Self::Unavailable, Ok(focus)) => {
                *self = Self::Anchored(Anchor::new(focus));
            }
            (Self::Pending, Err(reason)) => {
                tracing::info!(
                    reason,
                    "focused field unreadable; streaming checks only the frontmost app"
                );
                *self = Self::Unavailable;
            }
            (Self::Anchored(_) | Self::Unavailable, Err(_)) => {}
        }
    }

    /// Refuses when typing would land away from the text typed last.
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

    /// Before pressing the person's Return: only where the text went, with
    /// the caret after it. A field never bound refuses it.
    pub(super) fn before_return(&mut self) -> Result<(), DeliveryError> {
        match self {
            Self::Anchored(anchor) => anchor.check(),
            Self::Pending => Err(DeliveryError::TargetUnverified),
            Self::Unavailable => Ok(()),
        }
    }

    /// Refuses unless a read field shows the caret right after the typed
    /// text with nothing selected. A terminal gets a segment's check.
    pub(super) fn before_closing_space(&mut self) -> Result<(), DeliveryError> {
        match self {
            Self::Anchored(anchor) => anchor.check_closing_space(),
            Self::Pending | Self::Unavailable => Err(DeliveryError::TargetUnverified),
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
            anchor.after_typing(text);
        }
    }

    pub(super) fn after_closing_space(&mut self, space: &str) {
        if let Self::Anchored(anchor) = self {
            anchor.after_closing_space(space);
        }
    }

    pub(super) fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

pub struct Anchor {
    application: arc::R<ax::UiElement>,
    field: arc::R<ax::UiElement>,
    /// The caret's UTF-16 offset once everything typed so far has landed.
    expected_caret: cf::Index,
    /// The end of the text typed last. `None` before the first segment.
    ending: Option<String>,
    tracking: Tracking,
}

/// What types once a check passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Insertion {
    /// A segment or a Return. May type over an inline completion.
    Segment,
    /// The closing space. Never types over a selection.
    ClosingSpace,
}

#[derive(Debug, PartialEq, Eq)]
enum Tracking {
    /// Holds the caret's offset before the first segment, until a check
    /// judges it.
    Untested(cf::Index),
    /// The caret moves with typed text, so it shows where typing goes.
    Caret,
    /// The caret stayed where the first segment began, so it shows nothing.
    Field,
}

impl Anchor {
    fn new(focus: Focus) -> Self {
        Self {
            application: focus.application,
            field: focus.field,
            expected_caret: focus.selection.loc,
            ending: None,
            tracking: Tracking::Untested(focus.selection.loc),
        }
    }

    /// The user may have typed or clicked between the press and its first
    /// segment, so the caret is read again then.
    fn refresh(&mut self, focus: &Focus) {
        if self.ending.is_some() || !self.field.equal(&focus.field) {
            return;
        }
        self.expected_caret = focus.selection.loc;
        self.tracking = Tracking::Untested(focus.selection.loc);
    }

    fn check(&mut self) -> Result<(), DeliveryError> {
        let started = Instant::now();
        let deadline = started + CATCH_UP_BUDGET;
        // Before any text, only the field can be checked.
        let Some(ending) = &self.ending else {
            return match self.observe_field(deadline) {
                Settled::Left => Err(target_changed("another field took focus")),
                _ => Ok(()),
            };
        };
        let settled = match self.tracking {
            Tracking::Untested(_) => {
                let settable = element::is_selection_settable(&self.field, deadline);
                let polls_until = first_check_polls_until(started, settable);
                wait_for_caret(polls_until, || {
                    self.observe(ending, Insertion::Segment, deadline)
                })
            }
            Tracking::Caret => wait_for_caret(deadline, || {
                self.observe(ending, Insertion::Segment, deadline)
            }),
            Tracking::Field => self.observe_field(deadline),
        };
        // Past one poll, the app was still applying the last segment.
        let waited = started.elapsed();
        if waited >= POLL_INTERVAL {
            tracing::debug!(
                waited_ms = waited.as_millis(),
                "waited for the last segment to land"
            );
        }
        self.settle(settled, Insertion::Segment)
    }

    fn check_closing_space(&mut self) -> Result<(), DeliveryError> {
        let deadline = Instant::now() + CATCH_UP_BUDGET;
        if element::is_selection_settable(&self.field, deadline) == Ok(false) {
            return self.check();
        }
        let Some(ending) = &self.ending else {
            return Err(DeliveryError::TargetUnverified);
        };
        let settled = wait_for_caret(deadline, || {
            self.observe(ending, Insertion::ClosingSpace, deadline)
        });
        self.settle(settled, Insertion::ClosingSpace)
    }

    /// The first check comes a whole segment after the first one landed. A
    /// caret still at its start by then means the field never moves it. For
    /// the closing space, a caret short of the text always refuses.
    fn settle(&mut self, settled: Settled, insertion: Insertion) -> Result<(), DeliveryError> {
        match settled {
            // The closing space proves the caret moves too, so a later
            // Return refuses a caret back at the start.
            Settled::After(caret) => {
                self.expected_caret = caret;
                if matches!(self.tracking, Tracking::Untested(_)) {
                    self.tracking = Tracking::Caret;
                }
            }
            Settled::Short(caret)
                if insertion == Insertion::Segment
                    && self.tracking == Tracking::Untested(caret) =>
            {
                tracing::info!("caret stays put as text lands; only the field is checked");
                self.tracking = Tracking::Field;
            }
            Settled::Short(_) => return Err(caret_moved("caret short of the typed text")),
            Settled::Moved(reason) => return Err(caret_moved(reason)),
            Settled::Left => return Err(target_changed("another field took focus")),
            Settled::Unknown(reason) if insertion == Insertion::ClosingSpace => {
                tracing::info!(reason, "insertion point unreadable; the space is not typed");
                return Err(DeliveryError::TargetUnverified);
            }
            Settled::Unknown(reason) => {
                tracing::debug!(reason, "insertion point unreadable; typing goes on");
            }
        }
        Ok(())
    }

    /// Ends on this segment's text alone, since an app may fix earlier
    /// text, as `dont` → `don't`.
    fn after_typing(&mut self, text: &str) {
        self.expected_caret = advanced(self.expected_caret, text);
        self.ending = Some(ending_of(text));
    }

    /// A lone space would match any caret after a space, so it extends the
    /// segment's ending.
    fn after_closing_space(&mut self, space: &str) {
        self.expected_caret = advanced(self.expected_caret, space);
        self.ending = Some(ending_after(self.ending.as_deref(), space));
    }

    fn resting_caret(&self, deadline: Instant) -> Option<(arc::R<ax::UiElement>, cf::Index)> {
        let focused = element::focused_field(&self.application, deadline).ok()?;
        if !self.field.equal(&focused) {
            return None;
        }
        // A selection, inline completions included, hides the ghost so it
        // never covers highlighted text.
        let selection = element::selection(&self.field, deadline).ok()?;
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

    fn observe(
        &self,
        ending: &str,
        insertion: Insertion,
        deadline: Instant,
    ) -> Result<Verdict, &'static str> {
        let focused = element::focused_field(&self.application, deadline)?;
        if !self.field.equal(&focused) {
            return Ok(Verdict::Left);
        }
        let selection = element::selection(&self.field, deadline)?;
        let at_expected = selection.loc == self.expected_caret;
        let before = (insertion == Insertion::Segment && selection.len == 0 && !at_expected)
            .then(|| self.text_before(ending, selection.loc, deadline))
            .flatten();
        let field_length = (selection.len > 0 && at_expected)
            .then(|| element::field_length(&self.field, deadline).ok())
            .flatten();
        Ok(judge(
            self.expected_caret,
            ending,
            selection,
            insertion,
            before.as_deref(),
            field_length,
        ))
    }

    fn observe_field(&self, deadline: Instant) -> Settled {
        match element::focused_field(&self.application, deadline) {
            Ok(focused) if self.field.equal(&focused) => Settled::After(self.expected_caret),
            Ok(_) => Settled::Left,
            Err(reason) => Settled::Unknown(reason),
        }
    }

    fn text_before(&self, ending: &str, caret: cf::Index, deadline: Instant) -> Option<String> {
        let units = cf::Index::try_from(ending.encode_utf16().count()).ok()?;
        if caret < units {
            return None;
        }
        element::limit(&self.field, deadline).ok()?;
        element::string_for_range(&self.field, caret - units, units)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// The caret sits right after the typed text, at this offset.
    Continues(cf::Index),
    /// The caret sits short of the typed text, at this offset. The app may
    /// still be applying it.
    Behind(cf::Index),
    /// The caret moved inside the field, away from the typed text.
    Moved(&'static str),
    /// Another field took focus.
    Left,
}

fn judge(
    expected_caret: cf::Index,
    ending: &str,
    selection: cf::Range,
    insertion: Insertion,
    before: Option<&str>,
    field_length: Option<i64>,
) -> Verdict {
    let at_expected = selection.loc == expected_caret;
    if at_expected && selection.len == 0 {
        return Verdict::Continues(expected_caret);
    }
    if insertion == Insertion::ClosingSpace && selection.len > 0 {
        return Verdict::Moved("text selected");
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
    // A short ending can repeat elsewhere, so the closing space never
    // follows the ending to a new offset.
    if insertion == Insertion::Segment && selection.len == 0 && before == Some(ending) {
        return Verdict::Continues(selection.loc);
    }
    if selection.len == 0 && selection.loc < expected_caret {
        return Verdict::Behind(selection.loc);
    }
    Verdict::Moved("caret moved")
}

/// Typing replaces any selection, so the caret lands after the typed units.
fn advanced(caret: cf::Index, text: &str) -> cf::Index {
    let units = cf::Index::try_from(text.encode_utf16().count()).unwrap_or(cf::Index::MAX);
    caret.saturating_add(units)
}

fn ending_after(previous: Option<&str>, text: &str) -> String {
    ending_of(&format!("{}{text}", previous.unwrap_or_default()))
}

/// The last units of `text`, decoded like a field read that starts mid-pair.
fn ending_of(text: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let start = units.len().saturating_sub(ENDING_UNITS);
    element::decode(units[start..].iter().copied())
}

#[derive(Debug, PartialEq, Eq)]
enum Settled {
    After(cf::Index),
    Short(cf::Index),
    Moved(&'static str),
    Left,
    /// Every read failed, which is no sign the caret moved.
    Unknown(&'static str),
}

fn first_check_polls_until(started: Instant, settable: Result<bool, &'static str>) -> Instant {
    match settable {
        Ok(false) => started,
        // Unanswered counts as settable, so a slow app still gets its wait.
        Ok(true) | Err(_) => started + CATCH_UP_BUDGET,
    }
}

/// Polls until the caret sits after the typed text or clearly moved. Past
/// `deadline`, the last caret seen decides.
fn wait_for_caret(
    deadline: Instant,
    mut observe: impl FnMut() -> Result<Verdict, &'static str>,
) -> Settled {
    let mut short = None;
    loop {
        let settled = match observe() {
            Ok(Verdict::Continues(caret)) => return Settled::After(caret),
            Ok(Verdict::Moved(reason)) => return Settled::Moved(reason),
            Ok(Verdict::Left) => return Settled::Left,
            Ok(Verdict::Behind(caret)) => {
                short = Some(caret);
                Settled::Short(caret)
            }
            Err(reason) => short.map_or(Settled::Unknown(reason), Settled::Short),
        };
        let Some(remaining) = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
        else {
            return settled;
        };
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

pub(super) fn target_changed(reason: &'static str) -> DeliveryError {
    tracing::info!(reason, "insertion point left the field");
    DeliveryError::TargetChanged
}

fn caret_moved(reason: &'static str) -> DeliveryError {
    tracing::info!(reason, "caret moved in the field; typing stops");
    DeliveryError::CaretMoved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caret(loc: cf::Index) -> cf::Range {
        cf::Range { loc, len: 0 }
    }

    #[test]
    fn a_bare_caret_at_the_expected_offset_continues() {
        assert_eq!(
            judge(5, "", caret(5), Insertion::Segment, None, None),
            Verdict::Continues(5)
        );
    }

    #[test]
    fn inline_completion_to_the_field_end_continues() {
        let suggestion = cf::Range { loc: 5, len: 3 };
        assert_eq!(
            judge(5, "", suggestion, Insertion::Segment, None, Some(8)),
            Verdict::Continues(5)
        );
        assert_eq!(
            judge(5, "", suggestion, Insertion::Segment, None, Some(9)),
            Verdict::Moved("caret moved")
        );
    }

    #[test]
    fn the_owed_space_never_types_over_a_selection() {
        let suggestion = cf::Range { loc: 5, len: 3 };
        assert_eq!(
            judge(5, "", suggestion, Insertion::ClosingSpace, None, Some(8)),
            Verdict::Moved("text selected")
        );
        assert_eq!(
            judge(5, "", caret(5), Insertion::ClosingSpace, None, None),
            Verdict::Continues(5)
        );
    }

    #[test]
    fn the_owed_space_never_resyncs_on_a_repeated_ending() {
        assert_eq!(
            judge(
                9,
                " go",
                caret(12),
                Insertion::ClosingSpace,
                Some(" go"),
                None
            ),
            Verdict::Moved("caret moved")
        );
    }

    #[test]
    fn the_owed_space_needs_the_caret_seen_after_the_text() {
        assert!(
            typed_anchor()
                .settle(Settled::After(9), Insertion::ClosingSpace)
                .is_ok()
        );
        assert!(matches!(
            typed_anchor().settle(Settled::Unknown("no selection"), Insertion::ClosingSpace),
            Err(DeliveryError::TargetUnverified)
        ));
        assert!(matches!(
            typed_anchor().settle(Settled::Short(4), Insertion::ClosingSpace),
            Err(DeliveryError::CaretMoved)
        ));
    }

    #[test]
    fn the_owed_space_never_takes_a_caret_at_its_start_for_a_field_that_stays_put() {
        let mut anchor = typed_anchor();
        anchor.tracking = Tracking::Untested(4);

        assert!(matches!(
            anchor.settle(Settled::Short(4), Insertion::ClosingSpace),
            Err(DeliveryError::CaretMoved)
        ));
        assert_eq!(anchor.tracking, Tracking::Untested(4));
    }

    #[test]
    fn a_return_after_the_space_refuses_a_caret_moved_back_to_the_start() {
        let mut anchor = typed_anchor();
        anchor.tracking = Tracking::Untested(4);

        assert!(
            anchor
                .settle(Settled::After(9), Insertion::ClosingSpace)
                .is_ok()
        );
        assert_eq!(anchor.tracking, Tracking::Caret);
        assert!(matches!(
            anchor.settle(Settled::Short(4), Insertion::Segment),
            Err(DeliveryError::CaretMoved)
        ));
    }

    #[test]
    fn a_field_checked_only_for_focus_still_reads_the_caret_for_the_space() {
        // Pid 1 exposes no field, so every read fails.
        let mut anchoring = Anchoring::Anchored(Anchor {
            tracking: Tracking::Field,
            ..typed_anchor()
        });

        assert!(matches!(
            anchoring.before_closing_space(),
            Err(DeliveryError::TargetUnverified)
        ));
    }

    #[test]
    fn a_segment_ends_on_its_own_text_and_the_space_on_the_segment() {
        let mut anchor = typed_anchor();
        anchor.after_typing(" go");
        assert_eq!(anchor.ending.as_deref(), Some(" go"));
        anchor.after_closing_space(" ");
        assert_eq!(anchor.ending.as_deref(), Some(" go "));
        assert_eq!(anchor.expected_caret, 13);

        let long = "a".repeat(ENDING_UNITS);
        assert_eq!(
            ending_after(Some(&long), " ").encode_utf16().count(),
            ENDING_UNITS
        );
    }

    #[test]
    fn a_length_change_resyncs_on_the_typed_ending() {
        assert_eq!(
            judge(
                9,
                "lo there",
                caret(10),
                Insertion::Segment,
                Some("lo there"),
                None
            ),
            Verdict::Continues(10)
        );
        assert_eq!(
            judge(
                9,
                "lo there",
                caret(10),
                Insertion::Segment,
                Some("o therex"),
                None
            ),
            Verdict::Moved("caret moved")
        );
    }

    #[test]
    fn a_caret_short_of_the_text_may_still_catch_up() {
        assert_eq!(
            judge(9, "there", caret(4), Insertion::Segment, None, None),
            Verdict::Behind(4)
        );
    }

    #[test]
    fn a_user_selection_stops_typing() {
        assert_eq!(
            judge(
                5,
                "",
                cf::Range { loc: 2, len: 3 },
                Insertion::Segment,
                None,
                None
            ),
            Verdict::Moved("caret moved")
        );
    }

    #[test]
    fn an_unreadable_field_before_any_text_lets_typing_go_on() {
        // Pid 1 exposes no field, so the read comes back unknown.
        let mut anchor = Anchor {
            application: ax::UiElement::with_app_pid(1),
            field: ax::UiElement::with_app_pid(1),
            expected_caret: 5,
            ending: None,
            tracking: Tracking::Caret,
        };

        assert!(anchor.check().is_ok());
        assert_eq!(anchor.expected_caret, 5);
    }

    fn typed_anchor() -> Anchor {
        Anchor {
            application: ax::UiElement::with_app_pid(1),
            field: ax::UiElement::with_app_pid(1),
            expected_caret: 9,
            ending: Some("hello".to_owned()),
            tracking: Tracking::Caret,
        }
    }

    #[test]
    fn a_caret_still_at_its_start_by_the_first_check_is_ignored() {
        let mut anchor = typed_anchor();
        anchor.tracking = Tracking::Untested(4);

        let result = anchor.settle(Settled::Short(4), Insertion::Segment);

        assert!(result.is_ok());
        assert_eq!(anchor.tracking, Tracking::Field);
    }

    #[test]
    fn a_caret_that_moved_with_the_first_segment_is_tracked() {
        let mut anchor = typed_anchor();
        anchor.tracking = Tracking::Untested(4);

        let result = anchor.settle(Settled::After(10), Insertion::Segment);

        assert!(result.is_ok());
        assert_eq!(anchor.tracking, Tracking::Caret);
        assert_eq!(anchor.expected_caret, 10);
    }

    #[test]
    fn a_caret_short_of_the_text_but_off_its_start_stops_the_first_check() {
        let mut anchor = typed_anchor();
        anchor.tracking = Tracking::Untested(4);

        let result = anchor.settle(Settled::Short(6), Insertion::Segment);

        assert!(matches!(result, Err(DeliveryError::CaretMoved)));
    }

    #[test]
    fn a_tracked_caret_back_at_the_start_stops_typing() {
        let mut anchor = typed_anchor();

        let result = anchor.settle(Settled::Short(4), Insertion::Segment);

        assert!(matches!(result, Err(DeliveryError::CaretMoved)));
    }

    #[test]
    fn a_field_unreadable_at_first_can_still_anchor_later() {
        let focus = || Focus {
            application: ax::UiElement::with_app_pid(1),
            field: ax::UiElement::with_app_pid(1),
            selection: caret(3),
        };
        let mut anchoring = Anchoring::Pending;

        anchoring.start(Err("too slow"));
        assert!(anchoring.is_unavailable());
        anchoring.start(Err("too slow"));
        assert!(anchoring.is_unavailable());
        anchoring.start(Ok(focus()));
        assert!(matches!(anchoring, Anchoring::Anchored(_)));
        anchoring.start(Err("too slow"));
        assert!(matches!(anchoring, Anchoring::Anchored(_)));
    }

    #[test]
    fn a_read_before_any_text_moves_only_the_expected_caret() {
        let focus = |loc| Focus {
            application: ax::UiElement::with_app_pid(1),
            field: ax::UiElement::with_app_pid(1),
            selection: caret(loc),
        };
        let mut anchor = typed_anchor();
        anchor.ending = None;

        anchor.refresh(&focus(3));
        assert_eq!(anchor.expected_caret, 3);
        assert_eq!(anchor.tracking, Tracking::Untested(3));

        anchor.ending = Some("hello".to_owned());
        anchor.refresh(&focus(7));
        assert_eq!(anchor.expected_caret, 3);
    }

    #[test]
    fn another_field_taking_focus_is_told_apart_from_a_moved_caret() {
        let mut anchor = typed_anchor();

        assert!(matches!(
            anchor.settle(Settled::Left, Insertion::Segment),
            Err(DeliveryError::TargetChanged)
        ));
        assert!(matches!(
            anchor.settle(Settled::Moved("caret moved"), Insertion::Segment),
            Err(DeliveryError::CaretMoved)
        ));
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
            Ok(Verdict::Behind(4)),
            Err("no selection"),
            Ok(Verdict::Continues(9)),
        ]
        .into_iter();
        let deadline = Instant::now() + Duration::from_secs(1);

        let result = wait_for_caret(deadline, || {
            reads.next().expect("must stop at the caught-up caret")
        });

        assert_eq!(result, Settled::After(9));
        assert!(reads.next().is_none());
    }

    #[test]
    fn only_a_selection_that_cannot_be_set_skips_the_first_wait() {
        let started = Instant::now();

        assert_eq!(first_check_polls_until(started, Ok(false)), started);
        assert_eq!(
            first_check_polls_until(started, Ok(true)),
            started + CATCH_UP_BUDGET
        );
        assert_eq!(
            first_check_polls_until(started, Err("unreadable")),
            started + CATCH_UP_BUDGET
        );
    }

    #[test]
    fn a_deadline_already_past_gives_one_look() {
        let mut reads = 0;

        let result = wait_for_caret(Instant::now(), || {
            reads += 1;
            Ok(Verdict::Behind(4))
        });

        assert_eq!(result, Settled::Short(4));
        assert_eq!(reads, 1);
    }

    #[test]
    fn a_clear_move_stops_typing_without_waiting() {
        let mut reads = [Ok(Verdict::Moved("caret moved"))].into_iter();
        let deadline = Instant::now() + Duration::from_secs(60);

        let result = wait_for_caret(deadline, || {
            reads.next().expect("must stop at the first clear move")
        });

        assert_eq!(result, Settled::Moved("caret moved"));
    }

    #[test]
    fn a_caret_that_never_catches_up_settles_short() {
        let deadline = Instant::now() + Duration::from_millis(20);

        let result = wait_for_caret(deadline, || Ok(Verdict::Behind(4)));

        assert_eq!(result, Settled::Short(4));
    }

    #[test]
    fn a_read_cut_off_by_the_deadline_keeps_the_short_caret() {
        let mut first = true;
        let deadline = Instant::now() + Duration::from_millis(20);

        let result = wait_for_caret(deadline, || {
            if std::mem::take(&mut first) {
                Ok(Verdict::Behind(4))
            } else {
                Err("read budget spent")
            }
        });

        assert_eq!(result, Settled::Short(4));
    }

    #[test]
    fn an_unreadable_field_proves_nothing() {
        let deadline = Instant::now() + Duration::from_millis(20);

        let result = wait_for_caret(deadline, || Err("no selection"));

        assert_eq!(result, Settled::Unknown("no selection"));
    }
}
