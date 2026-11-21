//! How loud the microphone is, while a press records.

use std::sync::Arc;

use aria_kit::audio::RecordingReader;

use super::{DictationEventEmitter, ExecutionControl, RecordingId};
use crate::level::Meter;

/// One press worth of levels. Ends when capture does, or when the press is
/// cancelled.
pub(crate) fn meter(
    recording: &RecordingReader,
    control: &Arc<ExecutionControl>,
    events: &DictationEventEmitter,
) -> Meter {
    let id = control.recording_id();
    let control = Arc::clone(control);
    let events = events.clone();
    Meter::watching(
        recording,
        "aria-dictation-level",
        move || control.is_cancelled(),
        move |level| {
            events.emit(MicLevel {
                recording: id,
                level,
            });
        },
    )
}

/// How loud the microphone is right now, from silent to pinned.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MicLevel {
    pub recording: RecordingId,
    /// 0 at the noise floor, 1 at loud speech.
    pub level: f32,
}
