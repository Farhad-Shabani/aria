use std::sync::Arc;

use anyhow::Context;
use aria_daemon::{DaemonSession, DeliveryMode, DictationOutcome, DictationRequest, StopMode};
use aria_platform::runtime::PreparedRuntimePaths;

use super::CliOutputMode;
use super::logging::init_daemon_logging;
use crate::cli::Interrupt;

pub(super) fn run(
    mode: Option<CliOutputMode>,
    toggle: bool,
    stream: bool,
    no_project: bool,
) -> anyhow::Result<()> {
    let paths = PreparedRuntimePaths::for_current_user()
        .context("failed to prepare daemon runtime paths")?;
    init_daemon_logging(paths.log_path())?;
    let session = Arc::new(DaemonSession::open_at(&paths)?);
    let project = if no_project || !session.get_settings()?.settings.vocabulary_enabled {
        aria_voice::vocabulary::Context::Disabled
    } else {
        aria_voice::vocabulary::Context::Directory(aria_settings::Directory::new(
            std::env::current_dir()?,
        )?)
    };
    let mut interrupt = Interrupt::install(&session)?;
    // Only `--stream` streams; the delivery setting never applies here.
    let delivery = if stream {
        DeliveryMode::Streaming
    } else {
        DeliveryMode::Complete
    };
    let mut recording = interrupt.recording(delivery);
    let outcome = session.request_dictation(
        DictationRequest {
            output_mode: mode.map(Into::into),
            delivery_mode: Some(delivery),
            stop_mode: if toggle {
                StopMode::Manual
            } else {
                StopMode::OnSilence
            },
            transcript_only: false,
            project,
        },
        |stage| recording.follow(stage),
    );
    if let Some(error) = recording.take_failure() {
        eprintln!("warning: the recording could not be stopped: {error:#}");
    }
    // There is one microphone. A second manual request ends the recording
    // already on it, so this run never listened to anything.
    if let Ok(DictationOutcome::StopAccepted) = outcome {
        recording.erase();
        eprintln!("stopped the recording already running");
    }
    outcome?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use aria_daemon::OutputMode;
    use clap::Parser;

    use crate::commands::{Cli, Command};

    #[test]
    fn dictate_accepts_insert_and_submit() {
        let cli = Cli::try_parse_from(["aria", "dictate", "--mode", "insert-and-submit"]).unwrap();
        let Command::Dictate {
            mode: Some(mode), ..
        } = cli.command
        else {
            panic!("expected a dictation mode");
        };
        assert_eq!(OutputMode::from(mode), OutputMode::InsertAndSubmit);
    }
}
