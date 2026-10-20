//! Domain types for push-to-talk operations.

use std::fmt;
use std::time::Duration;

use crate::audio::SilenceDuration;
use crate::transcription::Language;

mod backtrack;
pub(crate) mod cleanup;
mod cursor;
mod event;
mod execution;
mod fillers;
mod level;
#[cfg(target_os = "macos")]
mod output;
mod pipeline;
mod speech;
pub(crate) mod subscribers;
#[cfg(test)]
pub(crate) mod testing;
mod transcripts;

pub use crate::delivery::{DeliveryOutcome, OutputMode};
use backtrack::Backtrack;
pub use cleanup::{CleanupError, LlmCleanup};
pub(crate) use cursor::Cursor;
pub use event::DictationStage;
pub(crate) use event::{
    CaptionSnapshot, DictationEvent, DictationEventEmitter, EVENT_THREAD, GhostSnapshot,
};
pub(crate) use execution::{
    DictationCoordinator, DictationError, DictationExecutor, DictationPolicy, DictationPolicyError,
    ExecutionControl,
};
use fillers::Fillers;
use level::Meter;
pub(crate) use level::MicLevel;
pub(crate) use pipeline::DictationPipeline;
pub(crate) use transcripts::{TranscriptSink, Utterance};

pub(crate) const MAX_DICTATION_TIMEOUT: Duration = Duration::from_secs(320);

/// Names one recording for as long as the daemon that took it runs. What
/// `Stop` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordingId(pub u64);

impl fmt::Display for RecordingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "#{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopMode {
    OnSilence,
    Manual,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DeliveryMode {
    #[default]
    Complete,
    Streaming,
}

/// What one dictation should do; `None` means the daemon's preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictationRequest {
    pub output_mode: Option<OutputMode>,
    pub delivery_mode: Option<DeliveryMode>,
    pub stop_mode: StopMode,
    /// Hand the transcript back without delivering it anywhere.
    pub transcript_only: bool,
    pub project: crate::vocabulary::Context,
}

impl DictationRequest {
    pub fn new(stop_mode: StopMode) -> Self {
        Self {
            output_mode: None,
            delivery_mode: None,
            stop_mode,
            transcript_only: false,
            project: crate::vocabulary::Context::Default,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DictationSettings {
    pub(crate) dictation_history: bool,
    pub(crate) history_audio_retention: bool,
    pub(crate) mute_output: bool,
    pub(crate) recognition_language: Language,
    pub(crate) output_mode: OutputMode,
    pub(crate) delivery_mode: DeliveryMode,
    pub(crate) stop_mode: StopMode,
    pub(crate) silence_duration: SilenceDuration,
    pub(crate) own_voice_only: bool,
    pub(crate) transcript_only: bool,
    pub(crate) project: Option<crate::vocabulary::Directory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictationResult {
    pub text: String,
    /// The speaker model matched the recording to an enrolled owner.
    pub own_voice: bool,
}

/// Fixes words in what the decoder heard, on the transcription path.
/// `None` when nothing changed; empty when nothing was worth keeping.
/// Never fails: at worst it changes nothing.
pub(crate) trait Corrector: Send + Sync {
    fn correct(&self, text: &str) -> Option<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationOutcome {
    Completed(DictationResult),
    StopAccepted,
}
