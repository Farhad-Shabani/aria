//! What a dictation run announces.

use std::sync::Arc;

use super::level::MicLevel;
use crate::delivery::{DeliveryOutcome, Placement};
use crate::event::EventEmitter;
use crate::rpc::RpcErrorCode;
use crate::speaker::SpeakerName;

use super::RecordingId;

/// Where a press has got to. A failure names its category from the same
/// vocabulary the transport and the logs use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationStage {
    /// The recording scope is open. The id is what a stop names.
    Recording(RecordingId),
    Transcribing,
    Delivering,
    /// Where the text went, none for a transcript-only press, and how much
    /// of it there was.
    Delivered {
        outcome: Option<DeliveryOutcome>,
        words: u32,
    },
    Cancelled,
    Failed(RpcErrorCode),
    /// A finalized segment, and the enrolled voice the press so far sounds
    /// like.
    SegmentTranscribed {
        text: Arc<str>,
        speaker: Option<SpeakerName>,
    },
    SegmentDelivered,
}

impl DictationStage {
    pub(crate) fn delivered(outcome: Option<DeliveryOutcome>, text: &str) -> Self {
        Self::Delivered {
            outcome,
            words: text.split_whitespace().count() as u32,
        }
    }

    /// How a press that did not deliver ends. A cancellation ends quietly,
    /// not as a failure.
    pub(crate) fn ended_by(code: RpcErrorCode) -> Self {
        match code {
            RpcErrorCode::Cancelled => Self::Cancelled,
            code => Self::Failed(code),
        }
    }

    /// The one spelling of a stage, so its readers cannot drift apart.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Recording(..) => "recording",
            Self::Transcribing => "transcribing",
            Self::Delivering => "delivering",
            Self::Delivered { .. } => "delivered",
            Self::Cancelled => "cancelled",
            Self::Failed(..) => "failed",
            Self::SegmentTranscribed { .. } => "segment transcribed",
            Self::SegmentDelivered => "segment delivered",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CaptionSnapshot {
    pub recording: RecordingId,
    pub committed: Arc<str>,
    pub provisional: Arc<str>,
    pub ended: bool,
}

impl CaptionSnapshot {
    /// A caption with no words in it: before the first, or after the last.
    pub(super) fn empty(recording: RecordingId, ended: bool) -> Self {
        Self {
            recording,
            committed: Arc::from(""),
            provisional: Arc::from(""),
            ended,
        }
    }
}

impl std::fmt::Debug for CaptionSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CaptionSnapshot")
            .field("recording", &self.recording)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

/// Words streaming has heard but not typed yet, and where to draw them.
/// Sent with each changed decode preview, and empty once a segment is typed
/// or fails to type.
#[derive(Clone, PartialEq)]
pub(crate) struct GhostSnapshot {
    pub recording: RecordingId,
    pub text: Arc<str>,
    pub placement: Option<Placement>,
}

impl GhostSnapshot {
    pub(super) fn empty(recording: RecordingId) -> Self {
        Self {
            recording,
            text: Arc::from(""),
            placement: None,
        }
    }
}

impl std::fmt::Debug for GhostSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GhostSnapshot")
            .field("recording", &self.recording)
            .field("placement", &self.placement)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DictationEvent {
    Stage(DictationStage),
    Caption(CaptionSnapshot),
    Ghost(GhostSnapshot),
    /// How loud the microphone is, while one is recording.
    Level(MicLevel),
}

impl From<DictationStage> for DictationEvent {
    fn from(stage: DictationStage) -> Self {
        Self::Stage(stage)
    }
}

impl From<CaptionSnapshot> for DictationEvent {
    fn from(caption: CaptionSnapshot) -> Self {
        Self::Caption(caption)
    }
}

impl From<GhostSnapshot> for DictationEvent {
    fn from(ghost: GhostSnapshot) -> Self {
        Self::Ghost(ghost)
    }
}

impl From<MicLevel> for DictationEvent {
    fn from(level: MicLevel) -> Self {
        Self::Level(level)
    }
}

pub(crate) type DictationEventEmitter = EventEmitter<DictationEvent>;

pub(crate) const EVENT_THREAD: &str = "aria-dictation-events";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cancellation_is_not_announced_as_failure() {
        assert_eq!(
            DictationStage::ended_by(RpcErrorCode::Cancelled),
            DictationStage::Cancelled
        );
        assert_eq!(
            DictationStage::ended_by(RpcErrorCode::TranscriptionFailed),
            DictationStage::Failed(RpcErrorCode::TranscriptionFailed)
        );
    }
}
