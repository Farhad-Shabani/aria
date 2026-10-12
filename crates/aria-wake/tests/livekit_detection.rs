#![cfg(not(target_family = "wasm"))]

use std::path::{Path, PathBuf};

use aria_wake::livekit::{Config, LiveKitWakeWord};

use aria_kit::wake_word::{WakeWordDetector, WakeWordMatch};
use aria_wake::Error;

const THRESHOLD: f32 = 0.5;

/// What livekit's own Python runtime scores on `negative.wav`.
const NEGATIVE_REFERENCE: f32 = 0.0042;

fn models(classifier: &str) -> Option<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/wakeword");
    directory.join(classifier).is_file().then_some(directory)
}

fn detector(classifier: &str) -> Option<Result<LiveKitWakeWord, Error>> {
    detector_at(classifier, THRESHOLD)
}

fn detector_at(classifier: &str, threshold: f32) -> Option<Result<LiveKitWakeWord, Error>> {
    let config =
        Config::new(models(classifier)?.join(classifier), threshold).expect("threshold is valid");
    Some(LiveKitWakeWord::load_from(&config))
}

fn fires(detector: &mut LiveKitWakeWord, audio: &[i16]) -> bool {
    audio
        .chunks(1_280)
        .map(|chunk| detector.detect(chunk).expect("detection should succeed"))
        .any(|matched| matched == WakeWordMatch::Detected)
}

fn negative_samples() -> Vec<i16> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/negative.wav");
    let mut reader = hound::WavReader::open(path).expect("the negative fixture should open");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16_000, "the fixture must be canonical");
    assert_eq!(spec.channels, 1, "the fixture must be mono");
    reader
        .samples::<i16>()
        .map(|sample| sample.expect("the fixture should decode"))
        .collect()
}

#[test]
fn the_negative_fixture_scores_what_livekit_scores() {
    let audio = negative_samples();
    let (Some(below), Some(above)) = (
        detector_at("hey_livekit.onnx", NEGATIVE_REFERENCE * 0.99),
        detector_at("hey_livekit.onnx", NEGATIVE_REFERENCE * 1.01),
    ) else {
        return;
    };

    let mut below = below.expect("the livekit head should load");
    let mut above = above.expect("the livekit head should load");

    assert!(fires(&mut below, &audio), "peak score fell below reference");
    assert!(
        !fires(&mut above, &audio),
        "peak score rose above reference"
    );
}

#[test]
fn the_openwakeword_head_is_refused() {
    let Some(loaded) = detector("aria.onnx") else {
        return;
    };

    let Err(error) = loaded else {
        panic!("aria.onnx is an openWakeWord head and must not load");
    };

    assert!(
        matches!(
            &error,
            Error::Incompatible { role: "classifier", detail } if detail.contains("embeddings")
        ),
        "expected a classifier signature mismatch, got {error}"
    );
}

#[test]
fn silence_does_not_fire() {
    let Some(detector) = detector("hey_livekit.onnx") else {
        return;
    };
    let mut detector = detector.expect("the livekit head should load");

    assert!(!fires(&mut detector, &vec![0; 48_000]));
}
