//! What a system transcription announces while it runs.

use std::sync::Arc;

use aria_settings::event::EventEmitter;
use aria_settings::rpc::RpcErrorCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemTranscriptionStage {
    /// The session was accepted. Capture may still fail to start, which
    /// `Failed` reports.
    Started,
    SegmentTranscribed {
        text: Arc<str>,
        /// The tentative live speaker label; the transcript's final rewrite
        /// may settle on another.
        label: Option<Arc<str>>,
        /// Seconds into the recording, so a follower's clock reads the same
        /// as the transcript file's whenever it attached.
        at_seconds: u64,
    },
    /// A voice turned out to be one already heard. Everything `from` was
    /// named is now `into`, in the file and from here on.
    SpeakerMerged { from: Arc<str>, into: Arc<str> },
    /// The session ended as asked.
    Stopped,
    /// The session ended on its own, and why.
    Failed(RpcErrorCode),
}

pub type SystemTranscriptionEventEmitter = EventEmitter<SystemTranscriptionStage>;

/// How loud the Mac's audio is right now, from silent to pinned. About 20
/// a second while a session runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SystemAudioLevel {
    /// 0 at the noise floor, 1 at loud audio.
    pub level: f32,
}

/// A level that arrives late is worth less than none, so whoever wires this
/// drops levels a full queue can't take.
pub type SystemAudioLevelEmitter = EventEmitter<SystemAudioLevel>;

pub const EVENT_THREAD: &str = "aria-transcribe-events";
