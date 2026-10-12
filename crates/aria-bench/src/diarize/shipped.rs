//! Scores the voice track listen actually runs, not a copy of it.
//!
//! [`label_as_shipped`] scores the track's own timeline. [`label_as_attributed`]
//! scores its attribution of independently chosen spans, as Listen does.
//! These exercise different paths and can expose different labeling errors.
//!
//! Neither exercises the live release floor: a finished recording is handed
//! over whole.

use std::ops::Range;
use std::path::Path;

use aria::speaker::{Grouping, VoiceTrack, Voices, voice_track};
use aria_kit::audio::RecordingReader;
use aria_speaker::wespeaker::{Config as SpeakerConfig, WeSpeaker};

use crate::diarize::journal::Journal;
use crate::diarize::{BenchError, Embedded, Labeled, SAMPLE_RATE, Turn};

/// One pass of the shipped track over one recording.
pub struct Run<'a> {
    pub segmentation: &'a Path,
    pub speaker: SpeakerConfig,
    pub wav: &'a Path,
    /// The rules being measured. Everything else here is fixed cost.
    pub grouping: Grouping,
    /// Where to keep what the nets heard. They answer the same whatever the
    /// grouping, so a sweep reads a recording once and varies the rest.
    pub cache: Option<&'a Path>,
    /// Where to write what the track believed after every chunk.
    pub journal: Option<&'a Path>,
}

/// Labels a recording through the shipped voice track, on the track's own
/// timeline: every stretch it heard a voice in.
pub fn label_as_shipped(run: Run<'_>) -> Result<Labeled, BenchError> {
    let voices = observe(run)?;
    let turns = voices
        .turns()
        .into_iter()
        .map(|spoken| Turn {
            start: spoken.span.start as f64 / SAMPLE_RATE,
            duration: (spoken.span.end - spoken.span.start) as f64 / SAMPLE_RATE,
            speaker: spoken.speaker,
        })
        .collect();
    Ok(labeled(turns))
}

/// Labels a recording by asking the track who owned each span, the way the
/// transcript is written.
///
/// Listen asks about paragraphs, which the transcriber cut. Here the voice
/// detector cuts them: an independent source, as a paragraph is, without
/// needing a transcriber in the path. What this measures is the ownership
/// rule — `turns` shapes its runs before reporting them, and `attribute`
/// does not, so a voice too small or too scattered to survive one can still
/// win whole spans in the other.
pub fn label_as_attributed(run: Run<'_>) -> Result<Labeled, BenchError> {
    let samples = crate::diarize::sibling::read(run.wav)?;
    let voices = observe(run)?;

    let spans: Vec<Range<u64>> = crate::diarize::sibling::speech(&samples)?
        .into_iter()
        .map(|region| region.start as u64..region.end as u64)
        .collect();
    let named = voices.attribute(&spans);
    // Speech the detector found and the track could not name. It costs a
    // miss exactly as a wrong name costs a confusion, and an RTTM shows only
    // the rows that made it — so a coverage hole is invisible without this.
    let unnamed: Vec<&Range<u64>> = spans
        .iter()
        .zip(&named)
        .filter(|(_, speaker)| speaker.is_none())
        .map(|(span, _)| span)
        .collect();
    let turns = spans
        .iter()
        .zip(&named)
        .filter_map(|(span, speaker)| {
            speaker.clone().map(|speaker| Turn {
                start: span.start as f64 / SAMPLE_RATE,
                duration: (span.end - span.start) as f64 / SAMPLE_RATE,
                speaker,
            })
        })
        .collect();
    Ok(Labeled {
        turns,
        dropped_segments: unnamed.len(),
        dropped_seconds: unnamed
            .iter()
            .map(|span| (span.end - span.start) as f64 / SAMPLE_RATE)
            .sum(),
        embeddings: Vec::new(),
    })
}

/// Runs the track over a whole recording and hands back what it heard.
///
/// Hearing and grouping are driven separately here, which the product does
/// not need to do — it lets a sweep keep the chunks under `cache` and pay
/// for the nets once. What the track ends up believing is the same either
/// way; only `retained_from` differs, and nothing below reads it.
fn observe(run: Run<'_>) -> Result<VoiceTrack, BenchError> {
    let samples = crate::diarize::sibling::read(run.wav)?;

    let model = run.speaker.model.clone();
    let Some(diarizer) = aria::speaker::open::<WeSpeaker>(run.speaker, Vec::new()) else {
        return Err(BenchError::NoSpeakerModel);
    };
    let Some(mut voices) = voice_track(run.segmentation, diarizer, run.grouping) else {
        return Err(BenchError::NoSpeakerModel);
    };

    // The product's track reads a live `RecordingReader`; give it a finished
    // one rather than a second code path that reads a slice.
    let mut recording = RecordingReader::from_samples(&samples);
    recording.fill().map_err(|_| BenchError::NoSpeakerModel)?;

    let nets = crate::diarize::cache::Nets {
        segmentation: run.segmentation,
        speaker: &model,
    };
    let chunks = crate::diarize::cache::heard(&mut voices, &recording, run.wav, nets, run.cache)?;

    let mut journal = run.journal.map(Journal::create).transpose()?;
    for chunk in &chunks {
        let merges = voices.group(chunk).merges;
        if let Some(journal) = &mut journal {
            journal.chunk(chunk.at, &merges, &voices.heard_so_far())?;
        }
    }
    Ok(voices)
}

/// A timeline the track drew itself, which by construction leaves nothing
/// unnamed — every stretch in it is a stretch it had a voice for.
fn labeled(turns: Vec<Turn>) -> Labeled {
    Labeled {
        turns,
        dropped_segments: 0,
        dropped_seconds: 0.0,
        embeddings: Vec::<Embedded>::new(),
    }
}
