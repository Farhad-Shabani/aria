use std::sync::Arc;

use crate::delivery::{Caret, DeliveryError, DeliveryOutcome, OutputMode, Placement, TextDelivery};

use super::{DeliveryMode, DictationSettings, ExecutionControl, TranscriptSink, Utterance};

/// The frontmost app as a transcript sink. Streaming types each segment as
/// it arrives; complete types the whole transcript once. A transcript-only
/// press binds no target and types nothing.
pub(crate) struct Cursor<D: TextDelivery> {
    delivery: D,
    target: Option<Target<D::Prepared>>,
}

/// Where the dictation under way lands, picked before any text.
struct Target<P> {
    mode: DeliveryMode,
    control: Arc<ExecutionControl>,
    prepared: P,
    /// Read from the field before the first segment, then moved past each
    /// segment typed.
    caret: Option<Caret>,
    finished: bool,
    delivered: Option<DeliveryOutcome>,
}

impl<D: TextDelivery> Cursor<D> {
    pub(crate) fn new(delivery: D) -> Self {
        Self {
            delivery,
            target: None,
        }
    }

    /// Types `text` where the press landed, or nowhere when the press
    /// asked only for its transcript.
    fn deliver(&mut self, mode: DeliveryMode, text: &str) -> Result<(), DeliveryError> {
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        if target.finished || target.mode != mode {
            return Ok(());
        }
        let spaced = target
            .caret
            .get_or_insert_with(|| self.delivery.caret(&target.prepared))
            .insert(text);
        let outcome =
            match self
                .delivery
                .deliver(&target.prepared, &spaced, should_cancel(&target.control))
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    target.finished = true;
                    return Err(error);
                }
            };
        target.delivered = Some(outcome);
        Ok(())
    }
}

impl<D: TextDelivery> TranscriptSink for Cursor<D> {
    type Error = DeliveryError;

    fn dictation_started(
        &mut self,
        settings: &DictationSettings,
        control: &Arc<ExecutionControl>,
    ) -> Result<(), DeliveryError> {
        if settings.transcript_only {
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
        let prepared = self.delivery.prepare(output, should_cancel(control))?;
        self.target = Some(Target {
            mode: settings.delivery_mode,
            control: Arc::clone(control),
            prepared,
            caret: None,
            finished: false,
            delivered: None,
        });
        Ok(())
    }

    fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), DeliveryError> {
        self.deliver(DeliveryMode::Streaming, utterance.text)
    }

    fn completed(&mut self, utterance: Utterance<'_>) -> Result<(), DeliveryError> {
        if self.target.as_ref().is_none_or(|target| target.finished) {
            return Ok(());
        }
        self.deliver(DeliveryMode::Complete, utterance.text)?;
        let Some(target) = self.target.as_mut() else {
            return Ok(());
        };
        target.finished = true;
        if target.delivered == Some(DeliveryOutcome::Inserted) {
            self.delivery
                .finish(&target.prepared, should_cancel(&target.control))?;
        }
        Ok(())
    }

    fn delivered(&self) -> Option<DeliveryOutcome> {
        self.target.as_ref()?.delivered
    }

    fn placement(&self) -> Option<Placement> {
        let target = self.target.as_ref()?;
        if target.finished || target.mode != DeliveryMode::Streaming {
            return None;
        }
        self.delivery.placement(&target.prepared)
    }

    fn dictation_finished(&mut self) {
        self.target = None;
    }
}

fn should_cancel(control: &ExecutionControl) -> impl Fn() -> bool + '_ {
    || control.should_stop()
}

// Streaming inserts each segment as it finalizes, so no whole transcript is
// left to fall back with: one failed segment would strand a fragment on the
// clipboard. `InsertOrCopy` therefore streams as plain insertion.
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
    use crate::dictation::testing::{CallLog, PlacedDelivery, RecordingDelivery, settings};

    use super::*;

    fn cursor() -> (CallLog, Cursor<RecordingDelivery>) {
        let log = CallLog::default();
        let cursor = log.cursor();
        (log, cursor)
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
    fn clipboard_fallback_never_runs_submission() {
        struct ClipboardFallback;

        impl TextDelivery for ClipboardFallback {
            type Prepared = ();
            const DEFAULT_MODE: OutputMode = OutputMode::Copy;
            const OUTLIVES_PROCESS: bool = true;

            fn prepare(
                &self,
                mode: OutputMode,
                _cancel: impl Fn() -> bool,
            ) -> Result<(), DeliveryError> {
                assert_eq!(mode, OutputMode::InsertOrCopy);
                Ok(())
            }

            fn deliver(
                &self,
                _prepared: &(),
                _text: &str,
                _cancel: impl Fn() -> bool,
            ) -> Result<DeliveryOutcome, DeliveryError> {
                Ok(DeliveryOutcome::Copied)
            }

            fn finish(
                &self,
                _prepared: &(),
                _cancel: impl Fn() -> bool,
            ) -> Result<(), DeliveryError> {
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
        assert_eq!(cursor.delivered(), Some(DeliveryOutcome::Copied));
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
        settings.transcript_only = true;

        cursor.dictation_started(&settings, &control).unwrap();
        cursor.segment(Utterance::anonymous("hello")).unwrap();
        cursor.completed(Utterance::anonymous("hello")).unwrap();
        assert_eq!(cursor.delivered(), None);
        cursor.dictation_finished();

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

        assert_eq!(cursor.delivered(), Some(DeliveryOutcome::Inserted));
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
        cursor.dictation_finished();
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

    /// A field that reports `caret`.
    struct FieldWithText {
        delivery: RecordingDelivery,
        caret: Caret,
    }

    impl TextDelivery for FieldWithText {
        type Prepared = OutputMode;

        const DEFAULT_MODE: OutputMode = OutputMode::Insert;
        const OUTLIVES_PROCESS: bool = true;

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
        ) -> Result<(), DeliveryError> {
            self.delivery.finish(prepared, should_cancel)
        }

        fn deliver(
            &self,
            prepared: &OutputMode,
            text: &str,
            should_cancel: impl Fn() -> bool,
        ) -> Result<DeliveryOutcome, DeliveryError> {
            self.delivery.deliver(prepared, text, should_cancel)
        }

        fn caret(&self, _prepared: &OutputMode) -> Caret {
            self.caret
        }
    }

    fn streamed_into(before: &str, after: &str, segments: &[&str]) -> Vec<String> {
        let log = CallLog::default();
        let mut cursor = Cursor::new(FieldWithText {
            delivery: RecordingDelivery {
                log: log.clone(),
                fail: false,
            },
            caret: Caret::between(before, after),
        });
        let control = ExecutionControl::for_test(StopMode::Manual);
        cursor
            .dictation_started(
                &settings(DeliveryMode::Streaming, OutputMode::Insert),
                &control,
            )
            .unwrap();
        for segment in segments {
            cursor.segment(Utterance::anonymous(segment)).unwrap();
        }
        log.entries()[1..].to_vec()
    }

    #[test]
    fn text_after_a_sentence_starts_with_a_space() {
        assert_eq!(
            streamed_into("Hello.", "", &["How are you"]),
            ["deliver: How are you"]
        );
    }

    #[test]
    fn later_segments_are_spaced_by_what_was_typed() {
        assert_eq!(
            streamed_into("(", "", &["hello", "there"]),
            ["deliver:hello", "deliver: there"]
        );
        assert_eq!(
            streamed_into("", "", &["你好", "世界"]),
            ["deliver:你好", "deliver:世界"]
        );
    }

    #[test]
    fn a_word_after_the_caret_keeps_its_distance() {
        assert_eq!(
            streamed_into("", "w", &["hello", "there"]),
            ["deliver:hello ", "deliver:there "]
        );
    }
}
