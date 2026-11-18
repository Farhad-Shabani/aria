//! The system transcription stack replayed offline: a WAV heard through
//! streaming transcription, the speech map, the speaker tracker and the gate a
//! system transcription keeps its words by. Settled labels come afterwards,
//! from the voice track, as they do in system transcription.

use std::path::Path;
use std::time::Duration;

use aria_kit::speaker::{Admission, Decision, Gate, Timeline, TurnFeed};
use aria_kit::transcription::next_heard;
use aria_kit::voice_activity::{SileroVad, SpeechDetector};
use aria_speaker::wespeaker::WeSpeaker;
use aria_voice::speaker::{Diarizer, Label, VoiceprintTracker};
use aria_voice::stt::{
    Language, Recognizer, Stream, Streaming, StreamingEngine, Transcription, TranscriptionError,
};

use crate::diarize::BenchError;
use crate::replay::{Step, Stepped};

/// Streaming transcription whose finished decodes release the replay's
/// strides.
pub(crate) struct SteppedTranscription<T> {
    transcription: Transcription<Stepped<T>, Streaming>,
    step: Step,
    stride: usize,
}

impl<T: StreamingEngine> SteppedTranscription<T> {
    pub(crate) fn new(engine: T) -> Result<Self, BenchError> {
        let stride = stride(T::STREAM).ok_or(BenchError::NativeStream)?;
        let step = Step::default();
        Ok(Self {
            transcription: Transcription::stream(T::STREAM, Stepped::new(engine, step.clone())),
            step,
            stride,
        })
    }
}

/// The stride a stepped replay releases per decode. `None` for an engine that
/// streams natively: its chunk is known only once a stream opens, and a
/// stride shorter than it stalls the replay.
fn stride(stream: Stream) -> Option<usize> {
    match stream {
        Stream::Agreement(cadence) => Some(cadence.stride_samples()),
        Stream::Native => None,
    }
}

/// One segment the gate kept.
pub(crate) struct Segment {
    pub(crate) text: String,
    /// Where the segment's audio sits, in samples.
    pub(crate) span: std::ops::Range<u64>,
    /// The label the tracker gave it as it was released.
    pub(crate) live: Option<Label>,
    embedding: Option<Vec<f32>>,
}

/// Close to what a system transcription without the voice track ends with: the
/// live embeddings grouped again over the whole recording. Relabels each
/// segment by its own embedding, where system transcription relabels each paragraph by the
/// average of its segments' embeddings.
pub(crate) fn relabeled(
    diarizer: &Diarizer<WeSpeaker>,
    segments: &[Segment],
) -> Vec<Option<Label>> {
    let embeddings: Vec<Option<Vec<f32>>> = segments
        .iter()
        .map(|segment| segment.embedding.clone())
        .collect();
    let mut labels = diarizer.relabel(&embeddings);
    crate::diarize::settle_gaps(&mut labels, segments);
    labels
}

/// What the speech gate threw away. The seconds matter more than the count:
/// they are the whole budget a coverage fix could win back, and a scored
/// transcript never sees them.
#[derive(Default)]
pub(crate) struct Dropped {
    pub(crate) segments: usize,
    pub(crate) samples: u64,
}

/// The system transcription's loop, replayed from a WAV. With a `diarizer`, a
/// tracker labels voices live, as system transcription's does.
pub(crate) fn hear<T: Recognizer>(
    stepped: &SteppedTranscription<T>,
    wav: &Path,
    scenario: &'static str,
    longest: Duration,
    diarizer: Option<&mut Diarizer<WeSpeaker>>,
) -> Result<(Vec<Segment>, Dropped), BenchError> {
    let (mut recording, _armed) = stepped
        .step
        .replay(wav, longest, stepped.stride)
        .map_err(|source| BenchError::Replay { scenario, source })?;
    let transcription = &stepped.transcription;
    let transcribing = |source| BenchError::Transcription { scenario, source };
    let from = recording.held_from();
    let vad = SileroVad::load().map_err(BenchError::SpeechGate)?;
    let speech = SpeechDetector::new(vad, from);
    let tracker = diarizer.map(|diarizer| VoiceprintTracker::numbering(diarizer, from));
    let mut feed = TurnFeed::start(&mut recording, speech, tracker);
    let untracked = Timeline::default();
    let mut gate = Gate::new(Admission::Spoken, feed.held_from());
    let mut operation = transcription
        .start(&Language::Automatic)
        .map_err(transcribing)?;
    let mut kept = Kept::default();
    loop {
        let heard = match next_heard(&mut operation, &mut recording) {
            Ok(Some(heard)) => heard,
            Ok(None) | Err(TranscriptionError::Capture(_)) => break,
            Err(source) => return Err(transcribing(source)),
        };
        feed.hear_through(operation.read_to());
        gate.hold(heard);
        kept.take(gate.decide(feed.timeline().unwrap_or(&untracked), Some(feed.speech())));
    }
    feed.hear_rest();
    kept.take(gate.decide(feed.timeline().unwrap_or(&untracked), Some(feed.speech())));
    if feed.timeline().is_some_and(Timeline::is_broken) {
        return Err(BenchError::Tracking { scenario });
    }
    Ok((kept.segments, kept.dropped))
}

#[derive(Default)]
struct Kept {
    segments: Vec<Segment>,
    dropped: Dropped,
}

impl Kept {
    fn take(&mut self, decision: Decision) {
        for heard in decision.dropped {
            self.dropped.segments += 1;
            self.dropped.samples += heard
                .span
                .as_ref()
                .map_or(0, |span| span.end.saturating_sub(span.start));
        }
        for released in decision.released {
            let heard = released.heard;
            // A segment the engine did not time cannot be placed on a
            // timeline, so it is unusable here rather than mis-scored.
            match heard.span {
                Some(span) => self.segments.push(Segment {
                    text: heard.segment.as_str().to_owned(),
                    span,
                    live: released.speaker,
                    embedding: released.embedding,
                }),
                None => self.dropped.segments += 1,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use aria_voice::stt::Cadence;

    use super::*;

    #[test]
    fn only_an_agreement_stream_has_a_stride_to_step() {
        let cadence = Cadence::from_millis(500, 20_000);
        assert_eq!(
            stride(Stream::Agreement(cadence)),
            Some(cadence.stride_samples())
        );
        assert_eq!(stride(Stream::Native), None);
    }
}
