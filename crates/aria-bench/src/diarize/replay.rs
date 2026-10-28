//! The listen stack replayed offline: a WAV heard in realtime through
//! streaming transcription, the speech map and the gate a listen session
//! keeps its words by. Labels come afterwards, from the voice track, as they
//! do in listen.

use std::path::Path;
use std::time::Duration;

use aria::speaker::VoiceprintTracker;
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
}

/// What the speech gate threw away. The seconds matter more than the count:
/// they are the whole budget a coverage fix could win back, and a scored
/// transcript never sees them.
#[derive(Default)]
pub(crate) struct Dropped {
    pub(crate) segments: usize,
    pub(crate) samples: u64,
}

/// The listen session's loop, replayed from a WAV.
pub(crate) fn hear<T: Transcriber>(
    transcription: &Transcription<T, Streaming>,
    wav: &Path,
    spool_dir: &Path,
    scenario: &'static str,
    pace: ReplayPace,
    longest: Duration,
) -> Result<(Vec<Segment>, Dropped), BenchError> {
    let replay = |source| BenchError::Replay { scenario, source };
    let transcribing = |source| BenchError::Transcription { scenario, source };
    let mut recording = WavReplayCapture::new(spool_dir, longest, wav, pace)
        .map_err(replay)?
        .start(RecordingMode::UntilStopped, StopSignal::never())
        .map_err(replay)?;
    let vad = SileroVad::load().map_err(BenchError::SpeechGate)?;
    let speech = SpeechDetector::new(vad, recording.held_from());
    // No tracker runs: the score reads only the voice track's labels.
    let mut feed = TurnFeed::start(
        &mut recording,
        speech,
        None::<VoiceprintTracker<'_, WeSpeaker>>,
    );
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
        kept.take(gate.decide(&Timeline::default(), Some(feed.speech())));
    }
    feed.hear_rest();
    kept.take(gate.decide(&Timeline::default(), Some(feed.speech())));
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
                }),
                None => self.dropped.segments += 1,
            }
        }
    }
}
