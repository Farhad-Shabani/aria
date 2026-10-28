//! Who is speaking, wired to the shipped engines: the diarizer dictation and
//! listen share, and the voice track listen runs beside its transcriber. The
//! policy itself lives in `aria_kit::speaker`.

#[cfg(test)]
pub(crate) mod testing;

use std::time::Instant;

use aria_kit::segmentation::Engine as SegmentationEngine;
use aria_kit::speaker::Engine as SpeakerEngine;
use aria_kit::voice_activity::SileroVad;
use aria_segmentation::pyannote::{self, Pyannote};

use aria_speaker::wespeaker::{self, WeSpeaker};

use crate::config::SpeakerConfig;
use crate::primitives::error_chain;

pub use aria_kit::speaker::{
    Chunk, Diarizer, Enrolled, Enrollment, EnrollmentError, Grouping, Label, Merged, Profile,
    ProfileError, ProfileStore, PyannoteVoices, Replace, Revision, Role, SegmentLabeler,
    SegmentReader, SpeakerName, SpeakerNameError, Standing, StoredProfiles, Verdict, Voiceprint,
    VoiceprintTracker, Voices, attribute_spans, normalized, settle,
};

/// The voice track listen runs: the shipped segmentation net over the shipped
/// embedder.
pub type VoiceTrack = PyannoteVoices<Pyannote, WeSpeaker>;

/// The speaker model. `None` when it is not installed, which is ordinary —
/// it ships with nothing.
pub(crate) fn embedder(config: &SpeakerConfig) -> Option<WeSpeaker> {
    let model = wespeaker::Config {
        model: config.model.clone(),
    };
    if let Err(error) = WeSpeaker::check_from(&model) {
        tracing::info!(%error, "no speaker model; the transcript stays unlabeled");
        return None;
    }
    let started = Instant::now();
    match WeSpeaker::load_from(model) {
        Ok(embedder) => {
            tracing::info!(
                engine = wespeaker::NAME,
                load_ms = started.elapsed().as_millis(),
                "speaker model loaded"
            );
            Some(embedder)
        }
        Err(error) => {
            tracing::warn!(
                error = %error_chain(&error),
                "speaker model unusable; the transcript stays unlabeled"
            );
            None
        }
    }
}

/// The bundled speech gate, or none when the model will not load: audio then
/// passes ungated rather than the session failing.
pub fn speech_gate() -> Option<SileroVad> {
    SileroVad::load()
        .inspect_err(|error| {
            tracing::warn!(
                error = %error_chain(error),
                "no voice-activity model; audio is not speech-gated"
            );
        })
        .ok()
}

/// The segment reader listen runs. The speech gate marks
/// the pauses between speakers; the engines' word timings blur them.
pub(crate) fn reader(config: &SpeakerConfig) -> SegmentReader<WeSpeaker> {
    let diarizer = embedder(config).map(|embedder| Diarizer::new(embedder, enrolled(config)));
    SegmentReader::new(SegmentLabeler::new(speech_gate(), diarizer))
}

/// The models dictation tracks voices with: the diarizer, then the
/// segmenter. A segmenter only serves a diarizer, so it loads only beside
/// one.
pub(crate) fn tracking_models(
    config: &SpeakerConfig,
) -> (Option<Diarizer<WeSpeaker>>, Option<Pyannote>) {
    let Some(embedder) = embedder(config) else {
        return (None, None);
    };
    let diarizer = Diarizer::new(embedder, enrolled(config));
    (Some(diarizer), segmenter(&config.segmentation_model))
}

/// The profiles the shipped embedder can compare against. One enrollment
/// serves every feature that recognizes a speaker.
pub(crate) fn enrolled(config: &SpeakerConfig) -> Vec<Profile> {
    usable_profiles(
        ProfileStore::open(&config.speakers_dir)
            .read_for(wespeaker::NAME, wespeaker::EMBEDDING_DIM),
    )
}

/// The profiles that loaded. A failed read is logged and left out, so one
/// broken file does not cost a session every other enrolled voice.
pub fn usable_profiles(stored: Result<StoredProfiles, ProfileError>) -> Vec<Profile> {
    let stored = match stored {
        Ok(stored) => stored,
        Err(error) => {
            tracing::warn!(
                error = %error_chain(&error),
                "the speakers directory is unreadable; nobody is recognized"
            );
            return Vec::new();
        }
    };
    for error in &stored.unreadable {
        tracing::warn!(
            error = %error_chain(error),
            "skipping an unreadable speaker profile"
        );
    }
    stored.profiles
}

/// The voice track listen runs beside its transcriber. `None` when no
/// segmentation model is installed, which leaves the live per-segment guess
/// standing.
pub fn voice_track(
    model: &std::path::Path,
    embedder: WeSpeaker,
    enrolled: Vec<Profile>,
    grouping: Grouping,
) -> Option<VoiceTrack> {
    Some(PyannoteVoices::new(
        segmenter(model)?,
        embedder,
        enrolled,
        grouping,
    ))
}

fn segmenter(model: &std::path::Path) -> Option<Pyannote> {
    let settings = pyannote::Config {
        model: model.to_path_buf(),
    };
    if let Err(error) = Pyannote::check_from(&settings) {
        tracing::info!(%error, "segmentation model is unavailable");
        return None;
    }
    let started = Instant::now();
    match Pyannote::load_from(settings) {
        Ok(segmenter) => {
            tracing::info!(
                engine = pyannote::NAME,
                load_ms = started.elapsed().as_millis(),
                "segmentation model loaded"
            );
            Some(segmenter)
        }
        Err(error) => {
            tracing::warn!(
                error = %error_chain(&error),
                "segmentation model is unusable"
            );
            None
        }
    }
}
