//! What the desktop and a paired inference server exchange, independent of
//! the wire encoding.

use crate::transcription::{Language, TranscriptUpdate};

/// Uploads are mono PCM16 at this rate, so sample positions are wire units.
const SAMPLE_RATE: u64 = 16_000;
pub(crate) const MAX_AUDIO_SECONDS: u64 = 600;
pub(crate) const MAX_AUDIO_SAMPLES: u64 = MAX_AUDIO_SECONDS * SAMPLE_RATE;
pub(crate) const MAX_AUDIO_BYTES: usize = MAX_AUDIO_SAMPLES as usize * 2;
pub(crate) const MAX_AUDIO_MESSAGE_BYTES: usize = 256 * 1024;
/// Bounds one provisional, one finalized segment, and the cumulative transcript.
pub(crate) const MAX_TEXT_BYTES: usize = 128 * 1024;
/// An update can carry a segment and the completed transcript together.
pub(crate) const MAX_RESPONSE_BYTES: usize = 2 * MAX_TEXT_BYTES + 1024;
pub(crate) const MAX_ENGINE_NAME_BYTES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Strategy {
    Whole,
    Streaming,
}

#[derive(Clone)]
pub(crate) struct Start {
    pub engine: String,
    pub language: Language,
    pub strategy: Strategy,
}

/// What a server runs. Every engine transcribes whole recordings, so
/// `strategy` names the strongest one it supports.
pub(crate) struct Info {
    pub engine: String,
    pub strategy: Strategy,
}

#[derive(Clone)]
pub(crate) struct Completion {
    pub text: String,
    pub engine: String,
}

pub(crate) enum Event {
    Update {
        update: TranscriptUpdate,
        retained_from: u64,
        read_to: u64,
        completed: Option<Completion>,
    },
    Complete(Completion),
}
