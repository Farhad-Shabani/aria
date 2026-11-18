//! What a dictation run announces.

use std::sync::Arc;

use super::level::MicLevel;
use crate::speaker::SpeakerName;
use aria_kit::audio::InputDeviceId;
use aria_platform::delivery::Placement;
use aria_settings::DeliveryMode;
use aria_settings::event::EventEmitter;
use aria_settings::rpc::RpcErrorCode;

use super::transcripts::{Delivered, DeliveryOutcome};
use super::{RecordingId, TranscriptRoute};

/// Where a press has got to. A failure names its category from the same
/// vocabulary the transport and the logs use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DictationStage {
    /// The recording scope is open. The id is what a stop names,
    /// `delivery` is how the press delivers: Complete where it can't stream,
    /// and `route` is where its text goes.
    Recording {
        id: RecordingId,
        delivery: DeliveryMode,
        route: TranscriptRoute,
    },
    /// Capture is live, from `microphone`. Follows `Recording` once capture
    /// starts.
    Capturing {
        id: RecordingId,
        microphone: CapturedMicrophone,
    },
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
    /// Another speaker started, announced before any of their words are
    /// typed.
    SpeakerChanged(HeardSpeaker),
    /// A finalized segment, and the enrolled voice the press so far sounds
    /// like.
    SegmentTranscribed {
        text: Arc<str>,
        speaker: Option<SpeakerName>,
    },
    SegmentDelivered,
    /// A finalized segment decoded while its field lacks focus. It types
    /// when the field has focus again, and `SegmentDelivered` follows,
    /// unless the press ends first.
    SegmentQueued,
    /// The person's Return was held back to press after the text, and never
    /// pressed: Aria could not confirm the field. Comes after the terminal
    /// stage.
    ReturnDropped,
}

/// What a live recording captures from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedMicrophone {
    /// An input device.
    Device { id: InputDeviceId },
    /// Audio pushed in as it arrives.
    Pushed,
    /// Audio recorded earlier, such as a replay.
    Prerecorded,
}

/// What a press does with the current speaker's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeardSpeaker {
    /// Too little is heard yet to tell who is speaking.
    Undecided,
    /// Their words are typed, named when the speaker is enrolled.
    Typed(Option<SpeakerName>),
    /// Their words are not typed: the press types only the owner's.
    KeptOut,
}

impl DictationStage {
    pub fn delivered(delivered: Option<Delivered>, text: &str) -> Self {
        Self::Delivered {
            outcome: delivered.map(|delivered| delivered.outcome),
            words: delivered
                .and_then(|delivered| delivered.words)
                .unwrap_or_else(|| text.split_whitespace().count() as u32),
        }
    }

    /// How a press that did not deliver ends. A cancellation ends quietly,
    /// not as a failure.
    pub fn ended_by(code: RpcErrorCode) -> Self {
        match code {
            RpcErrorCode::Cancelled => Self::Cancelled,
            code => Self::Failed(code),
        }
    }

    /// The one spelling of a stage, so its readers cannot drift apart.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Recording { .. } => "recording",
            Self::Capturing { .. } => "capturing",
            Self::Transcribing => "transcribing",
            Self::Delivering => "delivering",
            Self::Delivered { .. } => "delivered",
            Self::Cancelled => "cancelled",
            Self::Failed(..) => "failed",
            Self::SegmentTranscribed { .. } => "segment transcribed",
            Self::SegmentDelivered => "segment delivered",
            Self::SegmentQueued => "segment queued",
            Self::ReturnDropped => "return dropped",
            Self::SpeakerChanged(..) => "voice heard",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct CaptionSnapshot {
    pub recording: RecordingId,
    pub finalized: Arc<str>,
    pub provisional: Arc<str>,
    pub ended: bool,
}

impl CaptionSnapshot {
    /// A caption with no words in it: before the first, or after the last.
    pub(super) fn empty(recording: RecordingId, ended: bool) -> Self {
        Self {
            recording,
            finalized: Arc::from(""),
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
pub struct GhostSnapshot {
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
pub enum DictationEvent {
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

pub type DictationEventEmitter = EventEmitter<DictationEvent>;

pub const EVENT_THREAD: &str = "aria-dictation-events";

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

    #[test]
    fn a_caption_snapshot_debugs_without_its_words() {
        for ended in [false, true] {
            let caption = CaptionSnapshot {
                recording: RecordingId(42),
                finalized: Arc::from("private finalized words"),
                provisional: Arc::from("private provisional words"),
                ended,
            };
            let debug = format!("{caption:?}");
            assert!(!debug.contains("private"));
        }
    }

    #[test]
    fn a_ghost_snapshot_debugs_without_its_words() {
        let ghost = GhostSnapshot {
            recording: RecordingId(42),
            text: Arc::from("private unconfirmed words"),
            placement: Some(Placement {
                caret: aria_platform::delivery::Rect {
                    x: 10.0,
                    y: 20.0,
                    width: 1.0,
                    height: 16.0,
                },
                field: None,
                font: Some(aria_platform::delivery::Font {
                    name: "Helvetica".to_owned(),
                    size: 13.0,
                }),
            }),
        };
        assert!(!format!("{ghost:?}").contains("private"));
    }
}
