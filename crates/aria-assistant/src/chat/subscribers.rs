//! Everything that reacts to a chat event: a reply you can hear land, and a
//! failure you can hear too.

use crate::chat::{ChatStage, ChatStageKind};

pub struct ChatSubscribers;

impl ChatSubscribers {
    pub fn observe(&self, stage: &ChatStage) {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(sound) = sound_for(&stage.kind) {
            aria_platform::feedback::play("chat-cue", &sound);
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let _ = stage;
    }
}

/// Distinct from dictation's end cue, so a chat and a dictation sound different.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sound_for(kind: &ChatStageKind) -> Option<String> {
    use aria_platform::feedback::SYSTEM_SOUNDS;

    #[cfg(target_os = "macos")]
    const REPLY: &str = "/System/Library/Sounds/Glass.aiff";
    #[cfg(target_os = "macos")]
    const ERROR: &str = "mic_unmute_fail.caf";
    #[cfg(target_os = "linux")]
    const REPLY: &str = "bell.oga";
    #[cfg(target_os = "linux")]
    const ERROR: &str = "dialog-error.oga";

    match kind {
        ChatStageKind::Working(..) => None,
        ChatStageKind::Replied(..) if cfg!(target_os = "macos") => Some(REPLY.to_owned()),
        ChatStageKind::Replied(..) => Some(format!("{SYSTEM_SOUNDS}/{REPLY}")),
        ChatStageKind::Failed { .. } => Some(format!("{SYSTEM_SOUNDS}/{ERROR}")),
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use crate::chat::{ChatReply, Step};
    use aria_settings::rpc::RpcErrorCode;

    use super::*;

    #[test]
    fn a_reply_and_a_failure_sound_different_and_thinking_is_silent() {
        let replied = ChatStageKind::Replied(ChatReply {
            text: "done".to_owned(),
            delivery_error: None,
            copied: false,
        });
        let failed = ChatStageKind::Failed {
            code: RpcErrorCode::AssistantFailed,
            message: String::new(),
        };

        assert_eq!(sound_for(&ChatStageKind::Working(Step::Thinking)), None);
        let reply_sound = sound_for(&replied).expect("a reply sounds");
        let error_sound = sound_for(&failed).expect("a failure sounds");
        assert_ne!(reply_sound, error_sound);
        assert!(error_sound.starts_with(aria_platform::feedback::SYSTEM_SOUNDS));
    }
}
