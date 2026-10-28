//! The listen stack replayed offline: a WAV heard in realtime through
//! streaming transcription and the speaker labeler.

use std::path::Path;
use std::time::Duration;

use aria::speaker::{self, Diarizer, Label, SegmentLabeler, SegmentReader, Verdict};
use aria::transcription::{Language, Streaming, Transcriber, Transcription, TranscriptionError};
use aria_kit::audio::{AudioCapture, RecordingMode, ReplayPace, StopSignal, WavReplayCapture};
use aria_kit::speaker::{Engine, ProfileStore};
use aria_speaker::wespeaker::{self, Config, WeSpeaker};

use crate::diarize::BenchError;

/// One segment the gate kept, as the labeler judged it.
pub(crate) struct Segment {
    pub(crate) text: String,
    /// The live, tentative label; `None` without a usable embedding.
    pub(crate) label: Option<Label>,
    pub(crate) embedding: Option<Vec<f32>>,
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

/// The reader this bench measures, with its engine resolved.
pub(crate) type Labeler = SegmentReader<WeSpeaker>;

/// A fresh per-scenario labeler; the speaker model is mandatory here — an
/// unlabeled bench measures nothing.
pub(crate) fn labeler(speaker: &Config, speakers_dir: &Path) -> Result<Labeler, BenchError> {
    let embedder = WeSpeaker::load_from(Config {
        model: speaker.model.clone(),
    })
    .map_err(BenchError::SpeakerModel)?;
    Ok(SegmentReader::new(SegmentLabeler::new(
        speaker::speech_gate(),
        Some(Diarizer::new(
            embedder,
            speaker::usable_profiles(
                ProfileStore::open(speakers_dir)
                    .read_for(wespeaker::NAME, wespeaker::EMBEDDING_DIM),
            ),
        )),
    )))
}

/// The listen session's loop, replayed from a WAV.
pub(crate) fn hear<T: Transcriber>(
    transcription: &Transcription<T, Streaming>,
    wav: &Path,
    spool_dir: &Path,
    labeler: &mut Labeler,
    scenario: &'static str,
    pace: ReplayPace,
    longest: Duration,
) -> Result<(Vec<Segment>, Dropped), BenchError> {
    let replay = |source| BenchError::Replay { scenario, source };
    let mut recording = WavReplayCapture::new(spool_dir, longest, wav, pace)
        .map_err(replay)?
        .start(RecordingMode::UntilStopped, StopSignal::never())
        .map_err(replay)?;

    let mut operation = transcription
        .start(&Language::Automatic)
        .map_err(|source| BenchError::Transcription { scenario, source })?;
    let mut segments = Vec::new();
    let mut dropped = Dropped::default();
    loop {
        let heard = match labeler.next(&mut operation, &mut recording) {
            Ok(Some(heard)) => heard,
            Ok(None) | Err(TranscriptionError::Capture(_)) => break,
            Err(source) => return Err(BenchError::Transcription { scenario, source }),
        };
        match heard.verdict {
            Verdict::Drop => {
                dropped.segments += 1;
                dropped.samples += heard
                    .span
                    .as_ref()
                    .map_or(0, |span| span.end.saturating_sub(span.start));
            }
            // A segment the engine did not time cannot be placed on a
            // timeline, so it is unusable here rather than mis-scored.
            Verdict::Keep { label, embedding } => match heard.span {
                Some(span) => segments.push(Segment {
                    text: heard.segment.as_str().to_owned(),
                    label,
                    embedding,
                    span,
                }),
                None => dropped.segments += 1,
            },
        }
    }
    Ok((segments, dropped))
}
