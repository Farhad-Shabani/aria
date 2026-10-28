//! Domain types for push-to-talk operations.

use std::fmt;
use std::time::Duration;

use crate::audio::SilenceDuration;
use crate::transcription::Language;

pub(crate) mod cleanup;
mod cursor;
mod event;
mod execution;
mod level;
#[cfg(target_os = "macos")]
mod output;
mod pipeline;
mod rewrites;
pub(crate) mod subscribers;
#[cfg(test)]
pub(crate) mod testing;
mod transcripts;

pub use crate::delivery::{DeliveryOutcome, OutputMode};
pub use cleanup::{CleanupError, LlmCleanup};
pub(crate) use cursor::Cursor;
pub(crate) use event::{
    CaptionSnapshot, DictationEvent, DictationEventEmitter, EVENT_THREAD, GhostSnapshot,
};
pub use event::{DictationStage, HeardSpeaker};
pub(crate) use execution::{
    DictationCoordinator, DictationError, DictationExecutor, DictationPolicy, DictationPolicyError,
    ExecutionControl, Transcribed,
};
use level::Meter;
pub(crate) use level::MicLevel;
pub(crate) use pipeline::DictationPipeline;
pub(crate) use rewrites::{ConfiguredLayers, Rewrites};
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
    /// The app in front as dictation was admitted.
    pub(crate) application: Option<crate::history::Application>,
    pub(crate) target: crate::delivery::TargetPolicy,
}

impl DictationSettings {
    /// Whether the app in front at admission has Aria switched off.
    pub(crate) fn application_excluded(&self) -> bool {
        self.target.excludes(self.application.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictationResult {
    pub text: String,
    /// The speaker model matched the recording to an enrolled owner.
    pub own_voice: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationOutcome {
    Completed(DictationResult),
    StopAccepted,
}
