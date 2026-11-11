use std::io::IsTerminal;

use aria_daemon::{AssistantId, ClientError, DaemonSession};
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_settings::{ChatKey, ChatName, Quote};
use clap::{Args, Subcommand};

use super::runtime_paths;
use super::{answer, assistant_parser};

#[derive(Subcommand)]
pub(super) enum HistoryCommand {
    /// List saved history and recording availability, each chat once
    List {
        /// Print one JSON array instead of text
        #[arg(long)]
        json: bool,
        /// List one section only
        #[arg(long, value_enum)]
        section: Option<Section>,
        /// List only entries whose text, reply, quote, title, chat name or app holds this, ignoring case
        #[arg(long, value_name = "TEXT")]
        query: Option<String>,
    },
    /// Delete saved history and retained recordings
    #[command(
        long_about = "Delete saved history and retained recordings, in every section or one. This cannot be undone. History and audio-retention settings stay as they are, and a dictation in progress is not stopped."
    )]
    Clear {
        /// Skip confirmation (required for noninteractive use)
        #[arg(long)]
        yes: bool,
        /// Clear one section only
        #[arg(long, value_enum)]
        section: Option<Section>,
    },
    /// Name an entry, or remove its name
    Rename {
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
        /// Leave out to remove the name
        title: Option<String>,
    },
    /// Print an entry's text, and an ask's reply
    Show {
        /// The entry's ID, as `aria history list` prints it
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
        /// Print one JSON object instead of text
        #[arg(long)]
        json: bool,
    },
    /// Delete one entry, its prompt results, and its retained recording
    Delete {
        /// The entry's ID, as `aria history list` prints it
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
        /// Skip confirmation (required for noninteractive use)
        #[arg(long)]
        yes: bool,
    },
    /// List the prompt results saved under an entry, newest first
    Results {
        /// The entry's ID, as `aria history list` prints it
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
        /// Print one JSON array instead of text
        #[arg(long)]
        json: bool,
    },
    /// Transcribe a retained recording again and replace its saved text
    #[command(
        long_about = "Transcribe a retained recording again and replace its saved text. Uses the configured local or server speech recognition. Failure preserves the previous text."
    )]
    Retry {
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
        #[arg(long)]
        engine: Option<String>,
        #[arg(long)]
        language: Option<aria_voice::stt::Language>,
    },
    /// Show, rename, or delete a chat
    Chat {
        #[command(subcommand)]
        command: ChatCommand,
    },
}

#[derive(Subcommand)]
pub(super) enum ChatCommand {
    /// Print a chat's questions and replies, oldest first
    Show {
        #[command(flatten)]
        chat: ChatArgs,
        /// Print one JSON array instead of text
        #[arg(long)]
        json: bool,
    },
    /// Give a chat a new name, which `aria ask --chat` then continues
    Rename {
        #[command(flatten)]
        chat: ChatArgs,
        /// One line of up to 128 bytes, not starting with `/`, that no other
        /// chat of the assistant has
        #[arg(value_parser = |name: &str| ChatName::new(name))]
        new_name: ChatName,
    },
    /// Delete a chat's questions, replies, and prompt results
    Delete {
        #[command(flatten)]
        chat: ChatArgs,
        /// Skip confirmation (required for noninteractive use)
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args)]
pub(super) struct ChatArgs {
    /// The chat's name, as `aria history list` prints it
    #[arg(value_parser = |name: &str| ChatName::new(name))]
    name: ChatName,
    /// The assistant the chat runs with. Defaults to the shared one
    #[arg(long = "assistant", value_parser = assistant_parser(), value_name = "ASSISTANT")]
    assistant_id: Option<AssistantId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum Section {
    Dictation,
    Transcription,
    Chat,
}

impl From<Section> for aria_voice::history::HistorySection {
    fn from(section: Section) -> Self {
        match section {
            Section::Dictation => Self::Dictation,
            Section::Transcription => Self::SystemTranscription,
            Section::Chat => Self::Chat,
        }
    }
}

pub(super) fn run(command: HistoryCommand) -> anyhow::Result<()> {
    use aria_voice::history::{HistoryQuery, RetryOptions, Title};

    if let HistoryCommand::Clear { yes, section } = &command
        && !confirm_clear(*yes, *section, std::io::stdin().is_terminal(), answer)?
    {
        println!("History clear cancelled.");
        return Ok(());
    }
    if let HistoryCommand::Delete { yes, id } = &command
        && !confirm_delete(*yes, *id, std::io::stdin().is_terminal(), answer)?
    {
        println!("History delete cancelled.");
        return Ok(());
    }
    if let HistoryCommand::Chat {
        command: ChatCommand::Delete { yes, chat },
    } = &command
        && !confirm_chat_delete(*yes, &chat.name, std::io::stdin().is_terminal(), answer)?
    {
        println!("Chat delete cancelled.");
        return Ok(());
    }

    let paths = runtime_paths()?;
    let session = DaemonSession::connect_at(&paths)?;
    match command {
        HistoryCommand::Clear { section, .. } => {
            let result = session
                .clear_history(section.map(Into::into))
                .map_err(clear_error)?;
            println!("{}", clear_message(result)?);
        }
        HistoryCommand::List {
            json,
            section,
            query,
        } => {
            let mut json_entries = Vec::new();
            let mut before_id = None;
            loop {
                let page = session.list_history(HistoryQuery {
                    section: section.map(Into::into),
                    query: query.clone().unwrap_or_default(),
                    before_id,
                    limit: 100,
                    collapse_chats: true,
                })?;
                if let Some(warning) = page.warning {
                    eprintln!("warning: {warning}");
                }
                if !json && before_id.is_none() && page.entries.is_empty() {
                    println!("no history");
                }
                for entry in page.entries {
                    if json {
                        json_entries.push(entry_json(&entry));
                    } else {
                        println!(
                            "{}\t{}\t{:?}\taudio={}\t{}",
                            entry.id,
                            entry.content.section().as_str(),
                            entry.status,
                            audio_label(entry.audio()),
                            row_text(&entry)
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
        HistoryCommand::Rename { id, title } => {
            let title = title.as_deref().map(Title::new).transpose()?;
            let entry = session.rename_history(id, title.as_ref())?;
            match entry.title {
                Some(title) => println!("Renamed {id} to {}.", title.as_str()),
                None => println!("Removed the name from {id}."),
            }
        }
        HistoryCommand::Show { id, json } => {
            let entry = session.get_history(id)?;
            if let Some(warning) = &entry.warning {
                eprintln!("warning: {warning}");
            }
            if let Some(notice) = transcript_notice(&entry.content) {
                eprintln!("warning: {notice}");
            }
            let reply = reply(&entry);
            if json {
                let mut object = entry_json(&entry);
                object["reply"] = reply.into();
                println!("{object}");
            } else {
                println!("{}", entry.text);
                if let Some(reply) = reply {
                    println!("\n{reply}");
                }
            }
        }
        HistoryCommand::Delete { id, .. } => {
            session.delete_history(id)?;
            println!("Deleted {id}.");
        }
        HistoryCommand::Results { id, json } => {
            let results = session.history_thread(id)?;
            if json {
                let results: Vec<serde_json::Value> = results.iter().map(result_json).collect();
                println!("{}", serde_json::Value::Array(results));
            } else if results.is_empty() {
                println!("no prompt results");
            } else {
                let blocks: Vec<String> = results
                    .iter()
                    .map(|result| format!("{}\t{}\n{}", result.id, result.prompt_name, result.text))
                    .collect();
                println!("{}", blocks.join("\n\n"));
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
        HistoryCommand::Chat { command } => run_chat(&session, command)?,
    }
    Ok(())
}

fn run_chat(session: &DaemonSession, command: ChatCommand) -> anyhow::Result<()> {
    let (ChatCommand::Show { chat, .. }
    | ChatCommand::Rename { chat, .. }
    | ChatCommand::Delete { chat, .. }) = &command;
    let name = chat.name.as_str().to_owned();
    let key = ChatKey {
        assistant_id: match chat.assistant_id {
            Some(assistant_id) => assistant_id,
            None => session.get_settings()?.settings.resolve_assistant_id(),
        },
        name: chat.name.clone(),
    };
    match command {
        ChatCommand::Show { json, .. } => {
            let (asks, quote) = chat_asks(session, key).map_err(|error| match error {
                ClientError::Rpc(RpcFailure {
                    code: RpcErrorCode::HistoryMissing,
                    ..
                }) => anyhow::anyhow!("no chat named {name} has saved asks"),
                error => error.into(),
            })?;
            if json {
                println!("{}", chat_json(&asks, quote.as_ref()));
            } else {
                let blocks: Vec<String> = quote
                    .iter()
                    .map(|quote| quote.as_str().to_owned())
                    .chain(asks.iter().map(|ask| {
                        format!(
                            "{}\t{}\n{}",
                            ask.id,
                            ask.text,
                            reply(ask).unwrap_or_default()
                        )
                    }))
                    .collect();
                println!("{}", blocks.join("\n\n"));
            }
        }
        ChatCommand::Rename { new_name, .. } => {
            let renamed = session.rename_chat(key, &new_name)?;
            println!("Renamed chat {name} to {}.", renamed.name.as_str());
        }
        ChatCommand::Delete { .. } => {
            let assistant = key.assistant_id.name();
            if session.delete_chat(key)? {
                println!("Deleted chat {name}.");
            } else {
                println!("No chat named {name} with {assistant}.");
            }
        }
    }
    Ok(())
}

/// Every saved ask in `chat`, oldest first, and the chat's quote.
fn chat_asks(
    session: &DaemonSession,
    chat: ChatKey,
) -> Result<(Vec<aria_voice::history::HistoryEntry>, Option<Quote>), ClientError> {
    use aria_voice::history::ChatQuery;

    let mut asks = Vec::new();
    let mut before_id = None;
    loop {
        let page = session.get_chat(ChatQuery {
            chat: chat.clone(),
            before_id,
            limit: ChatQuery::MAX_LIMIT,
        })?;
        // Pages run newest to oldest, and each one is oldest first.
        asks.splice(0..0, page.entries);
        let Some(next) = page.next_before_id else {
            return Ok((asks, page.quote));
        };
        if before_id.is_some_and(|previous| next >= previous) {
            return Err(ClientError::MalformedResponse(
                "chat pagination did not advance".to_owned(),
            ));
        }
        before_id = Some(next);
    }
}

fn reply(entry: &aria_voice::history::HistoryEntry) -> Option<&str> {
    use aria_voice::history::HistoryContent;
    match &entry.content {
        HistoryContent::Ask { reply, .. } => Some(reply),
        HistoryContent::Dictation(_) | HistoryContent::SystemTranscription { .. } => None,
    }
}

fn row_text(entry: &aria_voice::history::HistoryEntry) -> String {
    let text = entry.text.replace(['\n', '\r', '\t'], " ");
    match entry.content.chat() {
        Some(chat) => format!("[{}] {text}", chat.name.as_str()),
        None => text,
    }
}

fn confirm_clear(
    yes: bool,
    section: Option<Section>,
    interactive: bool,
    read_answer: impl FnOnce(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    let scope = match section {
        None => "all history",
        Some(Section::Dictation) => "dictations",
        Some(Section::Transcription) => "transcripts",
        Some(Section::Chat) => "chats",
    };
    confirm(
        yes,
        interactive,
        "clearing history",
        &format!(
            "Clear {scope} and retained recordings? This cannot be undone. History and audio-retention settings stay as they are; a dictation in progress is not stopped. [y/N] ",
        ),
        read_answer,
    )
}

fn confirm_chat_delete(
    yes: bool,
    session: &ChatName,
    interactive: bool,
    read_answer: impl FnOnce(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    confirm(
        yes,
        interactive,
        "deleting a chat",
        &format!(
            "Delete chat {}, its questions, replies, and prompt results? This cannot be undone. [y/N] ",
            session.as_str()
        ),
        read_answer,
    )
}

fn confirm_delete(
    yes: bool,
    id: u64,
    interactive: bool,
    read_answer: impl FnOnce(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    confirm(
        yes,
        interactive,
        "deleting a history entry",
        &format!(
            "Delete entry {id}, its prompt results, and its recording? This cannot be undone. [y/N] "
        ),
        read_answer,
    )
}

/// `--yes`, else a terminal's answer. Without a terminal, `action` is
/// refused rather than taken.
fn confirm(
    yes: bool,
    interactive: bool,
    action: &str,
    question: &str,
    read_answer: impl FnOnce(&str) -> anyhow::Result<String>,
) -> anyhow::Result<bool> {
    if yes {
        return Ok(true);
    }
    anyhow::ensure!(
        interactive,
        "{action} requires confirmation; pass --yes for noninteractive use"
    );
    let answer = read_answer(question)?;
    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn clear_message(result: aria_voice::history::ClearHistoryResult) -> anyhow::Result<String> {
    anyhow::ensure!(
        !result.cleanup_pending,
        "history text cleared, but some recordings could not be removed; Aria will retry"
    );
    Ok(format!(
        "History cleared ({} entries).",
        result.deleted_entries
    ))
}

fn clear_error(error: aria_daemon::ClientError) -> anyhow::Error {
    use aria_daemon::{ClientError, TransportFailure};
    let context = match &error {
        ClientError::Transport {
            kind: TransportFailure::Unimplemented,
            ..
        }
        | ClientError::Rpc(RpcFailure {
            code: RpcErrorCode::Unsupported,
            ..
        }) => "this daemon does not support clearing history; update Aria and restart the daemon",
        ClientError::Timeout(_)
        | ClientError::Transport { .. }
        | ClientError::MalformedResponse(_)
        | ClientError::Rpc(RpcFailure {
            code:
                RpcErrorCode::Cancelled
                | RpcErrorCode::OverallTimeout
                | RpcErrorCode::ShuttingDown
                | RpcErrorCode::HistoryUnavailable,
            ..
        }) => {
            "history clearing could not be confirmed; check history before retrying, as a retry also removes newer entries"
        }
        _ => return error.into(),
    };
    anyhow::Error::new(error).context(context)
}

fn chat_json(
    asks: &[aria_voice::history::HistoryEntry],
    quote: Option<&Quote>,
) -> serde_json::Value {
    let asks: Vec<serde_json::Value> = asks
        .iter()
        .map(|ask| {
            let mut object = entry_json(ask);
            object["reply"] = reply(ask).into();
            object
        })
        .collect();
    serde_json::json!({ "quote": quote.map(Quote::as_str), "asks": asks })
}

fn entry_json(entry: &aria_voice::history::HistoryEntry) -> serde_json::Value {
    serde_json::json!({
        "id": entry.id,
        "section": entry.content.section().as_str(),
        "title": entry.title.as_ref().map(|title| title.as_str()),
        "created_at_ms": entry.created_at_ms,
        "application": entry.application.as_ref().map(|application| &application.name),
        "status": entry.status.as_str(),
        "audio": entry.audio().as_str(),
        "result_count": entry.result_count,
        "text": entry.text,
        "chat": entry.content.chat().map(|chat| serde_json::json!({
            "assistant": chat.assistant_id.name(),
            "name": chat.name.as_str(),
        })),
    })
}

fn result_json(result: &aria_voice::history::PromptResult) -> serde_json::Value {
    serde_json::json!({
        "id": result.id,
        "created_at_ms": result.created_at_ms,
        "prompt_name": result.prompt_name,
        "text": result.text,
    })
}

fn audio_label(audio: aria_voice::history::AudioAvailability) -> &'static str {
    use aria_voice::history::AudioAvailability;
    match audio {
        AudioAvailability::Available { .. } => "Available",
        AudioAvailability::Expired => "Expired",
        AudioAvailability::Unavailable => "Unavailable",
    }
}

/// Says when the text is less than the whole transcript.
fn transcript_notice(content: &aria_voice::history::HistoryContent) -> Option<String> {
    use aria_voice::history::{HistoryContent, TranscriptRead};

    let HistoryContent::SystemTranscription { transcript, read } = content else {
        return None;
    };
    let path = transcript.display();
    match read {
        Some(TranscriptRead::Truncated) => Some(format!(
            "showing the start of a longer transcript; the whole one is at {path}"
        )),
        Some(TranscriptRead::Unavailable) => Some(format!(
            "couldn't read the transcript at {path}; showing its saved excerpt"
        )),
        Some(TranscriptRead::Whole) | None => None,
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
                Command::History { command: HistoryCommand::Clear { yes, section: None } } if yes == expected
            ));
        }
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "list"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::List {
                    json: false,
                    section: None,
                    query: None,
                }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "list", "--query", "weekly plan"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::List { query: Some(query), .. }
            } if query == "weekly plan"
        ));
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "clear", "--section", "chat"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::Clear {
                    yes: false,
                    section: Some(Section::Chat)
                }
            }
        ));
        assert!(Cli::try_parse_from(["aria", "history", "list", "--section", "mail"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "rename", "42", "Weekly plan"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::Rename { id: 42, title: Some(title) }
            } if title == "Weekly plan"
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
                    id: 42, engine: Some(engine), language: Some(aria_voice::stt::Language::Automatic)
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
    fn an_entry_is_shown_deleted_and_its_results_listed_by_id() {
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "show", "42", "--json"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::Show { id: 42, json: true }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "delete", "42"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::Delete { id: 42, yes: false }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "delete", "42", "--yes"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::Delete { id: 42, yes: true }
            }
        ));
        assert!(matches!(
            Cli::try_parse_from(["aria", "history", "results", "42"])
                .unwrap()
                .command,
            Command::History {
                command: HistoryCommand::Results {
                    id: 42,
                    json: false
                }
            }
        ));
        for arguments in [
            vec!["aria", "history", "show"],
            vec!["aria", "history", "delete", "0"],
            vec!["aria", "history", "results", "-1"],
        ] {
            assert!(Cli::try_parse_from(&arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn a_chat_is_shown_renamed_and_deleted_by_its_name() {
        let parsed = |arguments: &[&str]| Cli::try_parse_from(arguments).map(|cli| cli.command);
        assert!(matches!(
            parsed(&["aria", "history", "chat", "show", "trip", "--assistant", "codex", "--json"]).unwrap(),
            Command::History {
                command: HistoryCommand::Chat {
                    command: ChatCommand::Show {
                        chat: ChatArgs { name, assistant_id: Some(AssistantId::Codex) },
                        json: true,
                    }
                }
            } if name.as_str() == "trip"
        ));
        assert!(matches!(
            parsed(&["aria", "history", "chat", "rename", "trip", "Weekend"]).unwrap(),
            Command::History {
                command: HistoryCommand::Chat {
                    command: ChatCommand::Rename {
                        chat: ChatArgs { assistant_id: None, .. },
                        new_name,
                    }
                }
            } if new_name.as_str() == "Weekend"
        ));
        assert!(matches!(
            parsed(&["aria", "history", "chat", "delete", "trip", "--yes"]).unwrap(),
            Command::History {
                command: HistoryCommand::Chat {
                    command: ChatCommand::Delete { yes: true, .. }
                }
            }
        ));
        for arguments in [
            vec!["aria", "history", "chat", "show"],
            vec!["aria", "history", "chat", "show", "/a/folder"],
            vec!["aria", "history", "chat", "rename", "trip"],
            vec!["aria", "history", "chat", "rename", "trip", "/a/folder"],
        ] {
            assert!(parsed(&arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn deleting_a_chat_requires_explicit_confirmation() {
        let trip = ChatName::new("trip").unwrap();
        for response in ["y", "yes", "", "n"] {
            assert_eq!(
                confirm_chat_delete(false, &trip, true, |question| {
                    assert!(question.contains("trip"), "{question}");
                    Ok(response.into())
                })
                .unwrap(),
                matches!(response, "y" | "yes")
            );
        }
        let error =
            confirm_chat_delete(false, &trip, false, |_| panic!("must not prompt")).unwrap_err();
        assert!(error.to_string().contains("--yes"));
    }

    #[test]
    fn an_ask_in_a_named_chat_lists_under_its_name_and_its_chats_json_carries_the_quote() {
        use aria_voice::history::{HistoryContent, HistoryEntry, HistoryStatus};
        let ask = |name: Option<&str>| HistoryEntry {
            id: 7,
            title: None,
            content: HistoryContent::Ask {
                reply: "Friday.".to_owned(),
                chat: name.map(|name| ChatKey {
                    assistant_id: AssistantId::Codex,
                    name: ChatName::new(name).unwrap(),
                }),
            },
            created_at_ms: 1,
            duration_ms: None,
            source: aria_voice::history::HistorySource::Local,
            application: None,
            project: None,
            text: "When\tnext?".to_owned(),
            complete: true,
            result_count: 0,
            status: HistoryStatus::Success,
            error: None,
            warning: None,
            deletion_pending: false,
        };

        assert_eq!(row_text(&ask(Some("Trip"))), "[Trip] When next?");
        assert_eq!(row_text(&ask(None)), "When next?");
        assert_eq!(
            entry_json(&ask(Some("Trip")))["chat"],
            serde_json::json!({"assistant": "codex", "name": "Trip"})
        );
        assert!(entry_json(&ask(None))["chat"].is_null());
        let quote = Quote::new("the notes".to_owned()).unwrap();
        let shown = chat_json(&[ask(Some("Trip"))], Some(&quote));
        assert_eq!(shown["quote"], "the notes");
        assert_eq!(shown["asks"][0]["reply"], "Friday.");
        assert!(chat_json(&[], None)["quote"].is_null());
    }

    #[test]
    fn deleting_an_entry_requires_explicit_confirmation() {
        for response in ["y", "yes", "", "n", "no"] {
            assert_eq!(
                confirm_delete(false, 42, true, |question| {
                    assert!(question.contains("42"), "{question}");
                    Ok(response.into())
                })
                .unwrap(),
                matches!(response, "y" | "yes")
            );
        }
        let error = confirm_delete(false, 42, false, |_| panic!("must not prompt")).unwrap_err();
        assert!(error.to_string().contains("--yes"));
        assert!(confirm_delete(true, 42, false, |_| panic!("must not prompt")).unwrap());
    }

    #[test]
    fn history_clear_requires_explicit_confirmation() {
        for response in ["y", "yes", "", "n", "no", "yep"] {
            assert_eq!(
                confirm_clear(false, None, true, |_| Ok(response.into())).unwrap(),
                matches!(response, "y" | "yes")
            );
        }
        assert!(confirm_clear(false, None, true, |_| anyhow::bail!("input failed")).is_err());
        let error = confirm_clear(false, None, false, |_| panic!("must not prompt")).unwrap_err();
        assert!(error.to_string().contains("--yes"));
        for interactive in [true, false] {
            assert!(confirm_clear(true, None, interactive, |_| panic!("must not prompt")).unwrap());
        }
    }

    #[test]
    fn history_clear_reports_complete_partial_and_uncertain_outcomes() {
        use aria_daemon::ClientError;
        use aria_voice::history::ClearHistoryResult;

        for deleted_entries in [0, 42] {
            assert_eq!(
                clear_message(ClearHistoryResult {
                    deleted_entries,
                    cleanup_pending: false,
                })
                .unwrap(),
                format!("History cleared ({deleted_entries} entries).")
            );
        }
        let partial = clear_message(ClearHistoryResult {
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
            ClientError::Transport {
                kind: aria_daemon::TransportFailure::Unimplemented,
                message: "unknown method".into(),
            },
            ClientError::Rpc(RpcFailure::new(RpcErrorCode::Unsupported, "unsupported")),
        ] {
            assert!(
                clear_error(error)
                    .to_string()
                    .contains("update Aria and restart")
            );
        }
        for error in [
            ClientError::Timeout(std::time::Duration::from_secs(10)),
            ClientError::Transport {
                kind: aria_daemon::TransportFailure::Unavailable,
                message: "disconnected".into(),
            },
        ]
        .into_iter()
        .chain(
            [
                RpcErrorCode::Cancelled,
                RpcErrorCode::OverallTimeout,
                RpcErrorCode::ShuttingDown,
                RpcErrorCode::HistoryUnavailable,
            ]
            .map(|code| ClientError::Rpc(RpcFailure::new(code, "operation interrupted"))),
        ) {
            assert!(
                clear_error(error)
                    .to_string()
                    .contains("could not be confirmed")
            );
        }
        let busy = clear_error(ClientError::Rpc(RpcFailure::new(
            RpcErrorCode::Busy,
            "a history retry is active",
        )));
        assert!(busy.to_string().contains("a history retry is active"));
    }

    #[test]
    fn history_listing_distinguishes_audio_states() {
        use aria_voice::history::AudioAvailability;
        assert_eq!(
            audio_label(AudioAvailability::Available { expires_at_ms: 1 }),
            "Available"
        );
        assert_eq!(audio_label(AudioAvailability::Expired), "Expired");
        assert_eq!(audio_label(AudioAvailability::Unavailable), "Unavailable");
    }

    #[test]
    fn show_warns_when_the_text_is_not_the_whole_transcript() {
        use aria_voice::history::{HistoryContent, TranscriptRead};
        let system_transcription = |read| HistoryContent::SystemTranscription {
            transcript: "/sessions/talk.md".into(),
            read: Some(read),
        };

        let truncated =
            transcript_notice(&system_transcription(TranscriptRead::Truncated)).expect("notice");
        let unavailable =
            transcript_notice(&system_transcription(TranscriptRead::Unavailable)).expect("notice");

        assert!(
            truncated.contains("start of a longer transcript")
                && truncated.contains("/sessions/talk.md")
        );
        assert!(unavailable.contains("saved excerpt") && unavailable.contains("/sessions/talk.md"));
        assert_eq!(
            transcript_notice(&system_transcription(TranscriptRead::Whole)),
            None
        );
    }
}
