use std::io::Write;

use aria_daemon::{DaemonSession, SystemTranscriptionStage};
use clap::Subcommand;

use super::runtime_paths;

#[derive(Subcommand)]
pub(super) enum TranscribeCommand {
    /// Start transcribing what this machine plays into a transcript file
    Start {
        /// Write the transcript to this exact file instead of a timestamped
        /// one in the configured transcripts directory
        #[arg(long, short)]
        output: Option<std::path::PathBuf>,
        /// Print the transcript here as it grows, until Ctrl-C or stop
        #[arg(long, short)]
        follow: bool,
    },
    /// Print the running transcript as it grows
    Follow,
    /// Stop transcribing
    Stop,
    /// Report whether Transcription is running and where it writes
    Status,
    /// Start when idle, stop when transcribing
    Toggle {
        /// Transcript file to use when this toggle starts Transcription
        #[arg(long, short)]
        output: Option<std::path::PathBuf>,
    },
}

pub(super) fn run(command: TranscribeCommand) -> anyhow::Result<()> {
    let paths = runtime_paths()?;
    let session = DaemonSession::connect_at(&paths)?;
    match command {
        TranscribeCommand::Start { output, follow } => {
            start(&session, output)?;
            if follow {
                follow_transcript(&session)?;
            }
            Ok(())
        }
        TranscribeCommand::Follow => follow_transcript(&session),
        TranscribeCommand::Stop => stop(&session),
        TranscribeCommand::Status => {
            let status = session.system_transcription_status()?;
            match status.error {
                _ if status.running => {
                    println!("transcribing -> {}", status.transcript.display());
                }
                None => println!("not transcribing"),
                Some(failure) => {
                    println!("not transcribing; the last transcription failed: {failure}");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        TranscribeCommand::Toggle { output } => {
            if session.system_transcription_status()?.running {
                stop(&session)
            } else {
                start(&session, output)
            }
        }
    }
}

fn start(session: &DaemonSession, output: Option<std::path::PathBuf>) -> anyhow::Result<()> {
    let started = session.system_transcription_start(output)?;
    let already = if started.already_running {
        "already "
    } else {
        ""
    };
    println!("{already}transcribing -> {}", started.transcript.display());
    Ok(())
}

/// Streams the running session's segments here as they finalize, until
/// Ctrl-C or stop. Labels are the tentative live ones; the transcript
/// file's final rewrite may settle on others.
fn follow_transcript(session: &DaemonSession) -> anyhow::Result<()> {
    let follow = session.follow_system_transcription()?;
    let transcript_path = follow.transcript.display().to_string();

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
            SystemTranscriptionStage::Started => {}
            SystemTranscriptionStage::SegmentTranscribed {
                text,
                label,
                at_seconds,
            } => {
                let now = std::time::Instant::now();
                let label = label.as_deref().map(str::to_owned);
                let mut stdout = std::io::stdout().lock();
                if open_label.as_ref() != Some(&label)
                    || now.duration_since(last_segment)
                        >= aria_voice::system_transcription::GAP_BREAK
                {
                    if open_label.is_some() {
                        writeln!(stdout)?;
                        writeln!(stdout)?;
                    }
                    write!(
                        stdout,
                        "{} ",
                        aria_voice::system_transcription::stamp(at_seconds)
                    )?;
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
            SystemTranscriptionStage::SpeakerMerged { from, into } => {
                let mut stdout = std::io::stdout().lock();
                if open_label.is_some() {
                    writeln!(stdout)?;
                    writeln!(stdout)?;
                }
                writeln!(stdout, "-- {from} is {into}")?;
                stdout.flush()?;
                open_label = None;
            }
            SystemTranscriptionStage::Stopped => break,
            SystemTranscriptionStage::Failed(code) => {
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

fn stop(session: &DaemonSession) -> anyhow::Result<()> {
    let stopped = session.system_transcription_stop()?;
    if stopped.was_running {
        println!("stopped -> {}", stopped.transcript.display());
    } else {
        println!("nothing was transcribing");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn transcribe_commands_are_public_subcommands() {
        let parse = |arguments: &[&str]| {
            let cli = Cli::try_parse_from(arguments).expect("the transcribe command should parse");
            let Command::Transcribe { command } = cli.command else {
                panic!("a transcribe invocation should parse as Transcribe");
            };
            command
        };

        assert!(matches!(
            parse(&["aria", "transcribe", "start"]),
            TranscribeCommand::Start {
                output: None,
                follow: false
            }
        ));
        assert!(matches!(
            parse(&["aria", "transcribe", "start", "-o", "meeting.txt", "--follow"]),
            TranscribeCommand::Start { output: Some(path), follow: true } if path == std::path::Path::new("meeting.txt")
        ));
        assert!(matches!(
            parse(&["aria", "transcribe", "follow"]),
            TranscribeCommand::Follow
        ));
        assert!(matches!(
            parse(&["aria", "transcribe", "stop"]),
            TranscribeCommand::Stop
        ));
        assert!(matches!(
            parse(&["aria", "transcribe", "status"]),
            TranscribeCommand::Status
        ));
        assert!(matches!(
            parse(&["aria", "transcribe", "toggle"]),
            TranscribeCommand::Toggle { output: None }
        ));
    }
}
