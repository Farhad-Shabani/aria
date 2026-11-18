#![cfg(not(target_family = "wasm"))]

use std::path::Path;

use aria_wake::livekit::{Config, LiveKitWakeWord};

use aria_wake::Error;

mod support;

use support::{fires, model};

const THRESHOLD: f32 = 0.5;

/// What livekit's own Python runtime scores on `negative.wav`.
const NEGATIVE_REFERENCE: f32 = 0.0042;

fn detector_at(classifier: &str, threshold: f32) -> Result<LiveKitWakeWord, Error> {
    let config = Config::new(model(classifier), threshold).expect("threshold is valid");
    LiveKitWakeWord::load_from(&config)
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
#[ignore = "requires the wake-word classifiers in models/wakeword"]
fn the_negative_fixture_scores_what_livekit_scores() {
    let audio = negative_samples();
    let mut below = detector_at("hey_livekit.onnx", NEGATIVE_REFERENCE * 0.99)
        .expect("the livekit head should load");
    let mut above = detector_at("hey_livekit.onnx", NEGATIVE_REFERENCE * 1.01)
        .expect("the livekit head should load");

    assert!(fires(&mut below, &audio), "peak score fell below reference");
    assert!(
        !fires(&mut above, &audio),
        "peak score rose above reference"
    );
}

#[test]
#[ignore = "requires the wake-word classifiers in models/wakeword"]
fn the_openwakeword_head_is_refused() {
    let Err(error) = detector_at("aria.onnx", THRESHOLD) else {
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
#[ignore = "requires the wake-word classifiers in models/wakeword"]
fn silence_does_not_fire() {
    let mut detector =
        detector_at("hey_livekit.onnx", THRESHOLD).expect("the livekit head should load");

    assert!(!fires(&mut detector, &vec![0; 48_000]));
}
