//! Keeps streamed typing in the element it started in. A later segment types
//! only while that element holds focus and, where it shows a caret, while the
//! caret sits right after the text typed last. A native document that hides
//! its text shows none, so only its identity is checked.
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

use super::caret::{self, Focus, Refusal};
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
    /// No element is bound: the first read found nothing that takes text or
    /// got no answer. A paste route then checks only its pin. The keys route
    /// still needs a text field each time it posts.
    Unavailable,
}

impl Anchoring {
    /// Anchors to the element found by the first read that succeeds. A later
    /// read before any text only moves the expected caret, and only within
    /// that element. A busy app that misses one read can still anchor later.
    pub(super) fn start(&mut self, focus: Result<Focus, Refusal>) {
        match (&mut *self, focus) {
            (Self::Anchored(anchor), Ok(focus)) => anchor.refresh(&focus),
            (Self::Pending | Self::Unavailable, Ok(focus)) => {
                *self = Self::Anchored(Anchor::new(focus));
            }
            (Self::Pending, Err(refusal)) => {
                tracing::info!(
                    reason = refusal.reason(),
                    "no element bound as the press started"
                );
                *self = Self::Unavailable;
            }
            (Self::Anchored(_) | Self::Unavailable, Err(_)) => {}
        }
    }

    /// Refuses when `post` would land away from the text typed last. Waits
    /// for the app to apply that text first.
    pub(super) fn check(&mut self, post: Post) -> Result<(), DeliveryError> {
        match post {
            Post::Segment => self.before_typing(),
            Post::ClosingSpace => self.before_closing_space(),
            Post::Return => self.before_return(),
        }
    }

    fn before_typing(&mut self) -> Result<(), DeliveryError> {
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
    /// the caret after it where one shows. A press never bound refuses it.
    fn before_return(&mut self) -> Result<(), DeliveryError> {
        match self {
            Self::Anchored(anchor) => anchor.check(),
            Self::Pending => Err(DeliveryError::TargetUnverified),
            Self::Unavailable => Ok(()),
        }
    }

    /// Refuses unless a read caret sits right after the typed text with
    /// nothing selected. A terminal gets a segment's check.
    fn before_closing_space(&mut self) -> Result<(), DeliveryError> {
        match self {
            Self::Anchored(anchor) => anchor.check_closing_space(),
            Self::Pending | Self::Unavailable => Err(DeliveryError::TargetUnverified),
        }
    }

    /// The element and its caret offset, while the caret rests where the
    /// next segment types.
    pub(super) fn resting_caret(
        &self,
        process: u32,
        deadline: Instant,
    ) -> Option<(arc::R<ax::UiElement>, cf::Index)> {
        match self {
            Self::Anchored(anchor) => anchor.resting_caret(deadline),
            Self::Pending => {
                let focus = caret::read_focus(process, deadline).ok()?;
                let selection = focus.selection.filter(|selection| selection.len == 0)?;
                Some((focus.element, selection.loc))
            }
            Self::Unavailable => None,
        }
    }

    pub(super) fn after_typing(&mut self, text: &str) {
        if let Self::Anchored(Anchor {
            caret: Some(track), ..
        }) = self
        {
            track.after_typing(text);
        }
    }

    pub(super) fn after_closing_space(&mut self, space: &str) {
        if let Self::Anchored(Anchor {
            caret: Some(track), ..
        }) = self
        {
            track.after_closing_space(space);
        }
    }

    /// The last look before text or a Return posts. A bound element must
    /// still hold focus and meet the caret rule its earlier check applied.
    /// An unbound press is offered the element read here, so one that
    /// answered only now still keeps later text. Its Return refuses.
    pub(super) fn look(
        &self,
        focus: Result<Focus, Refusal>,
        post: Post,
        deadline: Instant,
    ) -> Result<Option<LastLook>, DeliveryError> {
        let insertion = match post {
            Post::Segment | Post::Return => Insertion::Segment,
            Post::ClosingSpace => Insertion::ClosingSpace,
        };
        match (self, focus) {
            (Self::Anchored(anchor), Ok(focus)) => Ok(anchor
                .look(&focus, insertion, deadline)?
                .map(LastLook::Caret)),
            (Self::Anchored(_), Err(refusal)) => Err(target_changed(refusal.reason())),
            // A Return lands only where the press typed, so it never binds.
            (Self::Pending | Self::Unavailable, _) if post == Post::Return => {
                Err(DeliveryError::TargetUnverified)
            }
            (Self::Pending | Self::Unavailable, Ok(focus)) => Ok(Some(LastLook::Bind(focus))),
            (Self::Pending | Self::Unavailable, Err(refusal)) => Err(refusal.into_error()),
        }
    }

    pub(super) fn accept(&mut self, look: LastLook) {
        match (&mut *self, look) {
            (Self::Anchored(anchor), LastLook::Caret(caret)) => anchor.follow(caret),
            (Self::Pending | Self::Unavailable, LastLook::Bind(focus)) => {
                *self = Self::Anchored(Anchor::new(focus));
            }
            (Self::Anchored(_), LastLook::Bind(_))
            | (Self::Pending | Self::Unavailable, LastLook::Caret(_)) => {}
        }
    }

    pub(super) fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

/// What a last look learned about the element.
pub(super) enum LastLook {
    /// The element an unbound press types into.
    Bind(Focus),
    /// Where the bound element's caret sits, which an app's correction of
    /// earlier text may have moved.
    Caret(cf::Index),
}

/// The element a press types into. Every check asks that it still has
/// focus; where it shows a caret, the caret must sit after the typed text.
pub struct Anchor {
    application: arc::R<ax::UiElement>,
    element: arc::R<ax::UiElement>,
    /// Absent while the element has shown no caret.
    caret: Option<CaretTrack>,
}

/// What a press is about to post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Post {
    Segment,
    ClosingSpace,
    /// The person's Return, owed after the text.
    Return,
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
struct CaretTrack {
    /// The caret's UTF-16 offset once everything typed so far has landed.
    expected: cf::Index,
    /// The end of the text typed last. `None` before the first segment.
    ending: Option<String>,
    tracking: Tracking,
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

impl CaretTrack {
    fn new(caret: cf::Index) -> Self {
        Self {
            expected: caret,
            ending: None,
            tracking: Tracking::Untested(caret),
        }
    }

    /// The first check comes a whole segment after the first one landed. A
    /// caret still at its start by then means the field never moves it. For
    /// the closing space, a caret short of the text always refuses.
    fn settle(&mut self, settled: Settled, insertion: Insertion) -> Result<(), DeliveryError> {
        match settled {
            // The closing space proves the caret moves too, so a later
            // Return refuses a caret back at the start.
            Settled::After(caret) => {
                self.expected = caret;
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
            Settled::Left => return Err(target_changed("another element took focus")),
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
        self.expected = advanced(self.expected, text);
        self.ending = Some(ending_of(text));
    }

    /// A lone space would match any caret after a space, so it extends the
    /// segment's ending.
    fn after_closing_space(&mut self, space: &str) {
        self.expected = advanced(self.expected, space);
        self.ending = Some(ending_after(self.ending.as_deref(), space));
    }
}

impl Anchor {
    fn new(focus: Focus) -> Self {
        Self {
            application: focus.application,
            element: focus.element,
            caret: focus
                .selection
                .map(|selection| CaretTrack::new(selection.loc)),
        }
    }

    /// The user may have typed or clicked between the press and its first
    /// segment, so the caret is read again then.
    fn refresh(&mut self, focus: &Focus) {
        if !self.element.equal(&focus.element) {
            return;
        }
        match (&mut self.caret, focus.selection) {
            (Some(track), Some(selection)) if track.ending.is_none() => {
                *track = CaretTrack::new(selection.loc);
            }
            (None, Some(selection)) => self.caret = Some(CaretTrack::new(selection.loc)),
            (Some(_), _) | (None, None) => {}
        }
    }

    /// Where typing continues. Before any text, the track starts again from
    /// this caret, as `refresh` does. A field whose first selection read
    /// failed is tracked from the first caret it shows.
    fn follow(&mut self, caret: cf::Index) {
        match &mut self.caret {
            Some(track) if track.ending.is_some() => track.expected = caret,
            Some(_) | None => self.caret = Some(CaretTrack::new(caret)),
        }
    }

    fn check(&mut self) -> Result<(), DeliveryError> {
        let started = Instant::now();
        let deadline = started + CATCH_UP_BUDGET;
        // Before any text, and where no caret shows, only the element can be
        // checked.
        let Some(
            track @ CaretTrack {
                ending: Some(ending),
                ..
            },
        ) = &self.caret
        else {
            return match self.holds_focus(deadline) {
                Ok(false) => Err(target_changed("another element took focus")),
                Ok(true) | Err(_) => Ok(()),
            };
        };
        let settled = match track.tracking {
            Tracking::Untested(_) => {
                let settable = element::is_selection_settable(&self.element, deadline);
                let polls_until = first_check_polls_until(started, settable);
                wait_for_caret(polls_until, || {
                    self.observe(track, ending, Insertion::Segment, deadline)
                })
            }
            Tracking::Caret => wait_for_caret(deadline, || {
                self.observe(track, ending, Insertion::Segment, deadline)
            }),
            Tracking::Field => self.observe_element(track, deadline),
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

    /// One look with no wait, since `check` already waited for the app.
    /// Returns the caret typing continues from, where one shows. Each
    /// insertion meets the rule its earlier check applied.
    fn look(
        &self,
        focus: &Focus,
        insertion: Insertion,
        deadline: Instant,
    ) -> Result<Option<cf::Index>, DeliveryError> {
        if !self.element.equal(&focus.element) {
            return Err(target_changed("another element took focus"));
        }
        let Some(track) = &self.caret else {
            return Ok(focus.selection.map(|selection| selection.loc));
        };
        let selection = focus
            .selection
            .ok_or_else(|| target_changed("the field stopped showing its caret"))?;
        if insertion == Insertion::ClosingSpace && selection.len > 0 {
            return Err(caret_moved("text selected"));
        }
        let Some(ending) = &track.ending else {
            return Ok(Some(selection.loc));
        };
        let tracks_field = track.tracking == Tracking::Field;
        if tracks_field {
            if insertion == Insertion::Segment {
                return Ok(Some(track.expected));
            }
            // As in `check_closing_space`, only a caret that can't be set
            // skips the caret rule.
            if element::is_selection_settable(&self.element, deadline) == Ok(false) {
                self.confirm(selection, deadline)?;
                return Ok(Some(track.expected));
            }
        }
        match self.verdict(track, ending, selection, insertion, deadline) {
            Verdict::Continues(caret) => {
                // Any read after the selection's gives the user time to move
                // it: the settability question, a selection, or a caret off
                // the expected offset, which make the verdict read more.
                if tracks_field || selection.len > 0 || caret != track.expected {
                    self.confirm(selection, deadline)?;
                }
                Ok(Some(caret))
            }
            Verdict::Left => Err(target_changed("another element took focus")),
            Verdict::Behind(_) => Err(caret_moved("caret moved back before posting")),
            Verdict::Moved(reason) => Err(caret_moved(reason)),
        }
    }

    /// A field keeps answering reads after focus or its selection moves on.
    /// So the selection is read again, then focus, and neither may have
    /// changed.
    fn confirm(&self, selection: cf::Range, deadline: Instant) -> Result<(), DeliveryError> {
        let now = element::selection(&self.element, deadline).map_err(target_changed)?;
        if (now.loc, now.len) != (selection.loc, selection.len) {
            return Err(caret_moved("selection changed during the last look"));
        }
        match self.holds_focus(deadline) {
            Ok(true) => Ok(()),
            Ok(false) => Err(target_changed("another element took focus")),
            Err(reason) => Err(target_changed(reason)),
        }
    }

    fn check_closing_space(&mut self) -> Result<(), DeliveryError> {
        let deadline = Instant::now() + CATCH_UP_BUDGET;
        // Nothing shows where the space would go.
        let Some(track) = &self.caret else {
            return Err(DeliveryError::TargetUnverified);
        };
        if element::is_selection_settable(&self.element, deadline) == Ok(false) {
            return self.check();
        }
        let Some(ending) = &track.ending else {
            return Err(DeliveryError::TargetUnverified);
        };
        let settled = wait_for_caret(deadline, || {
            self.observe(track, ending, Insertion::ClosingSpace, deadline)
        });
        self.settle(settled, Insertion::ClosingSpace)
    }

    fn settle(&mut self, settled: Settled, insertion: Insertion) -> Result<(), DeliveryError> {
        match &mut self.caret {
            Some(track) => track.settle(settled, insertion),
            None => Ok(()),
        }
    }

    fn resting_caret(&self, deadline: Instant) -> Option<(arc::R<ax::UiElement>, cf::Index)> {
        let track = self.caret.as_ref()?;
        let focused = element::focused_element(&self.application, deadline).ok()?;
        if !self.element.equal(&focused) {
            return None;
        }
        // A selection, inline completions included, hides the ghost so it
        // never covers highlighted text.
        let selection = element::selection(&self.element, deadline).ok()?;
        if selection.len != 0 {
            return None;
        }
        if selection.loc == track.expected {
            return Some((focused, selection.loc));
        }
        // If the app changed the typed text's length, its ending still marks
        // where typing continues.
        let ending = track.ending.as_deref()?;
        (self.text_before(ending, selection.loc, deadline).as_deref() == Some(ending))
            .then_some((focused, selection.loc))
    }

    fn observe(
        &self,
        track: &CaretTrack,
        ending: &str,
        insertion: Insertion,
        deadline: Instant,
    ) -> Result<Verdict, &'static str> {
        if !self.holds_focus(deadline)? {
            return Ok(Verdict::Left);
        }
        let selection = element::selection(&self.element, deadline)?;
        Ok(self.verdict(track, ending, selection, insertion, deadline))
    }

    fn verdict(
        &self,
        track: &CaretTrack,
        ending: &str,
        selection: cf::Range,
        insertion: Insertion,
        deadline: Instant,
    ) -> Verdict {
        let at_expected = selection.loc == track.expected;
        let before = (insertion == Insertion::Segment && selection.len == 0 && !at_expected)
            .then(|| self.text_before(ending, selection.loc, deadline))
            .flatten();
        let field_length = (selection.len > 0 && at_expected)
            .then(|| element::field_length(&self.element, deadline).ok())
            .flatten();
        judge(
            track.expected,
            ending,
            selection,
            insertion,
            before.as_deref(),
            field_length,
        )
    }

    /// For a caret that stays put: the element still holding focus is all
    /// that shows.
    fn observe_element(&self, track: &CaretTrack, deadline: Instant) -> Settled {
        match self.holds_focus(deadline) {
            Ok(true) => Settled::After(track.expected),
            Ok(false) => Settled::Left,
            Err(reason) => Settled::Unknown(reason),
        }
    }

    fn holds_focus(&self, deadline: Instant) -> Result<bool, &'static str> {
        element::focused_element(&self.application, deadline)
            .map(|focused| self.element.equal(&focused))
    }

    fn text_before(&self, ending: &str, caret: cf::Index, deadline: Instant) -> Option<String> {
        let units = cf::Index::try_from(ending.encode_utf16().count()).ok()?;
        if caret < units {
            return None;
        }
        element::limit(&self.element, deadline).ok()?;
        element::string_for_range(&self.element, caret - units, units)
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
            typed_track()
                .settle(Settled::After(9), Insertion::ClosingSpace)
                .is_ok()
        );
        assert!(matches!(
            typed_track().settle(Settled::Unknown("no selection"), Insertion::ClosingSpace),
            Err(DeliveryError::TargetUnverified)
        ));
        assert!(matches!(
            typed_track().settle(Settled::Short(4), Insertion::ClosingSpace),
            Err(DeliveryError::CaretMoved)
        ));
    }

    #[test]
    fn the_owed_space_never_takes_a_caret_at_its_start_for_a_field_that_stays_put() {
        let mut track = typed_track();
        track.tracking = Tracking::Untested(4);

        assert!(matches!(
            track.settle(Settled::Short(4), Insertion::ClosingSpace),
            Err(DeliveryError::CaretMoved)
        ));
        assert_eq!(track.tracking, Tracking::Untested(4));
    }

    #[test]
    fn a_return_after_the_space_refuses_a_caret_moved_back_to_the_start() {
        let mut track = typed_track();
        track.tracking = Tracking::Untested(4);

        assert!(
            track
                .settle(Settled::After(9), Insertion::ClosingSpace)
                .is_ok()
        );
        assert_eq!(track.tracking, Tracking::Caret);
        assert!(matches!(
            track.settle(Settled::Short(4), Insertion::Segment),
            Err(DeliveryError::CaretMoved)
        ));
    }

    #[test]
    fn a_field_checked_only_for_focus_still_reads_the_caret_for_the_space() {
        // Pid 1 exposes no field, so every read fails.
        let mut anchoring = Anchoring::Anchored(anchor_with(CaretTrack {
            tracking: Tracking::Field,
            ..typed_track()
        }));

        assert!(matches!(
            anchoring.before_closing_space(),
            Err(DeliveryError::TargetUnverified)
        ));
    }

    #[test]
    fn a_segment_ends_on_its_own_text_and_the_space_on_the_segment() {
        let mut track = typed_track();
        track.after_typing(" go");
        assert_eq!(track.ending.as_deref(), Some(" go"));
        track.after_closing_space(" ");
        assert_eq!(track.ending.as_deref(), Some(" go "));
        assert_eq!(track.expected, 13);

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
        let mut anchor = anchor_with(CaretTrack {
            expected: 5,
            ending: None,
            tracking: Tracking::Caret,
        });

        assert!(anchor.check().is_ok());
        assert_eq!(anchor.caret.map(|track| track.expected), Some(5));
    }

    fn focus(pid: i32, selection: cf::Range) -> Focus {
        Focus {
            application: ax::UiElement::with_app_pid(pid),
            element: ax::UiElement::with_app_pid(pid),
            selection: Some(selection),
        }
    }

    /// A focus that shows no caret, as a native document that hides its
    /// text reports.
    fn document(pid: i32) -> Focus {
        Focus {
            selection: None,
            ..focus(pid, caret(0))
        }
    }

    fn soon() -> Instant {
        Instant::now() + CATCH_UP_BUDGET
    }

    #[test]
    fn the_field_that_passes_the_last_look_binds_an_unbound_press() {
        for mut anchoring in [Anchoring::Pending, Anchoring::Unavailable] {
            let looked = anchoring.look(Ok(focus(1, caret(4))), Post::Segment, soon());
            let Ok(Some(look)) = looked else {
                panic!("a text field binds the press");
            };
            assert!(!matches!(anchoring, Anchoring::Anchored(_)));

            anchoring.accept(look);

            assert!(
                matches!(&anchoring, Anchoring::Anchored(Anchor { caret: Some(track), .. }) if track.expected == 4)
            );
        }
    }

    #[test]
    fn the_first_segment_starts_from_the_caret_its_last_look_read() {
        let mut anchoring = Anchoring::Anchored(anchor_with(CaretTrack::new(4)));

        let Ok(Some(look)) = anchoring.look(Ok(focus(1, caret(7))), Post::Segment, soon()) else {
            panic!("the bound field is looked");
        };
        anchoring.accept(look);

        assert!(matches!(
            &anchoring,
            Anchoring::Anchored(Anchor { caret: Some(track), .. })
                if *track == CaretTrack::new(7)
        ));
    }

    #[test]
    fn a_return_never_binds_a_field_the_press_never_typed_into() {
        for anchoring in [Anchoring::Pending, Anchoring::Unavailable] {
            assert!(matches!(
                anchoring.look(Ok(focus(1, caret(4))), Post::Return, soon()),
                Err(DeliveryError::TargetUnverified)
            ));
        }
    }

    #[test]
    fn a_document_that_hides_its_text_binds_by_identity_alone() {
        let mut anchoring = Anchoring::Unavailable;
        let Ok(Some(look)) = anchoring.look(Ok(document(1)), Post::Segment, soon()) else {
            panic!("a document binds the press");
        };
        anchoring.accept(look);

        assert!(matches!(
            anchoring.look(Ok(document(1)), Post::Return, soon()),
            Ok(None)
        ));
        assert!(matches!(
            anchoring.look(Ok(document(2)), Post::Segment, soon()),
            Err(DeliveryError::TargetChanged)
        ));
        assert!(matches!(
            anchoring.check(Post::ClosingSpace),
            Err(DeliveryError::TargetUnverified)
        ));
    }

    #[test]
    fn an_unbound_press_tells_an_unanswered_read_from_no_text_field() {
        assert!(matches!(
            Anchoring::Pending.look(
                Err(Refusal::Unreadable("read budget spent")),
                Post::Segment,
                soon()
            ),
            Err(DeliveryError::FocusUnreadable)
        ));
    }

    #[test]
    fn no_text_field_reads_as_focus_lost_once_the_press_is_bound() {
        let unbound = Anchoring::Pending;
        let bound = Anchoring::Anchored(typed_anchor());
        let lost = || Err(Refusal::NoText("focus takes no text"));

        assert!(matches!(
            unbound.look(lost(), Post::Segment, soon()),
            Err(DeliveryError::NoTextField)
        ));
        assert!(matches!(
            bound.look(lost(), Post::Segment, soon()),
            Err(DeliveryError::TargetChanged)
        ));
    }

    #[test]
    fn the_last_look_refuses_another_field_in_the_app() {
        let anchor = typed_anchor();

        assert!(matches!(
            anchor.look(&focus(2, caret(9)), Insertion::Segment, soon()),
            Err(DeliveryError::TargetChanged)
        ));
    }

    #[test]
    fn a_selection_made_after_the_check_keeps_the_space_from_replacing_it() {
        let selected = cf::Range { loc: 9, len: 3 };
        assert!(matches!(
            typed_anchor().look(&focus(1, caret(9)), Insertion::ClosingSpace, soon()),
            Ok(Some(9))
        ));
        for tracking in [Tracking::Caret, Tracking::Field] {
            let anchor = anchor_with(CaretTrack {
                tracking,
                ..typed_track()
            });

            assert!(matches!(
                anchor.look(&focus(1, selected), Insertion::ClosingSpace, soon()),
                Err(DeliveryError::CaretMoved)
            ));
        }
    }

    #[test]
    fn a_space_in_a_field_that_sets_its_caret_still_needs_the_caret() {
        // Pid 1 answers no settability, which counts as settable.
        let anchor = anchor_with(CaretTrack {
            tracking: Tracking::Field,
            ..typed_track()
        });

        assert!(matches!(
            anchor.look(&focus(1, caret(2)), Insertion::ClosingSpace, soon()),
            Err(DeliveryError::CaretMoved)
        ));
    }

    #[test]
    fn a_space_after_the_settable_question_reads_the_selection_again() {
        // Pid 1 answers no reads, so the second selection read refuses.
        let anchor = anchor_with(CaretTrack {
            tracking: Tracking::Field,
            ..typed_track()
        });

        assert!(matches!(
            anchor.look(&focus(1, caret(9)), Insertion::ClosingSpace, soon()),
            Err(DeliveryError::TargetChanged)
        ));
    }

    #[test]
    fn a_caret_that_shows_after_binding_starts_the_tracking() {
        let mut anchoring = Anchoring::Unavailable;
        let Ok(Some(look)) = anchoring.look(Ok(document(1)), Post::Segment, soon()) else {
            panic!("a field that hides its caret binds the press");
        };
        anchoring.accept(look);

        let Ok(Some(look)) = anchoring.look(Ok(focus(1, caret(4))), Post::Segment, soon()) else {
            panic!("the caret it shows now is looked");
        };
        anchoring.accept(look);

        assert!(matches!(
            &anchoring,
            Anchoring::Anchored(Anchor { caret: Some(track), .. })
                if *track == CaretTrack::new(4)
        ));
    }

    #[test]
    fn a_refresh_tracks_a_caret_the_bound_field_hid() {
        let mut anchor = Anchor::new(document(1));

        anchor.refresh(&focus(1, caret(4)));

        assert_eq!(anchor.caret, Some(CaretTrack::new(4)));
    }

    fn typed_track() -> CaretTrack {
        CaretTrack {
            expected: 9,
            ending: Some("hello".to_owned()),
            tracking: Tracking::Caret,
        }
    }

    fn anchor_with(track: CaretTrack) -> Anchor {
        Anchor {
            application: ax::UiElement::with_app_pid(1),
            element: ax::UiElement::with_app_pid(1),
            caret: Some(track),
        }
    }

    fn typed_anchor() -> Anchor {
        anchor_with(typed_track())
    }

    #[test]
    fn a_caret_still_at_its_start_by_the_first_check_is_ignored() {
        let mut track = typed_track();
        track.tracking = Tracking::Untested(4);

        let result = track.settle(Settled::Short(4), Insertion::Segment);

        assert!(result.is_ok());
        assert_eq!(track.tracking, Tracking::Field);
    }

    #[test]
    fn a_caret_that_moved_with_the_first_segment_is_tracked() {
        let mut track = typed_track();
        track.tracking = Tracking::Untested(4);

        let result = track.settle(Settled::After(10), Insertion::Segment);

        assert!(result.is_ok());
        assert_eq!(track.tracking, Tracking::Caret);
        assert_eq!(track.expected, 10);
    }

    #[test]
    fn a_caret_short_of_the_text_but_off_its_start_stops_the_first_check() {
        let mut track = typed_track();
        track.tracking = Tracking::Untested(4);

        let result = track.settle(Settled::Short(6), Insertion::Segment);

        assert!(matches!(result, Err(DeliveryError::CaretMoved)));
    }

    #[test]
    fn a_tracked_caret_back_at_the_start_stops_typing() {
        let mut track = typed_track();

        let result = track.settle(Settled::Short(4), Insertion::Segment);

        assert!(matches!(result, Err(DeliveryError::CaretMoved)));
    }

    #[test]
    fn a_field_unreadable_at_first_can_still_anchor_later() {
        let mut anchoring = Anchoring::Pending;

        anchoring.start(Err(Refusal::Unreadable("too slow")));
        assert!(anchoring.is_unavailable());
        anchoring.start(Err(Refusal::Unreadable("too slow")));
        assert!(anchoring.is_unavailable());
        anchoring.start(Ok(focus(1, caret(3))));
        assert!(matches!(anchoring, Anchoring::Anchored(_)));
        anchoring.start(Err(Refusal::Unreadable("too slow")));
        assert!(matches!(anchoring, Anchoring::Anchored(_)));
    }

    #[test]
    fn a_read_before_any_text_moves_only_the_expected_caret() {
        let mut anchor = anchor_with(CaretTrack {
            ending: None,
            ..typed_track()
        });

        anchor.refresh(&focus(1, caret(3)));
        let track = anchor.caret.as_mut().expect("a caret track");
        assert_eq!(
            (track.expected, &track.tracking),
            (3, &Tracking::Untested(3))
        );

        track.ending = Some("hello".to_owned());
        anchor.refresh(&focus(1, caret(7)));
        assert_eq!(anchor.caret.map(|track| track.expected), Some(3));
    }

    #[test]
    fn another_field_taking_focus_is_told_apart_from_a_moved_caret() {
        let mut track = typed_track();

        assert!(matches!(
            track.settle(Settled::Left, Insertion::Segment),
            Err(DeliveryError::TargetChanged)
        ));
        assert!(matches!(
            track.settle(Settled::Moved("caret moved"), Insertion::Segment),
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
