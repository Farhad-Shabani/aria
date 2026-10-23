//! Feedback for a person at the machine: a press you can hear start, finish,
//! and fail. A failure reads in the status pill, which already follows the
//! press.

use crate::dictation::DictationStage;
use crate::feedback::{self, SYSTEM_SOUNDS};
use crate::rpc::RpcErrorCode;

pub(super) struct MacosFeedback;

impl MacosFeedback {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn observe(&self, stage: &DictationStage, suppress_cues: bool) {
        if let Some(cue) = Cue::for_stage(stage, suppress_cues) {
            play(cue);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cue {
    Start,
    Stop,
    Error,
}

impl Cue {
    fn for_stage(stage: &DictationStage, suppress_cues: bool) -> Option<Self> {
        if suppress_cues {
            return None;
        }
        match stage {
            DictationStage::Recording(..) => Some(Self::Start),
            // A focus change or a switched-off app is an ending, not a failure.
            DictationStage::Delivered { .. }
            | DictationStage::Failed(
                RpcErrorCode::DeliveryTargetChanged
                | RpcErrorCode::DeliveryInterrupted
                | RpcErrorCode::ApplicationExcluded,
            ) => Some(Self::Stop),
            DictationStage::Failed(_) => Some(Self::Error),
            DictationStage::Transcribing
            | DictationStage::Delivering
            | DictationStage::SegmentTranscribed(..)
            | DictationStage::SegmentDelivered
            | DictationStage::Cancelled => None,
        }
    }

    fn sound_file(self) -> &'static str {
        match self {
            Self::Start => "begin_record.caf",
            Self::Stop => "end_record.caf",
            Self::Error => "mic_unmute_fail.caf",
        }
    }
}

fn play(cue: Cue) {
    feedback::play(
        "dictation-cue",
        &format!("{SYSTEM_SOUNDS}/{}", cue.sound_file()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mute_policy_suppresses_every_cue() {
        let stages = [
            (
                DictationStage::Recording(crate::dictation::RecordingId(1)),
                Cue::Start,
            ),
            (
                DictationStage::Delivered {
                    outcome: None,
                    words: 0,
                },
                Cue::Stop,
            ),
            (
                DictationStage::Failed(RpcErrorCode::CaptureFailed),
                Cue::Error,
            ),
            (
                DictationStage::Failed(RpcErrorCode::DeliveryInterrupted),
                Cue::Stop,
            ),
            (
                DictationStage::Failed(RpcErrorCode::DeliveryTargetChanged),
                Cue::Stop,
            ),
            (
                DictationStage::Failed(RpcErrorCode::ApplicationExcluded),
                Cue::Stop,
            ),
        ];
        for (stage, cue) in stages {
            assert_eq!(Cue::for_stage(&stage, false), Some(cue));
            assert_eq!(Cue::for_stage(&stage, true), None);
        }
        assert_eq!(Cue::for_stage(&DictationStage::Cancelled, false), None);
        assert_eq!(Cue::for_stage(&DictationStage::Transcribing, false), None);
    }

    #[test]
    fn each_cue_has_its_own_system_sound() {
        let files = [Cue::Start, Cue::Stop, Cue::Error].map(Cue::sound_file);
        assert_eq!(
            files,
            ["begin_record.caf", "end_record.caf", "mic_unmute_fail.caf"]
        );
    }
}
