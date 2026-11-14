use super::execution::{ArmedReturn, TakenReturn};
use super::{
    Delivered, DeliveryMode, DeliveryOutcome, DictationError, DictationSettings, ExecutionControl,
    Spacing, TranscriptSink, Utterance,
};
use aria_platform::delivery::{
    Caret, DeliveryError, Finished, OutputMode, Placement, TextDelivery,
};
use std::sync::Arc;

/// The frontmost app as a transcript sink. Streaming types each segment as
/// it arrives; complete types the whole transcript once. A transcript-only
/// press binds no target and types nothing.
pub struct Cursor<D: TextDelivery> {
    delivery: D,
    target: Option<Target<D::Prepared>>,
}

/// Where the dictation under way lands, picked before any text.
struct Target<P> {
    policy: aria_platform::delivery::TargetPolicy,
    /// The app in front as the press started, when the platform can tell.
    start: Option<aria_platform::delivery::Application>,
    mode: DeliveryMode,
    absence: Absence,
    control: Arc<ExecutionControl>,
    prepared: P,
    /// Holds Return back while the text is still coming. `None` once the
    /// press stops typing, or when it never types into an app.
    armed: Option<ArmedReturn>,
    /// Read from the field before the first segment, then moved past each
    /// segment typed.
    caret: Option<Caret>,
    /// Text not typed yet, in order: the utterance being delivered, and
    /// while streaming, every segment decoded since the field lost focus.
    queue: Vec<Queued>,
    /// The text typed last held its closing mark back, so the queue opens
    /// with that mark.
    owes_mark: bool,
    finished: bool,
    delivered: Option<Delivered>,
}

struct Queued {
    text: String,
    spacing: Spacing,
}

/// What a press does with text decoded while its field lacks focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Absence {
    /// Typing stops for good. Complete delivery has one chance, and
    /// submission must not press Return after the user went elsewhere.
    Stops,
    /// The text waits for the field to regain focus. A press that ends
    /// first leaves it where the output mode allows.
    Queues(Leftover),
}

/// Where queued text goes when the press ends away from its field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leftover {
    /// The user chose insert or copy.
    Clipboard,
    /// Only history keeps it: plain insertion never touches the clipboard.
    History,
    /// Nothing keeps it: history saving is off.
    Nowhere,
}

impl<P> Target<P> {
    /// Ends typing for good, with the Return the person pressed meanwhile.
    fn finish(&mut self) -> Option<TakenReturn> {
        self.finished = true;
        self.armed.take().and_then(ArmedReturn::finish)
    }
}

impl<D: TextDelivery> Cursor<D> {
    pub fn new(delivery: D) -> Self {
        Self {
            delivery,
            target: None,
        }
    }

    /// Types the utterance, after any queued text, where the press landed.
    fn deliver(
        &mut self,
        mode: DeliveryMode,
        utterance: &Utterance<'_>,
    ) -> Result<(), DeliveryError> {
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        if target.finished || target.mode != mode {
            return Ok(());
        }
        target.queue.push(Queued {
            text: utterance.text.to_owned(),
            spacing: utterance.spacing,
        });
        self.type_queue()
    }

    /// Types every queued utterance at once. When the press queues, a
    /// refusal that typed nothing keeps the queue.
    fn type_queue(&mut self) -> Result<(), DeliveryError> {
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        if target.queue.is_empty() {
            return Ok(());
        }
        let typed = target.policy.confirm_front(&self.delivery).and_then(|()| {
            // Kept only once text lands: a read while the field is away
            // would space the queue against the wrong text.
            let mut caret = target
                .caret
                .unwrap_or_else(|| self.delivery.caret(&target.prepared));
            let spaced = spaced(&target.queue, &mut caret);
            let outcome =
                self.delivery
                    .deliver(&target.prepared, &spaced, should_cancel(&target.control))?;
            Ok((outcome, caret))
        });
        match typed {
            Ok((outcome, caret)) => {
                target.caret = Some(caret);
                target.owes_mark = target
                    .queue
                    .last()
                    .is_some_and(|last| last.spacing.defers_ending);
                target.queue.clear();
                target.delivered = Some(Delivered {
                    outcome: outcome.into(),
                    words: None,
                });
                Ok(())
            }
            Err(DeliveryError::TargetChanged | DeliveryError::ApplicationExcluded)
                if matches!(target.absence, Absence::Queues(_)) =>
            {
                Ok(())
            }
            Err(error) => {
                // A Return taken earlier stays unpressed: it would send
                // text cut short. One pressed from here on reaches the app.
                target.finish();
                Err(error)
            }
        }
    }

    /// Presses the Return the person pressed while the text was coming. It
    /// is their key, not the transcript, so a refusal fails nothing.
    /// True once pressed.
    fn press_return(delivery: &D, target: &Target<D::Prepared>) -> bool {
        if target.control.should_stop() {
            return false;
        }
        delivery
            .press_return(&target.prepared, should_cancel(&target.control))
            .inspect_err(|error| {
                tracing::warn!(%error, "the Return the person pressed was not pressed");
            })
            .is_ok()
    }

    /// The press ended with text still queued and its field away.
    fn leave_queue(&mut self) -> Result<(), DeliveryError> {
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        // The mark owed to typed text belongs to the field, not the
        // clipboard; a queue of only that mark leaves nothing behind.
        if target.owes_mark
            && let Some(first) = target.queue.first_mut()
        {
            first.text = without_owed_mark(&first.text).to_owned();
        }
        let spaced = spaced(&target.queue, &mut Caret::default());
        let text = spaced.trim_matches(' ');
        if text.is_empty() {
            target.queue.clear();
            return Ok(());
        }
        let words = Some(text.split_whitespace().count() as u32);
        let kept = match target.absence {
            Absence::Queues(Leftover::History) => Some(DeliveryOutcome::InHistory),
            Absence::Queues(Leftover::Nowhere) => Some(DeliveryOutcome::NotTyped),
            Absence::Queues(Leftover::Clipboard) | Absence::Stops => None,
        };
        if let Some(outcome) = kept {
            target.queue.clear();
            target.delivered = Some(Delivered { outcome, words });
            return Ok(());
        }
        // An app with Aria off gets nothing, not even the clipboard. Settings
        // chosen for the start app don't apply: it isn't in front by now.
        if target.policy.excludes(self.delivery.frontmost().as_ref()) {
            return Err(DeliveryError::ApplicationExcluded);
        }
        let clipboard = self
            .delivery
            .prepare(OutputMode::Copy, should_cancel(&target.control))?;
        let outcome = self
            .delivery
            .deliver(&clipboard, text, should_cancel(&target.control))?;
        target.queue.clear();
        target.delivered = Some(Delivered {
            outcome: outcome.into(),
            words,
        });
        Ok(())
    }
}

/// `text` without the closing mark `SentenceEnds` opened it with.
fn without_owed_mark(text: &str) -> &str {
    text.strip_prefix(['.', '?', '!'])
        .map_or(text, str::trim_start)
}

/// The queue as one string, spaced from `caret`, which moves past it.
fn spaced(queue: &[Queued], caret: &mut Caret) -> String {
    queue
        .iter()
        .map(|queued| queued.spacing.insert(&queued.text, caret))
        .collect()
}

impl<D: TextDelivery> TranscriptSink for Cursor<D> {
    type Error = DeliveryError;
    const STREAMS: bool = D::STREAMS;

    fn dictation_started(
        &mut self,
        settings: &DictationSettings,
        control: &Arc<ExecutionControl>,
    ) -> Result<(), DeliveryError> {
        if !settings.route.delivers() {
            self.target = None;
            return Ok(());
        }
        let output = match settings.delivery_mode {
            DeliveryMode::Complete => settings.output_mode,
            DeliveryMode::Streaming => streaming_output_mode(settings.output_mode)
                .ok_or(DeliveryError::InsertionUnsupported)?,
        };
        let output = if settings.output_mode == OutputMode::InsertAndSubmit {
            OutputMode::InsertAndSubmit
        } else {
            output
        };
        let absence = match (settings.delivery_mode, settings.output_mode) {
            (DeliveryMode::Complete, _) | (_, OutputMode::InsertAndSubmit) => Absence::Stops,
            (DeliveryMode::Streaming, OutputMode::InsertOrCopy) => {
                Absence::Queues(Leftover::Clipboard)
            }
            (DeliveryMode::Streaming, _) if settings.history_enabled => {
                Absence::Queues(Leftover::History)
            }
            (DeliveryMode::Streaming, _) => Absence::Queues(Leftover::Nowhere),
        };
        settings.target.confirm_front(&self.delivery)?;
        let prepared = self.delivery.prepare(output, should_cancel(control))?;
        settings.target.confirm_front(&self.delivery)?;
        // The read binds a queueing press to the field it starts in, before
        // the user can leave it. Spacing is read again when text types.
        if matches!(absence, Absence::Queues(_)) {
            let _ = self.delivery.caret(&prepared);
        }
        let armed = settings
            .return_sends
            .then(|| self.delivery.target_process(&prepared))
            .flatten()
            .map(|process| control.arm_return(process));
        self.target = Some(Target {
            policy: settings.target.clone(),
            start: self.delivery.frontmost(),
            mode: settings.delivery_mode,
            absence,
            control: Arc::clone(control),
            prepared,
            armed,
            caret: None,
            queue: Vec::new(),
            owes_mark: false,
            finished: false,
            delivered: None,
        });
        Ok(())
    }

    fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), DeliveryError> {
        self.deliver(DeliveryMode::Streaming, &utterance)
    }

    fn completed(&mut self, utterance: Utterance<'_>) -> Result<(), DeliveryError> {
        let Some(target) = self.target.as_ref() else {
            return Ok(());
        };
        if target.finished {
            return Ok(());
        }
        match target.mode {
            DeliveryMode::Complete => self.deliver(DeliveryMode::Complete, &utterance)?,
            // The user may be back in the field by the time they stop.
            DeliveryMode::Streaming => {
                self.type_queue()?;
                self.leave_queue()?;
            }
        }
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        // Disarmed first, so a Return pressed from here on reaches the app.
        // Dropped on every way out below, until pressed.
        let taken = target.finish();
        // Text left behind, in history or on the clipboard, means a Return
        // would send something else.
        if target.delivered.map(|delivered| delivered.outcome) != Some(DeliveryOutcome::Inserted) {
            return Ok(());
        }
        let finished = self
            .delivery
            .finish(&target.prepared, should_cancel(&target.control))?;
        if let Some(taken) = taken {
            let pressed = match finished {
                Finished::Submitted => true,
                Finished::Typed => Self::press_return(&self.delivery, target),
            };
            if pressed {
                taken.mark_pressed();
            }
        }
        Ok(())
    }

    fn delivered(&self) -> Option<Delivered> {
        self.target.as_ref()?.delivered
    }

    fn is_queueing(&self) -> bool {
        self.target
            .as_ref()
            .is_some_and(|target| !target.finished && !target.queue.is_empty())
    }

    fn retry(&mut self) -> Result<(), DeliveryError> {
        let Some(target) = self.target.as_ref() else {
            return Ok(());
        };
        if target.finished {
            return Ok(());
        }
        // A cheap look at the app in front keeps Accessibility reads off
        // this tick while the user is away.
        if target.start.is_some() && self.delivery.frontmost() != target.start {
            return Ok(());
        }
        self.type_queue()
    }

    fn placement(&self) -> Option<Placement> {
        let target = self.target.as_ref()?;
        if target.finished || target.mode != DeliveryMode::Streaming {
            return None;
        }
        self.delivery.placement(&target.prepared)
    }

    fn dictation_finished(&mut self, result: Result<(), &DictationError>) {
        // A press that heard no speech still owes the person their Return.
        // A failure keeps it: the text was cut short. Only a field bound as
        // the press started shows the Return lands where it was pressed.
        let Some(mut target) = self.target.take() else {
            return;
        };
        let Some(taken) = target.finish() else {
            return;
        };
        let owed = matches!(result, Err(DictationError::NoSpeech))
            && target.delivered.is_none()
            && matches!(target.absence, Absence::Queues(_));
        if owed && Self::press_return(&self.delivery, &target) {
            taken.mark_pressed();
        }
    }
}

fn should_cancel(control: &ExecutionControl) -> impl Fn() -> bool + '_ {
    || control.should_stop()
}

// Streaming inserts each segment as it finalizes. Only text still queued
// when the press ends away from its field may reach the clipboard, and only
// under insert or copy.
fn streaming_output_mode(mode: OutputMode) -> Option<OutputMode> {
    match mode {
        OutputMode::Insert | OutputMode::InsertAndSubmit | OutputMode::InsertOrCopy => {
            Some(OutputMode::Insert)
        }
        OutputMode::Copy => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::dictation::StopMode;
    use crate::dictation::testing::{
        CallLog, PlacedDelivery, RecordingDelivery, TYPED_PROCESS, settings,
    };

    use super::*;
    use crate::dictation::ActiveDictation;
    use aria_platform::delivery::DeliveryOutcome as Posted;

    fn cursor() -> (CallLog, Cursor<RecordingDelivery>) {
        let log = CallLog::default();
        let cursor = log.cursor();
        (log, cursor)
    }

    /// Types like `RecordingDelivery` until the field goes away, then
    /// refuses as told. A `Copy` target always takes the text.
    struct Wandering {
        delivery: RecordingDelivery,
        refusal: std::sync::Mutex<Option<fn() -> DeliveryError>>,
        /// Refuses the person's Return. The field leaving alone doesn't:
        /// only the app is checked, as with an unreadable field.
        return_refusal: std::sync::Mutex<Option<fn() -> DeliveryError>>,
        front: std::sync::Mutex<Option<aria_platform::delivery::Application>>,
        caret: std::sync::Mutex<Caret>,
        caret_reads: std::sync::atomic::AtomicUsize,
    }

    impl Wandering {
        fn leave(&self, refusal: fn() -> DeliveryError) {
            *self.refusal.lock().unwrap() = Some(refusal);
        }

        fn come_back(&self) {
            *self.refusal.lock().unwrap() = None;
        }
    }

    impl TextDelivery for Wandering {
        type Prepared = OutputMode;
        const DEFAULT_MODE: OutputMode = OutputMode::Insert;
        const OUTLIVES_PROCESS: bool = true;

        fn frontmost(&self) -> Option<aria_platform::delivery::Application> {
            self.front.lock().unwrap().clone()
        }

        fn prepare(
            &self,
            mode: OutputMode,
            should_cancel: impl Fn() -> bool,
        ) -> Result<OutputMode, DeliveryError> {
            self.delivery.prepare(mode, should_cancel)
        }

        fn finish(
            &self,
            prepared: &OutputMode,
            should_cancel: impl Fn() -> bool,
        ) -> Result<Finished, DeliveryError> {
            self.delivery.finish(prepared, should_cancel)
        }

        fn deliver(
            &self,
            prepared: &OutputMode,
            text: &str,
            should_cancel: impl Fn() -> bool,
        ) -> Result<Posted, DeliveryError> {
            if *prepared == OutputMode::Copy {
                self.delivery.deliver(prepared, text, should_cancel)?;
                return Ok(Posted::Copied);
            }
            if let Some(refusal) = *self.refusal.lock().unwrap() {
                return Err(refusal());
            }
            self.delivery.deliver(prepared, text, should_cancel)
        }

        fn target_process(&self, prepared: &OutputMode) -> Option<u32> {
            self.delivery.target_process(prepared)
        }

        fn press_return(
            &self,
            prepared: &OutputMode,
            should_cancel: impl Fn() -> bool,
        ) -> Result<(), DeliveryError> {
            if let Some(refusal) = *self.return_refusal.lock().unwrap() {
                return Err(refusal());
            }
            self.delivery.press_return(prepared, should_cancel)
        }

        fn caret(&self, _prepared: &OutputMode) -> Caret {
            self.caret_reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *self.caret.lock().unwrap()
        }
    }

    fn wandering(log: &CallLog) -> Wandering {
        Wandering {
            delivery: RecordingDelivery {
                log: log.clone(),
                fail: false,
            },
            refusal: std::sync::Mutex::new(None),
            return_refusal: std::sync::Mutex::new(None),
            front: std::sync::Mutex::new(None),
            caret: std::sync::Mutex::new(Caret::default()),
            caret_reads: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A streaming press in `mode` that typed `first`, then lost its field.
    fn left_after(mode: OutputMode, first: Utterance<'_>) -> (CallLog, Cursor<Wandering>) {
        let log = CallLog::default();
        let mut cursor = Cursor::new(wandering(&log));
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(&settings(DeliveryMode::Streaming, mode), &control)
            .unwrap();
        cursor.segment(first).unwrap();
        cursor.delivery.leave(|| DeliveryError::TargetChanged);
        (log, cursor)
    }

    fn left_after_hello(mode: OutputMode) -> (CallLog, Cursor<Wandering>) {
        left_after(mode, Utterance::anonymous("hello"))
    }

    fn deferring(text: &str) -> Utterance<'_> {
        Utterance {
            spacing: Spacing {
                defers_ending: true,
                ..Spacing::default()
            },
            ..Utterance::anonymous(text)
        }
    }

    #[test]
    fn a_retry_types_the_queue_only_once_the_start_app_is_back_in_front() {
        let app = |id: &str| aria_platform::delivery::Application {
            name: id.into(),
            bundle_id: Some(id.into()),
        };
        let log = CallLog::default();
        let mut cursor = Cursor::new(wandering(&log));
        *cursor.delivery.front.lock().unwrap() = Some(app("com.notes"));
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &ExecutionControl::for_test(StopMode::Manual),
            )
            .unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        cursor.delivery.leave(|| DeliveryError::TargetChanged);
        *cursor.delivery.front.lock().unwrap() = Some(app("com.safari"));
        cursor.segment(Utterance::anonymous("there")).unwrap();

        cursor.delivery.come_back();
        cursor.retry().unwrap();
        assert!(cursor.is_queueing(), "another app is still in front");

        *cursor.delivery.front.lock().unwrap() = Some(app("com.notes"));
        cursor.retry().unwrap();

        assert!(!cursor.is_queueing());
        assert_eq!(
            log.entries(),
            ["prepare:Insert", "deliver:hello", "deliver: there"]
        );
    }

    #[test]
    fn only_a_queueing_press_binds_its_field_as_it_starts() {
        for (mode, output, reads) in [
            (DeliveryMode::Streaming, OutputMode::Insert, 1),
            (DeliveryMode::Streaming, OutputMode::InsertAndSubmit, 0),
            (DeliveryMode::Complete, OutputMode::Insert, 0),
        ] {
            let mut cursor = Cursor::new(wandering(&CallLog::default()));
            cursor
                .dictation_started(
                    &settings(mode, output),
                    &ExecutionControl::for_test(StopMode::Manual),
                )
                .unwrap();
            assert_eq!(
                cursor
                    .delivery
                    .caret_reads
                    .load(std::sync::atomic::Ordering::Relaxed),
                reads,
                "{mode:?} {output:?}"
            );
        }
    }

    #[test]
    fn settings_chosen_for_the_start_app_still_copy_its_leftover() {
        let notes = aria_platform::delivery::Application {
            name: "Notes".into(),
            bundle_id: Some("com.notes".into()),
        };
        let log = CallLog::default();
        let mut cursor = Cursor::new(wandering(&log));
        *cursor.delivery.front.lock().unwrap() = Some(notes);
        let mut settings = settings(DeliveryMode::Streaming, OutputMode::InsertOrCopy);
        settings.target.overridden_for = Some("com.notes".into());
        cursor
            .dictation_started(&settings, &ExecutionControl::for_test(StopMode::Manual))
            .unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        *cursor.delivery.front.lock().unwrap() = Some(aria_platform::delivery::Application {
            name: "Safari".into(),
            bundle_id: Some("com.safari".into()),
        });

        cursor.segment(Utterance::anonymous("there")).unwrap();
        cursor
            .completed(Utterance::anonymous("hello there"))
            .unwrap();

        assert_eq!(
            cursor.delivered().map(|delivered| delivered.outcome),
            Some(DeliveryOutcome::Copied)
        );
    }

    #[test]
    fn text_queued_while_an_app_with_aria_off_is_in_front_is_never_copied() {
        let log = CallLog::default();
        let mut cursor = Cursor::new(wandering(&log));
        let mut settings = settings(DeliveryMode::Streaming, OutputMode::InsertOrCopy);
        settings.target.excluded = vec!["com.secret".into()];
        cursor
            .dictation_started(&settings, &ExecutionControl::for_test(StopMode::Manual))
            .unwrap();
        *cursor.delivery.front.lock().unwrap() = Some(aria_platform::delivery::Application {
            name: "Vault".into(),
            bundle_id: Some("com.secret".into()),
        });

        cursor.segment(Utterance::anonymous("hello")).unwrap();
        let ended = cursor.completed(Utterance::anonymous("hello"));

        assert!(matches!(ended, Err(DeliveryError::ApplicationExcluded)));
        assert_eq!(log.entries(), ["prepare:Insert"]);
    }

    #[test]
    fn the_mark_owed_to_typed_text_is_never_left_behind() {
        for mode in [OutputMode::Insert, OutputMode::InsertOrCopy] {
            let (log, mut cursor) = left_after(mode, deferring("we should"));

            cursor.segment(Utterance::anonymous(".")).unwrap();
            cursor
                .completed(Utterance::anonymous("we should."))
                .unwrap();

            assert_eq!(log.entries(), ["prepare:Insert", "deliver:we should"]);
        }
    }

    #[test]
    fn copied_text_drops_the_mark_owed_to_typed_text() {
        let (log, mut cursor) = left_after(OutputMode::InsertOrCopy, deferring("we should"));

        cursor.segment(Utterance::anonymous(". Ship it")).unwrap();
        cursor
            .completed(Utterance::anonymous("we should. Ship it"))
            .unwrap();

        assert_eq!(log.entries().last().unwrap(), "deliver:Ship it");
        assert_eq!(
            cursor.delivered(),
            Some(Delivered {
                outcome: DeliveryOutcome::Copied,
                words: Some(2)
            })
        );
    }

    #[test]
    fn a_caret_read_while_the_field_is_away_does_not_space_the_queue() {
        let log = CallLog::default();
        let mut cursor = Cursor::new(wandering(&log));
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &ExecutionControl::for_test(StopMode::Manual),
            )
            .unwrap();
        cursor.delivery.leave(|| DeliveryError::TargetChanged);
        *cursor.delivery.caret.lock().unwrap() = Caret::between("Elsewhere.", "");

        cursor.segment(Utterance::anonymous("hello")).unwrap();
        cursor.delivery.come_back();
        *cursor.delivery.caret.lock().unwrap() = Caret::between("", "");
        cursor.segment(Utterance::anonymous("there")).unwrap();

        assert_eq!(log.entries(), ["prepare:Insert", "deliver:hello there"]);
    }

    #[test]
    fn text_decoded_away_is_typed_with_the_next_segment_once_focus_returns() {
        let (log, mut cursor) = left_after_hello(OutputMode::Insert);

        cursor.segment(Utterance::anonymous("there")).unwrap();
        assert!(cursor.is_queueing());
        cursor.delivery.come_back();
        cursor.segment(Utterance::anonymous("again")).unwrap();

        assert!(!cursor.is_queueing());
        assert_eq!(
            log.entries(),
            ["prepare:Insert", "deliver:hello", "deliver: there again"]
        );
    }

    #[test]
    fn queued_text_is_typed_when_focus_returns_before_the_press_ends() {
        let (log, mut cursor) = left_after_hello(OutputMode::InsertOrCopy);

        cursor.segment(Utterance::anonymous("there")).unwrap();
        cursor.delivery.come_back();
        cursor
            .completed(Utterance::anonymous("hello there"))
            .unwrap();

        assert_eq!(
            log.entries(),
            ["prepare:Insert", "deliver:hello", "deliver: there"]
        );
        assert_eq!(
            cursor.delivered().map(|delivered| delivered.outcome),
            Some(DeliveryOutcome::Inserted)
        );
    }

    #[test]
    fn text_queued_when_the_press_ends_is_copied_under_insert_or_copy() {
        let (log, mut cursor) = left_after_hello(OutputMode::InsertOrCopy);

        cursor.segment(Utterance::anonymous("over there")).unwrap();
        cursor
            .completed(Utterance::anonymous("hello over there"))
            .unwrap();

        assert_eq!(
            log.entries(),
            [
                "prepare:Insert",
                "deliver:hello",
                "prepare:Copy",
                "deliver:over there"
            ]
        );
        assert_eq!(
            cursor.delivered(),
            Some(Delivered {
                outcome: DeliveryOutcome::Copied,
                words: Some(2)
            })
        );
    }

    #[test]
    fn plain_insertion_reports_text_queued_at_the_end_as_kept_not_copied() {
        for (history, outcome) in [
            (true, DeliveryOutcome::InHistory),
            (false, DeliveryOutcome::NotTyped),
        ] {
            let log = CallLog::default();
            let mut cursor = Cursor::new(wandering(&log));
            let mut settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
            settings.history_enabled = history;
            cursor
                .dictation_started(&settings, &ExecutionControl::for_test(StopMode::Manual))
                .unwrap();
            cursor.segment(Utterance::anonymous("hello")).unwrap();
            cursor.delivery.leave(|| DeliveryError::TargetChanged);

            cursor.segment(Utterance::anonymous("over there")).unwrap();
            cursor
                .completed(Utterance::anonymous("hello over there"))
                .unwrap();

            assert_eq!(log.entries(), ["prepare:Insert", "deliver:hello"]);
            assert_eq!(
                cursor.delivered(),
                Some(Delivered {
                    outcome,
                    words: Some(2)
                })
            );
        }
    }

    #[test]
    fn an_app_with_aria_off_in_front_queues_the_text() {
        let log = CallLog::default();
        let mut cursor = Cursor::new(wandering(&log));
        let mut settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
        settings.target.excluded = vec!["com.secret".into()];
        cursor
            .dictation_started(&settings, &ExecutionControl::for_test(StopMode::Manual))
            .unwrap();
        *cursor.delivery.front.lock().unwrap() = Some(aria_platform::delivery::Application {
            name: "Vault".into(),
            bundle_id: Some("com.secret".into()),
        });

        cursor.segment(Utterance::anonymous("hello")).unwrap();

        assert!(cursor.is_queueing());
        assert_eq!(log.entries(), ["prepare:Insert"]);
    }

    #[test]
    fn a_moved_caret_or_submission_ends_typing_instead_of_queueing() {
        let cases: [(OutputMode, fn() -> DeliveryError); 3] = [
            (OutputMode::Insert, || DeliveryError::CaretMoved),
            (OutputMode::Insert, || DeliveryError::TargetUnverified),
            (OutputMode::InsertAndSubmit, || DeliveryError::TargetChanged),
        ];
        for (mode, refusal) in cases {
            let (log, mut cursor) = left_after_hello(mode);
            cursor.delivery.leave(refusal);

            assert!(cursor.segment(Utterance::anonymous("there")).is_err());
            assert!(!cursor.is_queueing());
            cursor.delivery.come_back();
            cursor
                .completed(Utterance::anonymous("hello there"))
                .unwrap();

            assert_eq!(
                log.entries(),
                [&format!("prepare:{mode:?}"), "deliver:hello"]
            );
        }
    }

    #[test]
    fn auto_submit_follows_complete_or_streaming_delivery_once() {
        for mode in [DeliveryMode::Complete, DeliveryMode::Streaming] {
            let (log, mut cursor) = cursor();
            let control = ExecutionControl::for_test(StopMode::Manual);
            cursor
                .dictation_started(&settings(mode, OutputMode::InsertAndSubmit), &control)
                .unwrap();
            cursor.segment(Utterance::anonymous("hello")).unwrap();
            cursor.segment(Utterance::anonymous("there")).unwrap();
            assert!(!log.entries().iter().any(|entry| entry == "submit"));
            cursor
                .completed(Utterance::anonymous("hello there"))
                .unwrap();
            cursor
                .completed(Utterance::anonymous("hello there"))
                .unwrap();
            let expected = match mode {
                DeliveryMode::Complete => {
                    vec!["prepare:InsertAndSubmit", "deliver:hello there", "submit"]
                }
                DeliveryMode::Streaming => vec![
                    "prepare:InsertAndSubmit",
                    "deliver:hello",
                    "deliver: there",
                    "submit",
                ],
            };
            assert_eq!(log.entries(), expected);
        }
    }

    #[test]
    fn nothing_is_typed_when_the_overridden_app_is_not_in_front() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let settings = DictationSettings {
            target: aria_platform::delivery::TargetPolicy {
                overridden_for: Some("com.example.not-in-front".into()),
                excluded: Vec::new(),
            },
            ..settings(DeliveryMode::Complete, OutputMode::InsertAndSubmit)
        };

        let error = cursor
            .dictation_started(&settings, &control)
            .expect_err("another app is in front");

        assert!(matches!(error, DeliveryError::TargetChanged));
        assert!(log.entries().is_empty());
    }

    #[test]
    fn failed_streaming_delivery_prevents_submission_at_completion() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::InsertAndSubmit),
                &control,
            )
            .unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        cursor.delivery.fail = true;
        assert!(cursor.segment(Utterance::anonymous("there")).is_err());
        cursor
            .completed(Utterance::anonymous("hello there"))
            .unwrap();
        assert_eq!(
            log.entries(),
            ["prepare:InsertAndSubmit", "deliver:hello", "deliver: there"]
        );
    }

    #[test]
    fn failed_submission_is_not_retried_or_retyped() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::InsertAndSubmit),
                &control,
            )
            .unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        cursor.delivery.fail = true;
        assert!(cursor.completed(Utterance::anonymous("hello")).is_err());
        cursor.completed(Utterance::anonymous("hello")).unwrap();
        assert_eq!(
            log.entries(),
            ["prepare:InsertAndSubmit", "deliver:hello", "submit"]
        );
    }

    #[test]
    fn a_return_taken_mid_stream_is_pressed_after_the_last_text() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &control,
            )
            .unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        let active = ActiveDictation::new(Arc::clone(&control));
        assert_eq!(active.target_process(), Some(TYPED_PROCESS));

        assert!(active.take_return());
        cursor.segment(Utterance::anonymous("there")).unwrap();
        cursor
            .completed(Utterance::anonymous("hello there"))
            .unwrap();

        assert_eq!(
            log.entries(),
            [
                "prepare:Insert",
                "deliver:hello",
                "deliver: there",
                "return"
            ]
        );
        assert!(
            !control.take_return(),
            "a finished press lets Return through"
        );
        assert!(!control.return_dropped());
    }

    #[test]
    fn failed_delivery_lets_the_next_return_through() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &control,
            )
            .unwrap();
        assert!(control.take_return());
        cursor.delivery.fail = true;
        assert!(cursor.segment(Utterance::anonymous("hello")).is_err());

        assert!(!control.take_return(), "typing ended with the failure");
        let _ = cursor.completed(Utterance::anonymous("hello"));
        assert!(!log.entries().iter().any(|entry| entry == "return"));
    }

    #[test]
    fn a_taken_return_under_submission_presses_return_once() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Complete, OutputMode::InsertAndSubmit),
                &control,
            )
            .unwrap();

        assert!(control.take_return());
        cursor.completed(Utterance::anonymous("hello")).unwrap();

        assert_eq!(
            log.entries(),
            ["prepare:InsertAndSubmit", "deliver:hello", "submit"]
        );
        assert!(!control.return_dropped(), "submission pressed it");
    }

    #[test]
    fn a_return_taken_before_any_text_is_still_pressed() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &control,
            )
            .unwrap();

        assert!(control.take_return());
        // No speech: the press ends without completing.
        cursor.dictation_finished(Err(&DictationError::NoSpeech));

        assert_eq!(log.entries(), ["prepare:Insert", "return"]);
        assert!(!control.return_dropped());
    }

    #[test]
    fn a_return_is_dropped_when_text_was_left_behind() {
        let (log, mut cursor) = left_after_hello(OutputMode::Insert);
        let control = Arc::clone(&cursor.target.as_ref().unwrap().control);
        assert!(control.take_return());
        cursor.segment(Utterance::anonymous("there")).unwrap();

        cursor
            .completed(Utterance::anonymous("hello there"))
            .unwrap();

        assert!(
            !log.entries().iter().any(|entry| entry == "return"),
            "the focused field holds something else"
        );
        assert!(control.return_dropped(), "the person hears of it");
    }

    #[test]
    fn a_return_the_app_refuses_fails_nothing() {
        let (log, mut cursor) = left_after_hello(OutputMode::Insert);
        cursor.delivery.come_back();
        let control = Arc::clone(&cursor.target.as_ref().unwrap().control);
        assert!(control.take_return());
        *cursor.delivery.return_refusal.lock().unwrap() = Some(|| DeliveryError::TargetChanged);

        cursor.completed(Utterance::anonymous("hello")).unwrap();

        assert!(!log.entries().iter().any(|entry| entry == "return"));
    }

    #[test]
    fn a_failed_transcription_drops_its_return() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &control,
            )
            .unwrap();
        assert!(control.take_return());

        // The field holds the person's own text, which the Return would send
        // without the words they meant to add.
        cursor.dictation_finished(Err(&DictationError::DeliveryInterrupted));

        assert_eq!(log.entries(), ["prepare:Insert"]);
        assert!(control.return_dropped());
    }

    #[test]
    fn a_complete_press_that_typed_nothing_drops_its_return() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Complete, OutputMode::Insert),
                &control,
            )
            .unwrap();
        assert!(control.take_return());

        // No speech, and no field bound to show where the Return went.
        cursor.dictation_finished(Err(&DictationError::NoSpeech));

        assert_eq!(log.entries(), ["prepare:Insert"]);
    }

    #[test]
    fn a_cancelled_press_drops_its_return() {
        let (log, mut cursor) = cursor();
        let cancel = aria_kit::cancel::CancelToken::new();
        let control =
            ExecutionControl::for_test_with_cancellation(StopMode::Manual, cancel.clone());
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &control,
            )
            .unwrap();
        assert!(control.take_return());

        cancel.cancel();
        cursor.dictation_finished(Err(&DictationError::Cancelled));

        assert!(!log.entries().iter().any(|entry| entry == "return"));
    }

    #[test]
    fn with_return_sends_off_return_reaches_the_app() {
        let (_log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let off = DictationSettings {
            return_sends: false,
            ..settings(DeliveryMode::Streaming, OutputMode::Insert)
        };
        cursor.dictation_started(&off, &control).unwrap();

        assert!(!control.take_return());
    }

    #[test]
    fn a_press_that_only_copies_leaves_return_alone() {
        let (_log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Complete, OutputMode::Copy),
                &control,
            )
            .unwrap();

        assert!(!control.take_return());
    }

    #[test]
    fn clipboard_fallback_never_runs_submission() {
        struct ClipboardFallback;

        impl TextDelivery for ClipboardFallback {
            type Prepared = ();
            const DEFAULT_MODE: OutputMode = OutputMode::Copy;
            const OUTLIVES_PROCESS: bool = true;

            fn frontmost(&self) -> Option<aria_platform::delivery::Application> {
                None
            }

            fn prepare(
                &self,
                mode: OutputMode,
                _cancel: impl Fn() -> bool,
            ) -> Result<(), DeliveryError> {
                assert_eq!(mode, OutputMode::InsertOrCopy);
                Ok(())
            }

            fn press_return(
                &self,
                _prepared: &Self::Prepared,
                _should_cancel: impl Fn() -> bool,
            ) -> Result<(), DeliveryError> {
                // Names no target process, so it never takes a Return.
                Err(DeliveryError::TargetUnavailable)
            }

            fn deliver(
                &self,
                _prepared: &(),
                _text: &str,
                _cancel: impl Fn() -> bool,
            ) -> Result<Posted, DeliveryError> {
                Ok(Posted::Copied)
            }

            fn finish(
                &self,
                _prepared: &(),
                _cancel: impl Fn() -> bool,
            ) -> Result<Finished, DeliveryError> {
                panic!("clipboard fallback must not run submission");
            }
        }

        let mut cursor = Cursor::new(ClipboardFallback);
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Complete, OutputMode::InsertOrCopy),
                &control,
            )
            .unwrap();
        cursor.completed(Utterance::anonymous("hello")).unwrap();
        assert_eq!(
            cursor.delivered(),
            Some(Delivered {
                outcome: DeliveryOutcome::Copied,
                words: None
            })
        );
    }

    #[test]
    fn placement_is_reported_only_while_streaming() {
        let control = ExecutionControl::for_test(StopMode::Manual);
        for (mode, placed) in [
            (DeliveryMode::Streaming, true),
            (DeliveryMode::Complete, false),
        ] {
            let mut cursor = Cursor::new(PlacedDelivery);
            cursor
                .dictation_started(&settings(mode, OutputMode::Insert), &control)
                .unwrap();
            assert_eq!(cursor.placement().is_some(), placed);
            cursor.completed(Utterance::anonymous("hello")).unwrap();
            assert!(cursor.placement().is_none());
        }
    }

    #[test]
    fn streaming_inserts_without_the_clipboard_fallback() {
        assert_eq!(
            streaming_output_mode(OutputMode::Insert),
            Some(OutputMode::Insert)
        );
        assert_eq!(
            streaming_output_mode(OutputMode::InsertOrCopy),
            Some(OutputMode::Insert)
        );
        assert_eq!(
            streaming_output_mode(OutputMode::InsertAndSubmit),
            Some(OutputMode::Insert)
        );
        assert_eq!(streaming_output_mode(OutputMode::Copy), None);
    }

    #[test]
    fn a_transcript_only_press_prepares_and_types_nothing() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let mut settings = settings(DeliveryMode::Complete, OutputMode::Insert);
        settings.route = crate::dictation::TranscriptRoute::TranscriptOnly;

        cursor.dictation_started(&settings, &control).unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        cursor.completed(Utterance::anonymous("hello")).unwrap();
        assert_eq!(cursor.delivered(), None);
        cursor.dictation_finished(Ok(()));

        assert!(log.entries().is_empty());
    }

    #[test]
    fn streaming_types_each_segment_with_a_space_after_the_first() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);

        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::InsertOrCopy),
                &control,
            )
            .unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        cursor.segment(Utterance::anonymous("there")).unwrap();
        cursor
            .completed(Utterance::anonymous("hello there"))
            .unwrap();

        assert_eq!(
            log.entries(),
            ["prepare:Insert", "deliver:hello", "deliver: there"]
        );
    }

    #[test]
    fn complete_types_only_the_whole_transcript() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);

        cursor
            .dictation_started(
                &settings(DeliveryMode::Complete, OutputMode::InsertOrCopy),
                &control,
            )
            .unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        cursor
            .completed(Utterance::anonymous("hello there"))
            .unwrap();

        assert_eq!(
            cursor.delivered(),
            Some(Delivered {
                outcome: DeliveryOutcome::Inserted,
                words: None
            })
        );
        assert_eq!(
            log.entries(),
            ["prepare:InsertOrCopy", "deliver:hello there"]
        );
    }

    #[test]
    fn streaming_refuses_copy_mode_before_any_text() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);

        let started = cursor.dictation_started(
            &settings(DeliveryMode::Streaming, OutputMode::Copy),
            &control,
        );

        assert!(matches!(started, Err(DeliveryError::InsertionUnsupported)));
        assert!(log.entries().is_empty());
    }

    #[test]
    fn a_new_dictation_starts_without_a_space() {
        let (log, mut cursor) = cursor();
        let control = ExecutionControl::for_test(StopMode::Manual);
        let streaming = settings(DeliveryMode::Streaming, OutputMode::Insert);

        cursor.dictation_started(&streaming, &control).unwrap();
        cursor.segment(Utterance::anonymous("one")).unwrap();
        cursor.dictation_finished(Ok(()));
        cursor.dictation_started(&streaming, &control).unwrap();
        cursor.segment(Utterance::anonymous("two")).unwrap();

        assert_eq!(
            log.entries(),
            [
                "prepare:Insert",
                "deliver:one",
                "prepare:Insert",
                "deliver:two"
            ]
        );
    }

    /// A field that reports `caret`, in the application `front`.
    struct FieldWithText {
        delivery: RecordingDelivery,
        caret: Caret,
        front: Option<aria_platform::delivery::Application>,
    }

    impl TextDelivery for FieldWithText {
        type Prepared = OutputMode;
        const DEFAULT_MODE: OutputMode = OutputMode::Insert;

        const OUTLIVES_PROCESS: bool = true;

        fn frontmost(&self) -> Option<aria_platform::delivery::Application> {
            self.front.clone()
        }

        fn prepare(
            &self,
            mode: OutputMode,
            should_cancel: impl Fn() -> bool,
        ) -> Result<OutputMode, DeliveryError> {
            self.delivery.prepare(mode, should_cancel)
        }

        fn finish(
            &self,
            prepared: &OutputMode,
            should_cancel: impl Fn() -> bool,
        ) -> Result<Finished, DeliveryError> {
            self.delivery.finish(prepared, should_cancel)
        }

        fn press_return(
            &self,
            _prepared: &Self::Prepared,
            _should_cancel: impl Fn() -> bool,
        ) -> Result<(), DeliveryError> {
            // Names no target process, so it never takes a Return.
            Err(DeliveryError::TargetUnavailable)
        }

        fn deliver(
            &self,
            prepared: &OutputMode,
            text: &str,
            should_cancel: impl Fn() -> bool,
        ) -> Result<Posted, DeliveryError> {
            self.delivery.deliver(prepared, text, should_cancel)
        }

        fn caret(&self, _prepared: &OutputMode) -> Caret {
            self.caret
        }
    }

    fn streamed_into<'a>(
        before: &str,
        after: &str,
        utterances: impl IntoIterator<Item = Utterance<'a>>,
    ) -> Vec<String> {
        let log = CallLog::default();
        let mut cursor = Cursor::new(FieldWithText {
            delivery: RecordingDelivery {
                log: log.clone(),
                fail: false,
            },
            caret: Caret::between(before, after),
            front: None,
        });
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &control,
            )
            .unwrap();
        for utterance in utterances {
            cursor.segment(utterance).unwrap();
        }
        log.entries()[1..].to_vec()
    }

    #[test]
    fn nothing_is_typed_into_an_app_with_aria_off() {
        let log = CallLog::default();
        let mut cursor = Cursor::new(FieldWithText {
            delivery: RecordingDelivery {
                log: log.clone(),
                fail: false,
            },
            caret: Caret::between("", ""),
            front: Some(aria_platform::delivery::Application {
                name: "Vault".into(),
                bundle_id: Some("com.secret".into()),
            }),
        });
        let mut settings = settings(DeliveryMode::Streaming, OutputMode::Insert);
        settings.target.excluded = vec!["com.secret".into()];

        let started =
            cursor.dictation_started(&settings, &ExecutionControl::for_test(StopMode::Manual));

        assert!(matches!(started, Err(DeliveryError::ApplicationExcluded)));
        assert!(log.entries().is_empty());
    }

    #[test]
    fn text_after_a_sentence_starts_with_a_space() {
        assert_eq!(
            streamed_into("Hello.", "", ["How are you"].map(Utterance::anonymous)),
            ["deliver: How are you"]
        );
    }

    #[test]
    fn later_segments_are_spaced_by_what_was_typed() {
        assert_eq!(
            streamed_into("(", "", ["hello", "there"].map(Utterance::anonymous)),
            ["deliver:hello", "deliver: there"]
        );
        assert_eq!(
            streamed_into("", "", ["你好", "世界"].map(Utterance::anonymous)),
            ["deliver:你好", "deliver:世界"]
        );
    }

    #[test]
    fn a_word_after_the_caret_keeps_its_distance() {
        assert_eq!(
            streamed_into("", "w", ["hello", "there"].map(Utterance::anonymous)),
            ["deliver:hello ", "deliver:there "]
        );
    }

    #[test]
    fn a_deferred_period_attaches_before_the_text_after_the_caret() {
        let deferring = |text| Utterance {
            spacing: Spacing {
                defers_ending: true,
                ..Spacing::default()
            },
            ..Utterance::anonymous(text)
        };
        let utterances = [
            deferring("we should"),
            deferring(". Ship it"),
            Utterance::anonymous("."),
        ];

        assert_eq!(
            streamed_into("foo", "bar", utterances),
            ["deliver: we should", "deliver:. Ship it", "deliver:. "]
        );
    }
}
