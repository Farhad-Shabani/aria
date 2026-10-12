#![cfg(not(target_family = "wasm"))]

use std::path::{Path, PathBuf};

use aria_kit::audio::canonical_fixture;
use aria_kit::wake_word::{WakeWordDetector, WakeWordMatch};
use aria_wake::Error;
use aria_wake::openwakeword::{Config, OpenWakeWord};

/// openWakeWord's Python runtime scores this on the canonical fixture.
const FIXTURE_REFERENCE: f32 = 0.000772;

fn models(classifier: &str) -> Option<PathBuf> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models/wakeword");
    directory.join(classifier).is_file().then_some(directory)
}

fn detector_at(classifier: &str, threshold: f32) -> Option<Result<OpenWakeWord, Error>> {
    let config =
        Config::new(models(classifier)?.join(classifier), threshold).expect("threshold is valid");
    Some(OpenWakeWord::load_from(&config))
}

fn fixture_samples() -> Vec<i16> {
    let directory = tempfile::TempDir::new().expect("test directory should be created");
    let mut reader =
        hound::WavReader::open(canonical_fixture(directory.path())).expect("fixture should open");
    reader
        .samples::<i16>()
        .map(|sample| sample.expect("the fixture should decode"))
        .collect()
}

fn fires(detector: &mut OpenWakeWord, audio: &[i16]) -> bool {
    audio
        .chunks(1_280)
        .map(|chunk| detector.detect(chunk).expect("detection should succeed"))
        .any(|matched| matched == WakeWordMatch::Detected)
}

#[test]
fn the_canonical_fixture_scores_what_openwakeword_scores() {
    let audio = fixture_samples();
    let (Some(below), Some(above)) = (
        detector_at("aria.onnx", FIXTURE_REFERENCE * 0.99),
        detector_at("aria.onnx", FIXTURE_REFERENCE * 1.01),
    ) else {
        return;
    };

    let mut below = below.expect("the openWakeWord head should load");
    let mut above = above.expect("the openWakeWord head should load");

    assert!(fires(&mut below, &audio), "peak score fell below reference");
    assert!(
        !fires(&mut above, &audio),
        "peak score rose above reference"
    );
}

#[test]
fn the_livekit_head_is_refused() {
    let Some(loaded) = detector_at("hey_livekit.onnx", 0.5) else {
        return;
    };

    let Err(error) = loaded else {
        panic!("hey_livekit.onnx is a livekit head and must not load");
    };

    assert!(
        matches!(
            &error,
            Error::Incompatible { role: "classifier", detail } if detail.contains("onnx::Flatten_0")
        ),
        "expected a classifier signature mismatch, got {error}"
    );
}
