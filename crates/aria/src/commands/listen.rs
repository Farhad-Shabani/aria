use std::io::Write;

use anyhow::Context;
use aria_daemon::{DaemonSession, ListenStage};
use aria_platform::runtime::PreparedRuntimePaths;
use clap::Subcommand;

#[derive(Subcommand)]
pub(super) enum ListenCommand {
    /// Start capturing system audio into a new transcript file
    Start {
        /// Write the transcript to this exact file instead of a timestamped
        /// one in the configured transcripts directory
        #[arg(long, short)]
        output: Option<std::path::PathBuf>,
        /// Print the transcript here as it grows, until Ctrl-C or stop
        #[arg(long, short)]
        follow: bool,
    },
    /// Print the running capture's transcript as it grows
    Follow,
    /// Stop the running capture
    Stop,
    /// Report whether a capture is running and where it writes
    Status,
    /// Start when idle, stop when listening
    Toggle {
        /// Transcript file to use when this toggle starts a capture
        #[arg(long, short)]
        output: Option<std::path::PathBuf>,
    },
}

pub(super) fn run(command: ListenCommand) -> anyhow::Result<()> {
    let paths = PreparedRuntimePaths::for_current_user()
        .context("failed to prepare daemon runtime paths")?;
    let session = DaemonSession::connect_at(&paths)?;
    match command {
        ListenCommand::Start { output, follow } => {
            listen_start(&session, output)?;
            if follow {
                listen_follow(&session)?;
            }
            Ok(())
        }
        ListenCommand::Follow => listen_follow(&session),
        ListenCommand::Stop => listen_stop(&session),
        ListenCommand::Status => {
            let status = session.listen_status()?;
            if status.listening {
                println!("listening -> {}", status.transcript_path);
            } else if status.error.is_empty() {
                println!("not listening");
            } else {
                println!("not listening; the last session failed: {}", status.error);
                std::process::exit(1);
            }
            Ok(())
        }
        ListenCommand::Toggle { output } => {
            if session.listen_status()?.listening {
                listen_stop(&session)
            } else {
                listen_start(&session, output)
            }
        }
    }
}

fn listen_start(session: &DaemonSession, output: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let started = session.listen_start(output)?;
    let already = if started.already_listening {
        "already "
    } else {
        ""
    };
    println!("{already}listening -> {}", started.transcript_path);
    Ok(())
}

/// Streams the running session's segments here as they finalize, until
/// Ctrl-C or stop. Labels are the tentative live ones; the transcript
/// file's final rewrite may settle on others.
fn listen_follow(session: &DaemonSession) -> anyhow::Result<()> {
    let follow = session.follow_listen()?;
    let transcript_path = follow.transcript_path.clone();

    // Paragraph-style streaming, mirroring the transcript file's look: a
    // pause or a speaker change opens a fresh timestamped line, and the words
    // of one paragraph flow onto it as they finalize. The stamp is the
    // recording's own clock, so it reads the same as the file however late
    // this attached.
    let mut open_label: Option<Option<String>> = None;
    let mut last_segment = std::time::Instant::now();
    let mut failure = None;
    for stage in follow {
        match stage? {
            ListenStage::Started => {}
            ListenStage::SegmentTranscribed {
                text,
                label,
                at_seconds,
            } => {
                let now = std::time::Instant::now();
                let label = label.as_deref().map(str::to_owned);
                let mut stdout = std::io::stdout().lock();
                if open_label.as_ref() != Some(&label)
                    || now.duration_since(last_segment) >= aria_voice::listen::GAP_BREAK
                {
                    if open_label.is_some() {
                        writeln!(stdout)?;
                        writeln!(stdout)?;
                    }
                    write!(stdout, "{} ", aria_voice::listen::stamp(at_seconds))?;
                    if let Some(label) = &label {
                        write!(stdout, "{label}: ")?;
                    }
                    write!(stdout, "{text}")?;
                } else {
                    write!(stdout, " {text}")?;
                }
                stdout.flush()?;
                open_label = Some(label);
                last_segment = now;
            }
            // The terminal only appends, so a correction is a note rather
            // than a rewrite of what is already on screen. The file is where
            // the old name actually changes.
            ListenStage::SpeakerMerged { from, into } => {
                let mut stdout = std::io::stdout().lock();
                if open_label.is_some() {
                    writeln!(stdout)?;
                    writeln!(stdout)?;
                }
                writeln!(stdout, "-- {from} is {into}")?;
                stdout.flush()?;
                open_label = None;
            }
            ListenStage::Stopped => break,
            ListenStage::Failed(code) => {
                failure = Some(code);
                break;
            }
        }
    }
    if open_label.is_some() {
        println!();
    }
    match failure {
        Some(code) => println!("-- failed: {}; transcript: {transcript_path}", code.label()),
        None => println!("-- stopped; transcript: {transcript_path}"),
    }
    Ok(())
}

fn listen_stop(session: &DaemonSession) -> anyhow::Result<()> {
    let stopped = session.listen_stop()?;
    if stopped.was_listening {
        println!("stopped -> {}", stopped.transcript_path);
    } else {
        println!("nothing was listening");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn listen_commands_are_public_subcommands() {
        let parse = |arguments: &[&str]| {
            let cli = Cli::try_parse_from(arguments).expect("the listen command should parse");
            let Command::Listen { command } = cli.command else {
                panic!("a listen invocation should parse as Listen");
            };
            command
        };

        assert!(matches!(
            parse(&["aria", "listen", "start"]),
            ListenCommand::Start {
                output: None,
                follow: false
            }
        ));
        assert!(matches!(
            parse(&["aria", "listen", "start", "-o", "meeting.txt", "--follow"]),
            ListenCommand::Start { output: Some(path), follow: true } if path == std::path::Path::new("meeting.txt")
        ));
        assert!(matches!(
            parse(&["aria", "listen", "follow"]),
            ListenCommand::Follow
        ));
        assert!(matches!(
            parse(&["aria", "listen", "stop"]),
            ListenCommand::Stop
        ));
        assert!(matches!(
            parse(&["aria", "listen", "status"]),
            ListenCommand::Status
        ));
        assert!(matches!(
            parse(&["aria", "listen", "toggle"]),
            ListenCommand::Toggle { output: None }
        ));
    }
}
