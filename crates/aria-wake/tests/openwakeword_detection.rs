#![cfg(not(target_family = "wasm"))]

use aria_kit::audio::canonical_fixture;
use aria_wake::Error;
use aria_wake::openwakeword::{Config, OpenWakeWord};

mod support;

use support::{fires, model};

/// openWakeWord's Python runtime scores this on the canonical fixture.
const FIXTURE_REFERENCE: f32 = 0.000772;

fn detector_at(classifier: &str, threshold: f32) -> Result<OpenWakeWord, Error> {
    let config = Config::new(model(classifier), threshold).expect("threshold is valid");
    OpenWakeWord::load_from(&config)
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

#[test]
#[ignore = "requires the wake-word classifiers in models/wakeword"]
fn the_canonical_fixture_scores_what_openwakeword_scores() {
    let audio = fixture_samples();
    let mut below = detector_at("aria.onnx", FIXTURE_REFERENCE * 0.99)
        .expect("the openWakeWord head should load");
    let mut above = detector_at("aria.onnx", FIXTURE_REFERENCE * 1.01)
        .expect("the openWakeWord head should load");

    assert!(fires(&mut below, &audio), "peak score fell below reference");
    assert!(
        !fires(&mut above, &audio),
        "peak score rose above reference"
    );
}

#[test]
#[ignore = "requires the wake-word classifiers in models/wakeword"]
fn the_livekit_head_is_refused() {
    let Err(error) = detector_at("hey_livekit.onnx", 0.5) else {
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
