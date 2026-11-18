use std::path::{Path, PathBuf};

use aria_kit::wake_word::{WakeWordDetector, WakeWordMatch};

pub(crate) fn model(classifier: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../models/wakeword")
        .join(classifier);
    assert!(path.is_file(), "models/wakeword/{classifier} is missing");
    path
}

pub(crate) fn fires(detector: &mut impl WakeWordDetector, audio: &[i16]) -> bool {
    audio
        .chunks(1_280)
        .map(|chunk| detector.detect(chunk).expect("detection should succeed"))
        .any(|matched| matched == WakeWordMatch::Detected)
}
