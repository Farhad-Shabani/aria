//! Domain types for push-to-talk operations.

use std::fmt;

use crate::transcription::Language;
use aria_kit::audio::{AutoStop, SilenceDuration};
use aria_settings::DeliveryMode;

pub(crate) mod cleanup;
mod cursor;
mod event;
mod execution;
mod level;
#[cfg(target_os = "macos")]
mod output;
#[cfg(test)]
mod own_voice_eval;
mod pipeline;
mod resolve;
mod rewrites;
pub mod subscribers;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
mod transcripts;

pub use aria_platform::delivery::OutputMode;
pub use cleanup::{CleanupError, LlmCleanup};
pub use cursor::Cursor;
pub use event::{
    CaptionSnapshot, DictationEvent, DictationEventEmitter, EVENT_THREAD, GhostSnapshot,
};
pub use event::{DictationStage, HeardSpeaker};
pub use execution::{
    DictationCoordinator, DictationError, DictationExecutor, DictationPolicy, DictationPolicyError,
    ExecutionControl, Transcribed,
};
use level::Meter;
pub use level::MicLevel;
pub use pipeline::DictationPipeline;
pub use rewrites::{Cleanup, ConfiguredLayers, Rewrites};
pub use transcripts::{Delivered, DeliveryOutcome, TranscriptSink, Utterance};

/// Names one recording for as long as the daemon that took it runs. What
/// `Stop` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordingId(pub u64);

impl fmt::Display for RecordingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "#{}", self.0)
    }
}

/// Whether a recording may silence other apps' sound. Only a person at the
/// machine wants that, so tests and harness runs never do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundMuting {
    Never,
    Allowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopMode {
    OnSilence,
    Manual,
}

/// Where a requested dictation's transcript goes besides back to the caller.
/// Only a delivered one is typed, so only it takes typing overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DictationRoute {
    /// Each `None` mode comes from the app in front's overrides, else the settings.
    Deliver(aria_settings::DictationOverrides),
    TranscriptOnly,
    HistoryOnly,
}

impl From<DictationRoute> for TranscriptRoute {
    fn from(route: DictationRoute) -> Self {
        match route {
            DictationRoute::Deliver(_) => Self::Deliver,
            DictationRoute::TranscriptOnly => Self::TranscriptOnly,
            DictationRoute::HistoryOnly => Self::HistoryOnly,
        }
    }
}

/// Where a dictation's transcript goes besides back to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptRoute {
    /// Typed or copied as the settings say, and saved to history.
    Deliver,
    /// Not typed and not saved.
    TranscriptOnly,
    /// Not typed, but saved to history.
    HistoryOnly,
}

impl TranscriptRoute {
    pub fn delivers(self) -> bool {
        self == Self::Deliver
    }

    pub fn saves(self) -> bool {
        self != Self::TranscriptOnly
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictationRequest {
    pub stop_mode: StopMode,
    pub route: DictationRoute,
    pub project: crate::vocabulary::Context,
}

impl DictationRequest {
    pub fn new(stop_mode: StopMode) -> Self {
        Self {
            stop_mode,
            route: DictationRoute::Deliver(aria_settings::DictationOverrides::default()),
            project: crate::vocabulary::Context::Default,
        }
    }
}

use crate::history::HistorySource;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictationSettings {
    pub history_enabled: bool,
    pub history_keep_audio: bool,
    pub mute_sound: bool,
    pub stt_language: Language,
    pub output_mode: OutputMode,
    pub delivery_mode: DeliveryMode,
    pub stop_mode: StopMode,
    pub silence_duration: SilenceDuration,
    pub auto_stop: Option<AutoStop>,
    pub own_voice_only: bool,
    pub cleanup: Cleanup,
    pub route: TranscriptRoute,
    pub project: Option<aria_settings::Directory>,
    /// The app in front as dictation was admitted.
    pub application: Option<aria_platform::delivery::Application>,
    pub target: aria_platform::delivery::TargetPolicy,
    /// Where the recording came from, as history records it.
    pub source: HistorySource,
}

impl DictationSettings {
    /// Whether the app in front at admission has Aria switched off.
    pub fn application_excluded(&self) -> bool {
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
