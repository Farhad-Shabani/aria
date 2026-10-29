//! The listen stack replayed offline: a WAV heard in realtime through
//! streaming transcription, the speech map, the speaker tracker and the gate a
//! listen session keeps its words by. Settled labels come afterwards, from
//! the voice track, as they do in listen.

use std::path::Path;
use std::time::Duration;

use aria::speaker::{Diarizer, Label, VoiceprintTracker};
use aria::transcription::{Language, Streaming, Transcriber, Transcription, TranscriptionError};
use aria_kit::audio::{AudioCapture, RecordingMode, ReplayPace, StopSignal, WavReplayCapture};
use aria_kit::speaker::{Admission, Decision, Gate, Timeline, TurnFeed};
use aria_kit::transcription::next_heard;
use aria_kit::voice_activity::{SileroVad, SpeechDetector};
use aria_speaker::wespeaker::WeSpeaker;

use crate::diarize::BenchError;

/// One segment the gate kept.
pub(crate) struct Segment {
    pub(crate) text: String,
    /// Where the segment's audio sits, in samples.
    pub(crate) span: std::ops::Range<u64>,
    /// The label the tracker gave it as it was released.
    pub(crate) live: Option<Label>,
    embedding: Option<Vec<f32>>,
}

/// Close to what a listen session without the voice track ends with: the
/// live embeddings grouped again over the whole recording. Relabels each
/// segment by its own embedding, where listen relabels each paragraph by the
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

/// The listen session's loop, replayed from a WAV. With a `diarizer`, a
/// tracker labels voices live, as listen's does.
pub(crate) fn hear<T: Transcriber>(
    transcription: &Transcription<T, Streaming>,
    wav: &Path,
    spool_dir: &Path,
    scenario: &'static str,
    pace: ReplayPace,
    longest: Duration,
    diarizer: Option<&mut Diarizer<WeSpeaker>>,
) -> Result<(Vec<Segment>, Dropped), BenchError> {
    let replay = |source| BenchError::Replay { scenario, source };
    let transcribing = |source| BenchError::Transcription { scenario, source };
    let mut recording = WavReplayCapture::new(spool_dir, longest, wav, pace)
        .map_err(replay)?
        .start(RecordingMode::UntilStopped, StopSignal::never())
        .map_err(replay)?;
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
