use anyhow::Context;
use aria_daemon::{Daemon, DaemonSession, WakeWordStatus, read_log_tail};
use clap::Subcommand;

use super::logging::init_daemon_logging;
use super::runtime_paths;

#[derive(Subcommand)]
pub(super) enum WakeCommand {
    /// Run wake-word monitoring in the foreground
    Run,
    /// Report whether wake-word monitoring is listening
    Status,
    /// Show the latest daemon log entries
    Log,
}

pub(super) fn status() -> anyhow::Result<()> {
    let status = DaemonSession::wake_word_status()?;
    let (message, success) = status_message(status);
    println!("{message}");
    if !success {
        std::process::exit(1);
    }
    Ok(())
}

pub(super) fn run() -> anyhow::Result<()> {
    let daemon = Daemon::open_wake_word().context("failed to prepare daemon runtime paths")?;

    init_daemon_logging(daemon.log_path())?;
    tracing::info!(socket = %daemon.socket_path().display(), "starting Aria wake-word daemon");

    daemon
        .run()
        .context("Aria wake-word daemon terminated with an error")
}

pub(super) fn log() -> anyhow::Result<()> {
    let paths = runtime_paths()?;
    let tail = read_log_tail(paths.log_path().to_path_buf())?;
    if tail.is_empty() {
        println!("daemon log is empty");
    } else {
        println!("{tail}");
    }
    Ok(())
}

fn status_message(status: WakeWordStatus) -> (&'static str, bool) {
    match status {
        WakeWordStatus::Listening => ("listening", true),
        WakeWordStatus::Degraded => ("degraded: wake-word monitoring failed", false),
        WakeWordStatus::Disabled => (
            "disabled: daemon is running without wake-word monitoring",
            false,
        ),
        WakeWordStatus::Stopped => ("stopped: daemon is not running", false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn wake_commands_are_public_subcommands() {
        let run =
            Cli::try_parse_from(["aria", "wake", "run"]).expect("wake run should be accepted");
        let status = Cli::try_parse_from(["aria", "wake", "status"])
            .expect("wake status should be accepted");
        let log =
            Cli::try_parse_from(["aria", "wake", "log"]).expect("wake log should be accepted");

        assert!(matches!(
            run.command,
            Command::Wake {
                command: WakeCommand::Run
            }
        ));
        assert!(matches!(
            status.command,
            Command::Wake {
                command: WakeCommand::Status
            }
        ));
        assert!(matches!(
            log.command,
            Command::Wake {
                command: WakeCommand::Log
            }
        ));
    }

    #[test]
    fn only_a_listening_wake_monitor_has_a_successful_status() {
        for (status, expected) in [
            (WakeWordStatus::Listening, true),
            (WakeWordStatus::Degraded, false),
            (WakeWordStatus::Disabled, false),
            (WakeWordStatus::Stopped, false),
        ] {
            assert_eq!(status_message(status).1, expected);
        }
    }
}
