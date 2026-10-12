//! Who is speaking, wired to the shipped engines: the diarizer dictation and
//! listen share, and the voice track listen runs beside its transcriber. The
//! policy itself lives in `aria_kit::speaker`.

mod voices;

use aria_kit::segmentation::Engine as SegmentationEngine;
use aria_segmentation::pyannote::{self, Pyannote};

use aria_speaker::wespeaker::{self, WeSpeaker};

use crate::config::SpeakerConfig;

pub use aria_kit::speaker::{
    Diarizer, Enrolled, Enrollment, EnrollmentError, Profile, ProfileError, ProfileStore, Replace,
    Role, SegmentLabeler, SegmentReader, SpeakerName, SpeakerNameError, Verdict, normalized, open,
    settle,
};
pub use aria_kit::voice_activity::speech_gate;
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
    open::<WeSpeaker>(
        wespeaker::Config {
            model: config.model.clone(),
        },
        enrolled(config),
    )
}

/// The segment reader dictation and listen run. The speech gate marks
/// the pauses between speakers; the engines' word timings blur them.
pub(crate) fn reader(config: &SpeakerConfig) -> SegmentReader<WeSpeaker> {
    SegmentReader::new(SegmentLabeler::new(speech_gate(), diarizer(config)))
}

/// The profiles the shipped embedder can compare against. One enrollment
/// serves every feature that recognizes a speaker.
pub(crate) fn enrolled(config: &SpeakerConfig) -> Vec<Profile> {
    ProfileStore::open(&config.speakers_dir).read_for(wespeaker::NAME, wespeaker::EMBEDDING_DIM)
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
