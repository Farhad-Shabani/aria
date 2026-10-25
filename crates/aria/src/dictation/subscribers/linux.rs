//! Feedback for a person at the machine: sound cues and failure banners.
//!
//! Both surfaces are stock freedesktop tooling — `paplay` with the theme
//! sounds, `notify-send` for banners — so a desktop without them just logs.

use std::process::{Command, Stdio};

use crate::dictation::DictationStage;
use crate::feedback::run_detached;
use crate::feedback::{self, SYSTEM_SOUNDS};

use super::message;

const NOTIFY_SEND: &str = "/usr/bin/notify-send";
const TITLE: &str = "Aria";

pub(super) struct LinuxFeedback;

impl LinuxFeedback {
    pub(super) fn new() -> Self {
        Self
    }

    pub(super) fn observe(&self, stage: &DictationStage) {
        match stage {
            DictationStage::Recording(..) => play("message-new-instant.oga"),
            DictationStage::Delivered { .. } => play("complete.oga"),
            DictationStage::Failed(code) => {
                play("dialog-error.oga");
                show(message(*code));
            }
            DictationStage::Transcribing
            | DictationStage::Delivering
            | DictationStage::SegmentTranscribed { .. }
            | DictationStage::SegmentDelivered
            | DictationStage::Cancelled => {}
        }
    }
}

fn play(sound_file: &str) {
    feedback::play("dictation-cue", &format!("{SYSTEM_SOUNDS}/{sound_file}"));
}

// The session environment stays: `notify-send` needs the session bus
// address to reach its daemon.
fn show(message: &str) {
    let mut command = Command::new(NOTIFY_SEND);
    command
        .args(["--", TITLE, message])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    run_detached("dictation-notification", command);
}
