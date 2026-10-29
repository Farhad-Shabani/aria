//! Feedback for a person at the machine: a press you can hear start, finish,
//! and fail. A failure reads in the status pill, which already follows the
//! press.

use std::sync::Arc;

use crate::audio::Clip;
use crate::dictation::DictationStage;
use crate::feedback::{self, SYSTEM_SOUNDS};
use crate::primitives::error_chain;
use crate::rpc::RpcErrorCode;

/// The cues, read once from the system sounds. One that did not read stays
/// silent; the press goes on without it.
pub(super) struct MacosFeedback {
    start: Option<Arc<Clip>>,
    stop: Option<Arc<Clip>>,
    error: Option<Arc<Clip>>,
}

impl MacosFeedback {
    pub(super) fn load() -> Self {
        Self {
            start: load_cue(Cue::Start),
            stop: load_cue(Cue::Stop),
            error: load_cue(Cue::Error),
        }
    }

    pub(super) fn observe(&self, stage: &DictationStage, suppress_cues: bool) {
        let Some(cue) = Cue::for_stage(stage, suppress_cues) else {
            return;
        };
        if let Some(clip) = self.clip(cue) {
            feedback::play_clip("dictation-cue", Arc::clone(clip));
        }
    }

    fn clip(&self, cue: Cue) -> Option<&Arc<Clip>> {
        match cue {
            Cue::Start => self.start.as_ref(),
            Cue::Stop => self.stop.as_ref(),
            Cue::Error => self.error.as_ref(),
        }
    }
}

fn load_cue(cue: Cue) -> Option<Arc<Clip>> {
    match feedback::read_caf(cue.sound_path()) {
        Ok(clip) => Some(Arc::new(clip)),
        Err(error) => {
            tracing::warn!(
                error = %error_chain(&error),
                ?cue,
                "a dictation cue will stay silent"
            );
            None
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
            | DictationStage::SegmentTranscribed { .. }
            | DictationStage::SegmentDelivered
            | DictationStage::SpeakerChanged(..)
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

    fn sound_path(self) -> String {
        format!("{SYSTEM_SOUNDS}/{}", self.sound_file())
    }
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

    #[test]
    fn every_cue_file_reads_as_a_clip() {
        for cue in [Cue::Start, Cue::Stop, Cue::Error] {
            let clip = feedback::read_caf(cue.sound_path())
                .unwrap_or_else(|error| panic!("{cue:?} reads: {error}"));
            assert_eq!(clip.sample_rate(), 44_100, "{cue:?}");
            assert!(!clip.samples().is_empty(), "{cue:?}");
        }
    }
}
