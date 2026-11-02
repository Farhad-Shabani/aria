use std::io::Write;

use anyhow::Context;
use aria_daemon::{
    ChatStage, ChatStageKind, DaemonEvent, DaemonSession, DictationStage, EnrollmentStage,
    ListenStage, SpeechStage,
};
use aria_platform::runtime::PreparedRuntimePaths;

pub(super) fn run() -> anyhow::Result<()> {
    let paths = PreparedRuntimePaths::for_current_user()
        .context("failed to prepare daemon runtime paths")?;
    let events = DaemonSession::connect_at(&paths)?.events()?;
    let mut stdout = std::io::stdout();
    for event in events {
        let written = match event? {
            DaemonEvent::Dictation(DictationStage::Failed(code)) => {
                writeln!(stdout, "failed {}", code.label())
            }
            DaemonEvent::Dictation(stage) => writeln!(stdout, "{}", stage.label()),
            DaemonEvent::Chat(ChatStage {
                ticket,
                assistant_id,
                kind,
            }) => {
                let assistant_name = assistant_id.name();
                match kind {
                    ChatStageKind::Working(step) => {
                        writeln!(stdout, "chat {ticket} {assistant_name} {step}")
                    }
                    ChatStageKind::Replied(reply) => {
                        writeln!(
                            stdout,
                            "chat {ticket} {assistant_name} replied {}",
                            reply.text.lines().next().unwrap_or("")
                        )
                    }
                    ChatStageKind::Failed { code, message } => {
                        writeln!(
                            stdout,
                            "chat {ticket} {assistant_name} failed {}: {message}",
                            code.label()
                        )
                    }
                }
            }
            DaemonEvent::Enrollment(stage) => match stage {
                EnrollmentStage::Recording { index, of } => {
                    writeln!(stdout, "enrollment recording {index}/{of}")
                }
                EnrollmentStage::SampleTaken { index, of } => {
                    writeln!(stdout, "enrollment sample {index}/{of}")
                }
                EnrollmentStage::Failed(code) => {
                    writeln!(stdout, "enrollment failed {}", code.label())
                }
                stage => writeln!(stdout, "enrollment {}", stage.label()),
            },
            DaemonEvent::Speech(stage) => match stage {
                SpeechStage::Synthesizing => writeln!(stdout, "speech synthesizing"),
                SpeechStage::Speaking => writeln!(stdout, "speech speaking"),
                SpeechStage::Spoken => writeln!(stdout, "speech spoken"),
                SpeechStage::Stopped => writeln!(stdout, "speech stopped"),
                SpeechStage::Cancelled => writeln!(stdout, "speech cancelled"),
                SpeechStage::Failed(code) => writeln!(stdout, "speech failed {}", code.label()),
            },
            DaemonEvent::Listen(stage) => match stage {
                ListenStage::Started => writeln!(stdout, "listen started"),
                ListenStage::SegmentTranscribed { text, label, .. } => match label {
                    Some(label) => writeln!(stdout, "listen segment [{label}] {text}"),
                    None => writeln!(stdout, "listen segment {text}"),
                },
                ListenStage::SpeakerMerged { from, into } => {
                    writeln!(stdout, "listen merged {from} -> {into}")
                }
                ListenStage::Stopped => writeln!(stdout, "listen stopped"),
                ListenStage::Failed(code) => writeln!(stdout, "listen failed {}", code.label()),
            },
            DaemonEvent::Settings(status) => writeln!(stdout, "{}", settings_line(&status)),
        };
        match written {
            Ok(()) => {}
            // A closed pipe is the reader saying it has seen enough.
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// One line per status: the fault, else what waits for a restart, else ok.
fn settings_line(status: &aria_settings::SettingsStatus) -> String {
    if let Some(fault) = &status.fault {
        return match fault.position {
            Some(position) => format!("settings invalid at {position}: {}", fault.message),
            None => format!("settings invalid: {}", fault.message),
        };
    }
    let mut line = "settings ok".to_owned();
    if !status.restart_required.is_empty() {
        line.push_str(&format!(
            "; restart for {}",
            status.restart_required.join(", ")
        ));
    }
    if !status.reverted.is_empty() {
        line.push_str(&format!(
            "; an editor reverted {}",
            status.reverted.join(", ")
        ));
    }
    line
}
