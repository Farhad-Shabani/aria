use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::audio::Recording;
use crate::audio::testing::recorded;
use crate::transcription::{
    Cadence, ChunkStream, Emitted, Hypothesis, Language, RecognitionAudio, Recognizer, Session,
    Stream, TimedWord, Transcription,
};

struct Scripted(Mutex<VecDeque<Hypothesis>>);

impl Scripted {
    fn new(answers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(Mutex::new(
            answers.into_iter().map(Hypothesis::new).collect(),
        ))
    }
}

impl Recognizer for Scripted {
    fn validate_language(&self, _: &Language, _: &CancelToken) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn recognize_with(
        &self,
        _: RecognitionAudio<'_>,
        _: &Language,
        _: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        Ok(self
            .0
            .lock()
            .expect("script lock")
            .pop_front()
            .expect("script covers decodes"))
    }
}

struct Replace(&'static str, &'static str);

impl Layer for Replace {
    fn apply(&self, text: &mut String, _: &CancelToken) -> Result<(), TranscriptionError> {
        *text = text.replace(self.0, self.1);
        Ok(())
    }
}

#[test]
fn layers_apply_in_declaration_order_and_options_preserve_the_raw_path() {
    let (_directory, mut recording) = recorded(&[1, 2]);
    let transcription = Transcription::complete_samples(Scripted::new(["one"]))
        .with(Replace("one", "two"))
        .with(Replace("two", "three"));
    assert_eq!(
        transcription
            .transcribe(&mut recording, &Language::Automatic)
            .unwrap()
            .as_str(),
        "three"
    );

    for enabled in [false, true] {
        let (_directory, mut recording) = recorded(&[1, 2]);
        let transcription = Transcription::complete_samples(Scripted::new(["Um, hello."]))
            .with(enabled.then_some(RemoveHesitations));
        assert_eq!(
            transcription
                .transcribe(&mut recording, &Language::Automatic)
                .unwrap()
                .as_str(),
            if enabled { "Hello." } else { "Um, hello." }
        );
    }
}

#[test]
fn complete_and_streaming_sessions_apply_real_layers() {
    let vocabulary = Vocabulary::new(["Cargo.toml"]).unwrap();
    let (_directory, mut recording) = recorded(&[1, 2]);
    let transcription = Transcription::complete_samples(Scripted::new([
        "Um, open cargo dot toml. They had had enough.",
    ]))
    .with(RemoveHesitations)
    .with(&vocabulary);
    assert_eq!(
        transcription
            .transcribe(&mut recording, &Language::Automatic)
            .unwrap()
            .as_str(),
        "Open Cargo.toml. They had had enough."
    );

    let (_directory, recording) = recorded(&[1; 1600]);
    let transcription = Transcription::agreement(
        Cadence::from_millis(100, 20_000),
        Scripted::new(["Um, open cargo dot toml."]),
    )
    .with(RemoveHesitations)
    .with(vocabulary);
    let mut session = Session::over(&transcription, &Language::Automatic, recording).unwrap();
    let segment = session.next_finalized().unwrap().unwrap();
    assert_eq!(segment.as_str(), "Open Cargo.toml.");
    assert!(session.next_finalized().unwrap().is_none());
    assert_eq!(session.finish().unwrap().as_str(), segment.as_str());
}

struct WavScripted(&'static str);

impl Recognizer for WavScripted {
    fn validate_language(&self, _: &Language, _: &CancelToken) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn recognize_with(
        &self,
        audio: RecognitionAudio<'_>,
        _: &Language,
        _: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        let wav = audio.require_wav("scripted")?;
        assert!(wav.path().unwrap().is_file());
        Ok(Hypothesis::new(self.0))
    }
}

#[test]
fn whole_wav_layers_process_text_and_can_remove_all_speech() {
    for (input, expected) in [
        ("Um, open cargo dot toml.", Some("Open Cargo.toml.")),
        ("Um, uh.", None),
    ] {
        let (_directory, recording) = recorded(&[1, 2]);
        let transcription = Transcription::complete_wav(WavScripted(input))
            .with(RemoveHesitations)
            .with(Vocabulary::new(["Cargo.toml"]).unwrap());
        let session = Session::over(&transcription, &Language::Automatic, recording).unwrap();
        let result = session.finish();
        match expected {
            Some(text) => assert_eq!(result.unwrap().as_str(), text),
            None => assert!(matches!(result, Err(TranscriptionError::NoSpeech))),
        }
    }
}

struct NativeScripted;

impl Recognizer for NativeScripted {
    fn validate_language(&self, _: &Language, _: &CancelToken) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn recognize_with(
        &self,
        _: RecognitionAudio<'_>,
        _: &Language,
        _: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        panic!("native transcription must use the chunk stream")
    }

    fn open_stream(
        &self,
        _: &Language,
        _: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
        Ok(Some(Box::new(ScriptedStream(VecDeque::from([
            ("um car", false),
            ("go dot toml done", true),
            ("uh", false),
        ])))))
    }
}

struct ScriptedStream(VecDeque<(&'static str, bool)>);

impl ChunkStream for ScriptedStream {
    fn chunk_samples(&self) -> usize {
        1600
    }

    fn feed(&mut self, chunk: &[f32]) -> Result<Emitted, TranscriptionError> {
        assert_eq!(chunk.len(), self.chunk_samples());
        let (text, joins_previous) = self.0.pop_front().expect("script covers chunks and flush");
        Ok(Emitted {
            words: Hypothesis::new(text),
            joins_previous,
        })
    }
}

#[test]
fn native_layers_process_joined_words_and_flush_with_independent_opt_outs() {
    for (hesitations, vocabulary, expected) in [
        (false, false, vec!["um", "cargo dot toml", "done uh"]),
        (true, false, vec!["cargo dot toml", "done"]),
        (false, true, vec!["um", "Cargo.toml", "done uh"]),
        (true, true, vec!["Cargo.toml", "done"]),
    ] {
        for drain_tail in [false, true] {
            let (_directory, mut recording) = recorded(&[1; 3200]);
            let transcription = Transcription::stream(Stream::Native, NativeScripted)
                .with(hesitations.then_some(RemoveHesitations))
                .with(vocabulary.then(|| Vocabulary::new(["Cargo.toml"]).unwrap()));
            let mut operation = transcription.start(&Language::Automatic).unwrap();
            for text in expected
                .iter()
                .take(if drain_tail { expected.len() } else { 1 })
            {
                let segment = operation.next_finalized(&mut recording).unwrap().unwrap();
                assert_eq!(segment.as_str(), *text);
                recording.release_before(operation.retained_from());
            }
            if drain_tail {
                assert!(operation.next_finalized(&mut recording).unwrap().is_none());
                assert_eq!(operation.completed().unwrap().as_str(), expected.join(" "));
            }
            assert_eq!(
                operation.finish(&mut recording).unwrap().as_str(),
                expected.join(" ")
            );
        }
    }
}

#[test]
fn vocabulary_does_not_rewrite_across_finalized_segments() {
    let (mut recording, mut capture) = Recording::open();
    let transcription = Transcription::agreement(
        Cadence::from_millis(100, 20_000),
        Scripted::new(["cargo", "cargo dot toml"]),
    )
    .with(Vocabulary::new(["Cargo.toml"]).unwrap());
    let mut operation = transcription.start(&Language::Automatic).unwrap();
    capture.push(&[0.5; 1600]);
    operation.next_update(&mut recording).unwrap();
    capture.push(&[0.5; 1600]);
    capture.finish();
    recording.fill().unwrap();
    let first = operation.next_finalized(&mut recording).unwrap().unwrap();
    let tail = operation.next_finalized(&mut recording).unwrap().unwrap();
    assert_eq!(first.as_str(), "cargo");
    assert_eq!(tail.as_str(), "dot toml");
    assert_eq!(
        operation.finish(&mut recording).unwrap().as_str(),
        "cargo dot toml"
    );
}

#[test]
fn removing_every_word_finishes_without_redecoding() {
    let (_directory, mut recording) = recorded(&[1, 2]);
    let transcription =
        Transcription::complete_samples(Scripted::new(["Um, uh, hmm."])).with(RemoveHesitations);
    let mut operation = transcription.start(&Language::Automatic).unwrap();
    assert!(operation.next_finalized(&mut recording).unwrap().is_none());
    assert!(operation.completed().is_none());
    assert!(matches!(
        operation.finish(&mut recording),
        Err(TranscriptionError::NoSpeech)
    ));
}

#[test]
fn a_suppressed_streaming_segment_does_not_end_the_stream() {
    let (mut recording, mut capture) = Recording::open();
    let transcription = Transcription::agreement(
        Cadence::from_millis(100, 20_000),
        Scripted::new(["um", "um hello"]),
    )
    .with(RemoveHesitations);
    let mut operation = transcription.start(&Language::Automatic).unwrap();
    capture.push(&[0.5; 1600]);
    assert!(matches!(
        operation.next_update(&mut recording).unwrap(),
        Some(TranscriptUpdate::Provisional { text, .. }) if text == "um"
    ));
    capture.push(&[0.5; 1600]);
    capture.finish();
    recording.fill().unwrap();
    assert_eq!(
        operation
            .next_finalized(&mut recording)
            .unwrap()
            .unwrap()
            .as_str(),
        "hello"
    );
    assert_eq!(operation.completed().unwrap().as_str(), "hello");
    assert!(operation.next_finalized(&mut recording).unwrap().is_none());
}

#[test]
fn completion_without_a_final_segment_keeps_processed_text() {
    let (mut recording, mut capture) = Recording::open();
    let transcription = Transcription::agreement(
        Cadence::from_millis(100, 20_000),
        Scripted::new(["um hello", "um hello", "um hello"]),
    )
    .with(RemoveHesitations);
    let mut operation = transcription.start(&Language::Automatic).unwrap();
    capture.push(&[0.5; 1600]);
    operation.next_update(&mut recording).unwrap();
    capture.push(&[0.5; 1600]);
    assert_eq!(
        operation
            .next_finalized(&mut recording)
            .unwrap()
            .unwrap()
            .as_str(),
        "hello"
    );
    capture.finish();
    assert_eq!(operation.finish(&mut recording).unwrap().as_str(), "hello");
}

struct CountPreparations(AtomicUsize);

impl Layer for CountPreparations {
    fn apply(&self, _: &mut String, _: &CancelToken) -> Result<(), TranscriptionError> {
        Ok(())
    }

    fn prepare(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn preparing_a_stack_prepares_every_layer_in_it() {
    let inner = CountPreparations(AtomicUsize::new(0));
    let outer = CountPreparations(AtomicUsize::new(0));
    let absent: Option<CountPreparations> = None;
    Layered::new(Layered::new(&inner, absent), Some(&outer)).prepare();
    assert_eq!(inner.0.load(Ordering::Relaxed), 1);
    assert_eq!(outer.0.load(Ordering::Relaxed), 1);
}

struct CountApplications(AtomicUsize);

impl Layer for CountApplications {
    fn apply(&self, text: &mut String, _: &CancelToken) -> Result<(), TranscriptionError> {
        self.0.fetch_add(1, Ordering::Relaxed);
        text.push('!');
        Ok(())
    }
}

#[test]
fn completion_includes_the_processed_tail_without_reapplying_layers() {
    for drain_tail in [false, true] {
        let calls = CountApplications(AtomicUsize::new(0));
        let (mut recording, mut capture) = Recording::open();
        let transcription = Transcription::agreement(
            Cadence::from_millis(100, 20_000),
            Scripted::new(["one two", "one two three"]),
        )
        .with(&calls);
        let mut operation = transcription.start(&Language::Automatic).unwrap();
        capture.push(&[0.5; 1600]);
        assert!(matches!(
            operation.next_update(&mut recording).unwrap(),
            Some(TranscriptUpdate::Provisional { text, .. }) if text == "one two"
        ));
        capture.push(&[0.5; 1600]);
        capture.finish();
        recording.fill().unwrap();
        let first = operation.next_finalized(&mut recording).unwrap().unwrap();
        assert_eq!(first.as_str(), "one two!");
        assert_eq!(operation.completed().unwrap().as_str(), "one two! three!");
        if drain_tail {
            let tail = operation.next_finalized(&mut recording).unwrap().unwrap();
            assert_eq!(tail.as_str(), "three!");
            assert_eq!(
                [first.as_str(), tail.as_str()].join(" "),
                operation.completed().unwrap().as_str()
            );
        }
        assert_eq!(
            operation.finish(&mut recording).unwrap().as_str(),
            "one two! three!"
        );
        assert_eq!(calls.0.load(Ordering::Relaxed), 2);
    }
}

#[test]
fn processed_segments_keep_their_audio_spans_when_recording_is_released() {
    let timings = vec![
        TimedWord {
            start: 0.0,
            end: 0.025,
        },
        TimedWord {
            start: 0.025,
            end: 0.05,
        },
        TimedWord {
            start: 0.05,
            end: 0.075,
        },
    ];
    let scripted = Scripted(Mutex::new(VecDeque::from([
        Hypothesis::with_words("um one", timings[..2].to_vec()),
        Hypothesis::with_words("um one two", timings),
    ])));
    let (mut recording, mut capture) = Recording::open();
    let transcription = Transcription::agreement(Cadence::from_millis(100, 20_000), scripted)
        .with(RemoveHesitations);
    let mut operation = transcription.start(&Language::Automatic).unwrap();
    capture.push(&[0.5; 1600]);
    operation.next_update(&mut recording).unwrap();
    capture.push(&[0.5; 1600]);
    capture.finish();
    recording.fill().unwrap();
    let first = operation.next_finalized(&mut recording).unwrap().unwrap();
    assert_eq!(first.as_str(), "one");
    assert_eq!(first.span, Some(0..800));
    let retained = operation.retained_from();
    recording.release_before(retained);
    let tail = operation.next_finalized(&mut recording).unwrap().unwrap();
    assert_eq!(tail.as_str(), "two");
    assert_eq!(tail.span, Some(800..1200));
    let span = tail.span.unwrap();
    assert!(retained <= span.start);
    assert_eq!(recording.span(span).len(), 400);
}

struct StopOnTail(bool);

impl Layer for StopOnTail {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        if text == "three" {
            if self.0 {
                cancel.cancel();
            } else {
                return Err(TranscriptionError::Processing(Error::Failed(Box::new(
                    std::io::Error::other("rejected"),
                ))));
            }
        }
        Ok(())
    }
}

#[test]
fn cancellation_or_failure_in_the_tail_never_exposes_raw_completion() {
    for cancel_on_tail in [false, true] {
        let (mut recording, mut capture) = Recording::open();
        let transcription = Transcription::agreement(
            Cadence::from_millis(100, 20_000),
            Scripted::new(["one two", "one two three"]),
        )
        .with(StopOnTail(cancel_on_tail));
        let cancel = CancelToken::new();
        let mut operation = transcription
            .start_with(&Language::Automatic, &cancel)
            .unwrap();
        capture.push(&[0.5; 1600]);
        operation.next_update(&mut recording).unwrap();
        capture.push(&[0.5; 1600]);
        capture.finish();
        recording.fill().unwrap();
        let error = operation.next_finalized(&mut recording).unwrap_err();
        if cancel_on_tail {
            assert!(matches!(error, TranscriptionError::Cancelled));
        } else {
            assert!(matches!(
                error,
                TranscriptionError::Processing(Error::Failed(_))
            ));
        }
        assert!(operation.completed().is_none());
        assert!(operation.finish(&mut recording).is_err());
        assert!(operation.completed().is_none());
    }
}

#[test]
fn hesitations_preserve_sentence_punctuation() {
    for (input, expected) in [
        ("Well, um. Next.", "Well. Next."),
        ("Well, uh? Next.", "Well? Next."),
        ("Go. Um, so then", "Go. So then"),
    ] {
        let mut text = input.to_owned();
        RemoveHesitations
            .apply(&mut text, &CancelToken::never())
            .unwrap();
        assert_eq!(text, expected);
    }
}

#[test]
fn hesitations_preserve_delimiters_around_surviving_words() {
    for (input, expected) in [
        ("\"Um, hello.\"", "\"Hello.\""),
        ("(um hello)", "(hello)"),
        ("go (um uh now)", "go (now)"),
        ("go (um uh) now", "go now"),
        ("(Um) hello", "Hello"),
        ("(hello, um)", "(hello)"),
        ("[well; uh] next", "[well] next"),
        ("(um hello, uh)", "(hello)"),
        ("\"Hello. um\"", "\"Hello.\""),
    ] {
        let mut text = input.to_owned();
        RemoveHesitations
            .apply(&mut text, &CancelToken::never())
            .unwrap();
        assert_eq!(text, expected);
    }
}

fn applied(layer: impl Layer, input: &str) -> String {
    let mut text = input.to_owned();
    layer.apply(&mut text, &CancelToken::never()).unwrap();
    text
}

#[test]
fn hesitations_alone_leave_nothing_and_keep_a_newline() {
    assert_eq!(applied(RemoveHesitations, "Um."), "");
    assert_eq!(applied(RemoveHesitations, "uh uh"), "");
    assert_eq!(applied(RemoveHesitations, ""), "");
    assert_eq!(
        applied(RemoveHesitations, "report.\num, by friday"),
        "report.\nby friday"
    );
}

#[test]
fn repeats_drop_the_second_of_two_identical_words() {
    for (input, expected) in [
        ("The the report", "The report"),
        ("the The report", "the report"),
        ("so so so", "so"),
        ("the the, report", "the, report"),
        ("the (the report)", "the (report)"),
        ("ok (ok) go", "ok go"),
        ("", ""),
    ] {
        assert_eq!(applied(RemoveRepeats, input), expected, "{input:?}");
    }
}

#[test]
fn the_disfluency_corpus_cleans_as_expected() {
    let corpus = include_str!("../../../tests/fixtures/disfluencies.txt");
    for case in corpus.split("\n\n") {
        let mut lines = case.lines().filter(|line| !line.starts_with('#'));
        let Some(heard) = lines.next() else {
            continue;
        };
        let expected = lines.next().unwrap_or(heard);
        assert_eq!(
            applied(Layered::new(RemoveHesitations, RemoveRepeats), heard),
            expected,
            "heard {heard:?}"
        );
    }
}

#[test]
fn vocabulary_layer_rejects_personal_expansion_before_replacing_text() {
    let rules = vocabulary::PersonalRules::new(
        Vec::new(),
        vec![("x".to_owned(), "y".repeat(MAX_TEXT_BYTES + 1))],
    );
    let vocabulary = Vocabulary::compile(&[], rules).unwrap();
    let mut text = "x".to_owned();
    assert!(matches!(
        vocabulary.apply(&mut text, &CancelToken::never()),
        Err(TranscriptionError::Processing(Error::TooLarge))
    ));
    assert_eq!(text, "x");
}

struct Expand(usize);

impl Layer for Expand {
    fn apply(&self, text: &mut String, _: &CancelToken) -> Result<(), TranscriptionError> {
        *text = "x".repeat(self.0);
        Ok(())
    }
}

#[test]
fn text_bounds_cover_each_segment_before_and_after_a_layer() {
    for (input_bytes, output_bytes) in [
        (MAX_TEXT_BYTES, MAX_TEXT_BYTES),
        (MAX_TEXT_BYTES + 1, 1),
        (1, MAX_TEXT_BYTES + 1),
    ] {
        let (_directory, mut recording) = recorded(&[1, 2]);
        let transcription =
            Transcription::complete_samples(Scripted::new(["x".repeat(input_bytes)]))
                .with(Expand(output_bytes));
        let result = transcription.transcribe(&mut recording, &Language::Automatic);
        if input_bytes <= MAX_TEXT_BYTES && output_bytes <= MAX_TEXT_BYTES {
            assert_eq!(result.unwrap().as_str().len(), output_bytes);
        } else {
            assert!(matches!(
                result,
                Err(TranscriptionError::Processing(Error::TooLarge))
            ));
        }
    }
}

#[test]
fn a_session_accumulates_past_the_segment_bound() {
    let (mut recording, mut capture) = Recording::open();
    let transcription = Transcription::agreement(
        Cadence::from_millis(100, 20_000),
        Scripted::new(["one", "one two"]),
    )
    .with(Expand(MAX_TEXT_BYTES / 2));
    let mut operation = transcription.start(&Language::Automatic).unwrap();
    capture.push(&[0.5; 1600]);
    operation.next_update(&mut recording).unwrap();
    capture.push(&[0.5; 1600]);
    capture.finish();
    recording.fill().unwrap();
    while operation.next_finalized(&mut recording).unwrap().is_some() {}
    assert_eq!(
        operation.completed().unwrap().as_str().len(),
        MAX_TEXT_BYTES + 1
    );
}

mod correction_order {
    use rstest::rstest;

    use super::super::Vocabulary;
    use super::super::vocabulary::PersonalRules;

    fn corrected(heard: &str, terms: &[&str], learned: &[(&str, &str)]) -> String {
        let terms = terms.iter().map(|term| (*term).to_owned()).collect();
        let learned = learned
            .iter()
            .map(|(spoken, typed)| ((*spoken).to_owned(), (*typed).to_owned()))
            .collect();
        let vocabulary = Vocabulary::compile(&[], PersonalRules::new(terms, learned)).unwrap();
        vocabulary
            .correct_spoken(heard, "")
            .map_or_else(|| heard.to_owned(), |corrected| corrected.text)
    }

    #[rstest]
    #[case::spoken_form_before_a_number("i eighteen n", &["i18n"], &[], "i18n")]
    #[case::learned_pair_before_a_number("open twenty four seven", &[], &[("twenty four seven", "24/7")], "open 24/7")]
    #[case::number_beside_a_spoken_form("i eighteen n twenty one", &["i18n"], &[], "i18n 21")]
    #[case::symbol_joins_a_spoken_form("i eighteen n underscore key", &["i18n"], &[], "i18n_key")]
    #[case::symbol_joins_a_number("version twenty dot twenty one", &[], &[], "version 20.21")]
    #[case::form_inside_punctuation("(teh, twenty one)", &[], &[("teh", "the")], "(the, 21)")]
    #[case::near_miss_beside_a_number("kubernets twenty one", &["Kubernetes"], &[], "Kubernetes 21")]
    #[case::form_with_a_symbol_inside("open cargo dot toml", &[], &[("cargo dot toml", "Cargo.toml")], "open Cargo.toml")]
    #[case::form_opening_on_a_symbol_joins("example dot com", &[], &[("dot com", ".com")], "example.com")]
    #[case::form_opening_on_a_symbol_with_no_word_before("dot com is taken", &[], &[("dot com", ".com")], ".com is taken")]
    fn each_step_keeps_what_an_earlier_one_claimed(
        #[case] heard: &str,
        #[case] terms: &[&str],
        #[case] learned: &[(&str, &str)],
        #[case] typed: &str,
    ) {
        assert_eq!(corrected(heard, terms, learned), typed);
    }

    #[test]
    fn a_term_keeps_its_number_words() {
        let vocabulary = Vocabulary::new(["Area Fifty One"]).unwrap();
        assert_eq!(vocabulary.correct_spoken("area fifty one", ""), None);
        assert_eq!(
            Vocabulary::default()
                .correct_spoken("area fifty one", "")
                .map(|corrected| corrected.text)
                .as_deref(),
            Some("area 51")
        );
    }
}
