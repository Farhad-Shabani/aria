use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use aria_daemon::{
    AskCall, AssistantId, ChatChoice, ChatReply, ChatRequest, ChatStageKind, Clarify,
    DaemonSession, ReplyError, Routing, StopMode, TakenAsk, Ticket,
};
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use clap::Args;

use super::runtime_paths;
use super::vocabulary::vocabulary_dir;
use super::{CliOutputMode, assistant_parser};
use crate::cli::{INTERRUPTED, Interrupt};

#[derive(Args)]
pub(super) struct AskArgs {
    /// The question. Omit it to record one, or pipe it on stdin
    pub(super) text: Option<String>,
    /// Bind recording vocabulary and the answering assistant to this directory
    #[arg(long)]
    pub(super) project: Option<PathBuf>,
    /// Override the shared assistant for this question
    #[arg(long = "assistant", value_parser = assistant_parser(), value_name = "ASSISTANT")]
    pub(super) assistant_id: Option<AssistantId>,
    /// Override the shared model choice for this question
    #[arg(long = "model", value_name = "MODEL_ID")]
    pub(super) model_id: Option<String>,
    /// Continue a chat by name, in the folder it runs in; this directory only seeds a new name. Defaults to this directory's chat
    #[arg(long, value_parser = |name: &str| aria_settings::ChatName::new(name), value_name = "NAME")]
    pub(super) chat: Option<aria_settings::ChatName>,
    /// Start a new assistant session in the chat instead of resuming the last one; history still shows the chat's earlier asks with it
    #[arg(long)]
    pub(super) new: bool,
    /// Print the ticket and return; the reply shows up in `aria events`
    #[arg(long)]
    pub(super) detach: bool,
    /// Deliver the reply like dictated text instead of printing it
    #[arg(long, value_enum)]
    pub(super) mode: Option<CliOutputMode>,
    /// Record until you press Esc, run the command again, or Auto-stop ends it
    #[arg(long)]
    pub(super) toggle: bool,
    /// Cancel a waiting reply by its ticket instead of asking
    #[arg(
        long,
        value_name = "TICKET",
        conflicts_with_all = ["text", "assistant_id", "model_id", "chat", "new", "detach", "mode", "toggle", "project"]
    )]
    pub(super) cancel: Option<Ticket>,
}

pub(super) fn run(args: AskArgs) -> anyhow::Result<()> {
    let paths = runtime_paths()?;
    let session = Arc::new(DaemonSession::connect_at(&paths)?);
    if let Some(ticket) = args.cancel {
        if session.cancel(ticket)? {
            println!("cancelled {ticket}");
        } else {
            println!("{ticket} is not pending");
        }
        return Ok(());
    }
    let assistant_id = match args.assistant_id {
        Some(assistant_id) => assistant_id,
        None => session.get_settings()?.settings.resolve_assistant_id(),
    };
    let project = args
        .project
        .map(|path| {
            let path = path
                .canonicalize()
                .context("failed to resolve the project directory")?;
            anyhow::ensure!(path.is_dir(), "the project path is not a directory");
            Ok::<_, anyhow::Error>(aria_settings::Directory::new(path)?)
        })
        .transpose()?;
    let stop_mode = if args.toggle {
        StopMode::Manual
    } else {
        StopMode::OnSilence
    };
    let mut interrupt = Interrupt::install(&session)?;
    let (text, spoken) = match args.text {
        Some(text) => (text, None),
        None if !std::io::stdin().is_terminal() => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .context("failed to read the question from stdin")?;
            (text, None)
        }
        None => {
            // The phase ends with the recording: routing may speak and
            // record again, and a stale stop would cut that short.
            let heard = {
                let mut recording = interrupt.recording();
                let context = if let Some(project) = &project {
                    aria_voice::vocabulary::Context::Directory(project.clone())
                } else if session.get_settings()?.settings.vocabulary_enabled {
                    match session.route(String::new(), Clarify::Refuse, assistant_id)? {
                        Routing::Ready { request, .. } => {
                            aria_voice::vocabulary::Context::Directory(vocabulary_dir(
                                request.workdir,
                            )?)
                        }
                        Routing::Stopped => return Ok(()),
                    }
                } else {
                    aria_voice::vocabulary::Context::Disabled
                };
                let heard =
                    session.transcribe_in(stop_mode, context, |stage| recording.follow(stage));
                if let Some(error) = recording.take_failure() {
                    eprintln!("warning: the recording could not be stopped: {error:#}");
                }
                heard?
            };
            match heard {
                Some(heard) => (heard.text, Some(heard.own_voice)),
                None => return Ok(()),
            }
        }
    };
    anyhow::ensure!(!text.trim().is_empty(), "nothing to ask");

    let clarify = if spoken.is_some() {
        Clarify::Aloud { stop_mode }
    } else {
        Clarify::Refuse
    };
    let (configured, focus) = if let Some(project) = project {
        let request = ChatRequest {
            workdir: project.as_path().to_owned(),
            ..chat_request_from_settings(text.clone(), assistant_id)?
        };
        (request, None)
    } else {
        match session.route(text.clone(), clarify, assistant_id) {
            Ok(Routing::Ready { request, focus }) => (request, focus),
            Ok(Routing::Stopped) => return Ok(()),
            // What was said is not lost with the routing that failed.
            Err(error) if spoken.is_some() => {
                eprintln!("note: you said: {text}");
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        }
    };
    let workdir = configured.workdir;
    let folder = workdir.display().to_string();
    let chat = match args.chat {
        Some(name) => ChatChoice::Named {
            name,
            workdir: Some(workdir),
            new_session: args.new,
        },
        None => ChatChoice::Folder {
            workdir,
            new_session: args.new,
        },
    };
    let call = AskCall {
        text: configured.text,
        chat,
        assistant_id,
        model_id: args.model_id,
        output_mode: args.mode.map(Into::into),
        own_voice: spoken.unwrap_or(false),
        tap_cancels: configured.tap_cancels,
    };
    let assistant_name = assistant_id.name();
    let pending = match session.ask(call) {
        Ok(pending) => pending,
        // What was said is not lost with the chat that failed.
        Err(error) if spoken.is_some() => {
            eprintln!("note: you said: {text}");
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    let pending_ticket = pending.ticket();
    // A named chat runs in its own folder, which may not be this one.
    let place = match pending.chat() {
        Some(chat) => format!("chat {}", chat.name.as_str()),
        None => folder,
    };
    if let Some(error) = focus.and_then(|focus| focus.store().err()) {
        eprintln!("warning: the project was not remembered: {error:#}");
    }
    eprintln!("asked {pending_ticket} {assistant_name} in {place}");
    if args.detach {
        println!("ticket {pending_ticket}");
        return Ok(());
    }

    let reply = wait_for_reply(pending, &mut interrupt)?;
    print_reply(&reply, args.mode);
    Ok(())
}

/// Prints a reply nothing typed: one with no mode, or one whose delivery
/// failed.
pub(super) fn print_reply(reply: &ChatReply, mode: Option<CliOutputMode>) {
    match reply.delivery_error {
        Some(code) => {
            let reason = code.words();
            eprintln!("warning: the reply was not delivered ({reason}); printing it instead");
            println!("{}", reply.text);
        }
        None if mode.is_none() => println!("{}", reply.text),
        None => {}
    }
}

/// Shows each step of the chat or prompt run until it ends. Esc or Ctrl-C
/// cancels it, and a cancelled one ends the process as Ctrl-C would.
pub(super) fn wait_for_reply(
    pending: TakenAsk,
    interrupt: &mut Interrupt,
) -> anyhow::Result<ChatReply> {
    let pending_ticket = pending.ticket();
    // Everything before prints first: an armed phase holds the terminal raw,
    // where a plain newline does not return the carriage.
    let mut working = interrupt.asking(pending_ticket);
    let reply = pending.reply(|stage| {
        let ticket = stage.ticket;
        match &stage.kind {
            ChatStageKind::Working(step) => {
                working.show(format!("{ticket} {} · {step}", stage.assistant_id.name()));
            }
            ChatStageKind::Replied(..) => {
                working.finish();
                eprintln!("{ticket} replied in {}s", working.seconds());
            }
            ChatStageKind::Failed {
                code: RpcErrorCode::Cancelled,
                ..
            } if working.cancelled() => {
                working.finish();
                eprintln!("{ticket} cancelled after {}s", working.seconds());
            }
            ChatStageKind::Failed { .. } => {
                working.finish();
                eprintln!("{ticket} failed after {}s", working.seconds());
            }
        }
    });
    if let Some(error) = working.take_failure() {
        eprintln!("warning: {pending_ticket} could not be cancelled: {error:#}");
    }
    match reply {
        Err(ReplyError::Failed(RpcFailure {
            code: RpcErrorCode::Cancelled,
            ..
        })) if working.cancelled() => std::process::exit(INTERRUPTED),
        reply => Ok(reply?),
    }
}

fn chat_request_from_settings(
    text: String,
    assistant_id: AssistantId,
) -> anyhow::Result<ChatRequest> {
    let home = aria_settings::config::home_directory()?;
    let settings = aria_settings::load(&home)?;
    let config = aria_daemon::config::load_chat(&settings);
    Ok(ChatRequest::from_config(text, assistant_id, config)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{Cli, Command};
    use clap::Parser;

    #[test]
    fn ask_accepts_each_assistant_and_selection_rejects_unknown_names() {
        for assistant_id in AssistantId::ALL {
            let name = assistant_id.name();
            assert!(matches!(
                Cli::try_parse_from(["aria", "ask", "--assistant", name, "hello"])
                    .unwrap()
                    .command,
                Command::Ask(AskArgs {
                    assistant_id: Some(_),
                    ..
                })
            ));
        }
        assert!(Cli::try_parse_from(["aria", "assistant", "unknown"]).is_err());
    }

    #[test]
    fn ask_takes_a_chat_name_only_when_it_is_valid() {
        let parsed = Cli::try_parse_from(["aria", "ask", "--chat", " planning ", "hello"]);
        assert!(matches!(
            parsed.unwrap().command,
            Command::Ask(AskArgs { chat: Some(chat), .. }) if chat.as_str() == "planning"
        ));
        for name in ["   ", "two\nlines", "/Users/me/proj"] {
            assert!(Cli::try_parse_from(["aria", "ask", "--chat", name, "hello"]).is_err());
        }
    }
}
