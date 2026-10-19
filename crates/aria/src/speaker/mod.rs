//! Who is speaking, wired to the shipped engines: the diarizer dictation and
//! listen share, and the voice track listen runs beside its transcriber. The
//! policy itself lives in `aria_kit::speaker`.

mod voices;

use aria_kit::segmentation::Engine as SegmentationEngine;
use aria_kit::speaker::Engine as SpeakerEngine;
use aria_kit::voice_activity::SileroVad;
use aria_segmentation::pyannote::{self, Pyannote};

use aria_speaker::wespeaker::{self, WeSpeaker};

use crate::config::SpeakerConfig;

pub use aria_kit::speaker::{
    Diarizer, Enrolled, Enrollment, EnrollmentError, Profile, ProfileError, ProfileStore, Replace,
    Role, SegmentLabeler, SegmentReader, SpeakerName, SpeakerNameError, StoredProfiles, Verdict,
    normalized, settle,
};
#[cfg(test)]
pub(crate) use voices::testing;
pub use voices::{
    Chunk, Grouping, Merged, PyannoteVoices, Revision, Standing, Voiceprint, Voices,
    attribute_spans,
};

/// The voice track listen runs: the shipped segmentation net over the shipped
/// embedder.
pub type VoiceTrack = PyannoteVoices<Pyannote, WeSpeaker>;

/// The diarizer dictation and listen share. `None` when no embedding model
/// is installed, which is ordinary — it ships with nothing.
pub(crate) fn diarizer(config: &SpeakerConfig) -> Option<Diarizer<WeSpeaker>> {
    let model = wespeaker::Config {
        model: config.model.clone(),
    };
    if let Err(error) = WeSpeaker::check_from(&model) {
        tracing::info!(%error, "no speaker model; the transcript stays unlabeled");
        return None;
    }
    match WeSpeaker::load_from(model) {
        Ok(embedder) => Some(Diarizer::new(embedder, enrolled(config))),
        Err(error) => {
            tracing::warn!(%error, "speaker model unusable; the transcript stays unlabeled");
            None
        }
    }
}

/// The bundled speech gate, or none when the model will not load: audio then
/// passes ungated rather than the session failing.
pub fn speech_gate() -> Option<SileroVad> {
    SileroVad::load()
        .inspect_err(
            |error| tracing::warn!(%error, "no voice-activity model; audio is not speech-gated"),
        )
        .ok()
}

/// The segment reader dictation and listen run. The speech gate marks
/// the pauses between speakers; the engines' word timings blur them.
pub(crate) fn reader(config: &SpeakerConfig) -> SegmentReader<WeSpeaker> {
    SegmentReader::new(SegmentLabeler::new(speech_gate(), diarizer(config)))
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
            tracing::warn!(%error, "the speakers directory is unreadable; nobody is recognized");
            return Vec::new();
        }
    };
    for error in &stored.unreadable {
        tracing::warn!(%error, "skipping an unreadable speaker profile");
    }
    stored.profiles
}

/// The voice track listen runs beside its transcriber. `None` when no
/// segmentation model is installed, which leaves the live per-segment guess
/// standing.
pub fn voice_track(
    model: &std::path::Path,
    diarizer: Diarizer<WeSpeaker>,
    grouping: Grouping,
) -> Option<VoiceTrack> {
    let settings = pyannote::Config {
        model: model.to_path_buf(),
    };
    if let Err(error) = Pyannote::check_from(&settings) {
        tracing::info!(%error, "no segmentation model; speakers stay as the live pass guessed");
        return None;
    }
    match Pyannote::load_from(settings) {
        Ok(segmenter) => Some(PyannoteVoices::new(segmenter, diarizer, grouping)),
        Err(error) => {
            tracing::warn!(%error, "segmentation model unusable; speakers stay as guessed");
            None
        }
    }
}
