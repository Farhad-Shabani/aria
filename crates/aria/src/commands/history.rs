use std::io::IsTerminal;

use aria_daemon::DaemonSession;
use aria_settings::rpc::RpcErrorCode;
use clap::Subcommand;

use super::answer;
use super::runtime_paths;

#[derive(Subcommand)]
pub(super) enum HistoryCommand {
    /// List saved dictations and recording availability
    List {
        /// Print one JSON array instead of text
        #[arg(long)]
        json: bool,
    },
    /// Delete all saved dictations and retained recordings
    #[command(
        long_about = "Delete all saved dictations and retained recordings. This cannot be undone. History and audio-retention settings stay as they are, and a dictation in progress is not stopped."
    )]
    Clear {
        /// Skip confirmation (required for noninteractive use)
        #[arg(long)]
        yes: bool,
    },
    /// Transcribe a retained recording and replace its saved text
    #[command(
        long_about = "Transcribe a retained recording and replace its saved text. Uses the configured local or server transcription backend. Failure preserves the previous text."
    )]
    Retry {
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
        #[arg(long)]
        engine: Option<String>,
        #[arg(long)]
        language: Option<aria_voice::transcription::Language>,
    },
}

pub(super) fn run(command: HistoryCommand) -> anyhow::Result<()> {
    use aria_voice::history::{HistoryQuery, RetryOptions};

    if let HistoryCommand::Clear { yes } = &command
        && !confirm_history_clear(*yes, std::io::stdin().is_terminal(), answer)?
    {
        println!("History clear cancelled.");
        return Ok(());
    }

    let paths = runtime_paths()?;
    let session = DaemonSession::connect_at(&paths)?;
    match command {
        HistoryCommand::Clear { .. } => {
            let result = session.clear_history().map_err(history_clear_error)?;
            println!("{}", history_clear_message(result)?);
        }
        HistoryCommand::List { json } => {
            let mut json_entries = Vec::new();
            let mut before_id = None;
            loop {
                let page = session.list_history(HistoryQuery {
                    query: String::new(),
                    before_id,
                    limit: 100,
                })?;
                if let Some(warning) = page.warning {
                    eprintln!("warning: {warning}");
                }
                if !json && before_id.is_none() && page.entries.is_empty() {
                    println!("no dictation history");
                }
                for entry in page.entries {
                    if json {
                        json_entries.push(history_entry_json(&entry));
                    } else {
                        println!(
                            "{}\t{:?}\taudio={}\t{}",
                            entry.id,
                            entry.status,
                            history_audio_label(entry.audio),
                            entry.text.replace(['\n', '\r', '\t'], " ")
                        );
                    }
                    if let Some(warning) = entry.warning {
                        eprintln!("warning: {}: {warning}", entry.id);
                    }
                }
                let Some(next) = page.next_before_id else {
                    break;
                };
                anyhow::ensure!(
                    before_id.is_none_or(|previous| next < previous),
                    "history pagination did not advance"
                );
                before_id = Some(next);
            }
            if json {
                println!("{}", serde_json::Value::Array(json_entries));
            }
        }
        HistoryCommand::Retry {
            id,
            engine,
            language,
        } => {
            let entry = session.retry_history(id, RetryOptions { engine, language })?;
            println!("{}", entry.text);
            if let Some(warning) = entry.warning {
                eprintln!("warning: {warning}");
            }
        }
    }
    Ok(())
}

fn confirm_history_clear(
    yes: bool,
    interactive: bool,
    read_answer: impl FnOnce(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    if yes {
        return Ok(true);
    }
    anyhow::ensure!(
        interactive,
        "clearing history requires confirmation; pass --yes for noninteractive use"
    );
    let answer = read_answer(
        "Clear all dictation history and retained recordings? This cannot be undone. History and audio-retention settings stay as they are; a dictation in progress is not stopped. [y/N] ",
    )?;
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn history_clear_message(
    result: aria_voice::history::ClearHistoryResult,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        !result.cleanup_pending,
        "dictation text cleared, but some recordings could not be removed; Aria will retry"
    );
    Ok(format!(
        "Dictation history cleared ({} entries).",
        result.deleted_entries
    ))
}

fn history_clear_error(error: aria_daemon::ClientError) -> anyhow::Error {
    use aria_daemon::ClientError;
    let context = match &error {
        ClientError::Status(status) if status.code() == tonic::Code::Unimplemented => {
            "this daemon does not support clearing history; update Aria and restart the daemon"
        }
        ClientError::Rpc {
            code: RpcErrorCode::Unsupported,
            ..
        } => "this daemon does not support clearing history; update Aria and restart the daemon",
        ClientError::Timeout(_)
        | ClientError::Status(_)
        | ClientError::MalformedResponse(_)
        | ClientError::Rpc {
            code:
                RpcErrorCode::Cancelled
                | RpcErrorCode::OverallTimeout
                | RpcErrorCode::ShuttingDown
                | RpcErrorCode::HistoryUnavailable,
            ..
        } => {
            "history clearing could not be confirmed; check history before retrying, as a retry also removes newer dictations"
        }
        _ => return error.into(),
    };
    anyhow::Error::new(error).context(context)
}

fn history_entry_json(entry: &aria_voice::history::HistoryEntry) -> serde_json::Value {
    serde_json::json!({
        "id": entry.id,
        "created_at_ms": entry.created_at_ms,
        "application": entry.application.as_ref().map(|application| &application.name),
        "status": entry.status.as_str(),
        "audio": entry.audio.as_str(),
        "text": entry.text,
    })
}

fn history_audio_label(audio: aria_voice::history::AudioAvailability) -> &'static str {
    use aria_voice::history::AudioAvailability;
    match audio {
        AudioAvailability::Available { .. } => "Available",
        AudioAvailability::Expired => "Expired",
        AudioAvailability::Unavailable => "Unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn history_commands_parse_overrides_and_retention() {
        for (arguments, expected) in [
            (vec!["aria", "history", "clear"], false),
            (vec!["aria", "history", "clear", "--yes"], true),
        ] {
            assert!(matches!(
                Cli::try_parse_from(arguments).unwrap().command,
                Command::History { command: HistoryCommand::Clear { yes } } if yes == expected
            ));
        }
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "list"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::List { json: false }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "retry", "42"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::Retry {
                    id: 42,
                    engine: None,
                    language: None
                }
            }
        ));
        let command = Cli::try_parse_from([
            "aria",
            "history",
            "retry",
            "42",
            "--engine",
            "whisper",
            "--language",
            "auto",
        ])
        .unwrap()
        .command;
        assert!(matches!(
            command,
            Command::History {
                command: HistoryCommand::Retry {
                    id: 42, engine: Some(engine), language: Some(aria_voice::transcription::Language::Automatic)
                }
            } if engine == "whisper"
        ));
        for arguments in [
            vec!["aria", "history", "retry", "0"],
            vec!["aria", "history", "retry", "9223372036854775808"],
            vec!["aria", "history", "retry", "42", "--language", "fr_CA"],
        ] {
            assert!(Cli::try_parse_from(arguments).is_err());
        }
    }

    #[test]
    fn history_clear_requires_explicit_confirmation() {
        for response in ["y", "yes", "", "n", "no", "yep"] {
            assert_eq!(
                confirm_history_clear(false, true, |_| Ok(response.into())).unwrap(),
                matches!(response, "y" | "yes")
            );
        }
        assert!(confirm_history_clear(false, true, |_| anyhow::bail!("input failed")).is_err());
        let error = confirm_history_clear(false, false, |_| panic!("must not prompt")).unwrap_err();
        assert!(error.to_string().contains("--yes"));
        for interactive in [true, false] {
            assert!(
                confirm_history_clear(true, interactive, |_| panic!("must not prompt")).unwrap()
            );
        }
    }

    #[test]
    fn history_clear_reports_complete_partial_and_uncertain_outcomes() {
        use aria_daemon::ClientError;
        use aria_voice::history::ClearHistoryResult;

        for deleted_entries in [0, 42] {
            assert_eq!(
                history_clear_message(ClearHistoryResult {
                    deleted_entries,
                    cleanup_pending: false,
                })
                .unwrap(),
                format!("Dictation history cleared ({deleted_entries} entries).")
            );
        }
        let partial = history_clear_message(ClearHistoryResult {
            deleted_entries: 42,
            cleanup_pending: true,
        })
        .unwrap_err();
        assert!(
            partial
                .to_string()
                .contains("some recordings could not be removed")
        );
        for error in [
            ClientError::Status(tonic::Status::unimplemented("unknown method")),
            ClientError::Rpc {
                code: RpcErrorCode::Unsupported,
                message: "unsupported".into(),
            },
        ] {
            assert!(
                history_clear_error(error)
                    .to_string()
                    .contains("update Aria and restart")
            );
        }
        for error in [
            ClientError::Timeout(std::time::Duration::from_secs(10)),
            ClientError::Status(tonic::Status::unavailable("disconnected")),
        ]
        .into_iter()
        .chain(
            [
                RpcErrorCode::Cancelled,
                RpcErrorCode::OverallTimeout,
                RpcErrorCode::ShuttingDown,
                RpcErrorCode::HistoryUnavailable,
            ]
            .map(|code| ClientError::Rpc {
                code,
                message: "operation interrupted".into(),
            }),
        ) {
            assert!(
                history_clear_error(error)
                    .to_string()
                    .contains("could not be confirmed")
            );
        }
        let busy = history_clear_error(ClientError::Rpc {
            code: RpcErrorCode::Busy,
            message: "a history retry is active".into(),
        });
        assert!(busy.to_string().contains("a history retry is active"));
    }

    #[test]
    fn history_listing_distinguishes_audio_states() {
        use aria_voice::history::AudioAvailability;
        assert_eq!(
            history_audio_label(AudioAvailability::Available { expires_at_ms: 1 }),
            "Available"
        );
        assert_eq!(history_audio_label(AudioAvailability::Expired), "Expired");
        assert_eq!(
            history_audio_label(AudioAvailability::Unavailable),
            "Unavailable"
        );
    }
}
