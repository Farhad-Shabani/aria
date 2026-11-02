use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use aria_daemon::{
    AskError, AssistantId, ChatRequest, ChatStageKind, Clarify, DaemonSession, DeliveryMode,
    Routing, StopMode, Ticket,
};
use aria_platform::runtime::PreparedRuntimePaths;
use aria_settings::rpc::RpcErrorCode;

use super::CliOutputMode;
use super::vocabulary::vocabulary_dir;
use crate::cli::{INTERRUPTED, Interrupt};

pub(super) struct AskArgs {
    pub(super) text: Option<String>,
    pub(super) project: Option<PathBuf>,
    pub(super) assistant_id: Option<AssistantId>,
    pub(super) model_id: Option<String>,
    pub(super) session: Option<String>,
    pub(super) new: bool,
    pub(super) detach: bool,
    pub(super) mode: Option<CliOutputMode>,
    pub(super) toggle: bool,
    pub(super) cancel: Option<Ticket>,
}

pub(super) fn run(args: AskArgs) -> anyhow::Result<()> {
    let paths = PreparedRuntimePaths::for_current_user()
        .context("failed to prepare daemon runtime paths")?;
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
                let mut recording = interrupt.recording(DeliveryMode::Complete);
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
    let request = ChatRequest {
        assistant_id,
        model_id: args.model_id,
        session: args.session,
        new: args.new,
        output_mode: args.mode.map(Into::into),
        own_voice: spoken.unwrap_or(false),
        ..configured
    };
    let assistant_name = request.assistant_id.name();
    let workdir = request.workdir.display().to_string();
    let pending = match session.ask(request) {
        Ok(pending) => pending,
        // What was said is not lost with the ask that failed.
        Err(error) if spoken.is_some() => {
            eprintln!("note: you said: {text}");
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    let pending_ticket = pending.ticket();
    if let Some(error) = focus.and_then(|focus| focus.store().err()) {
        eprintln!("warning: the project was not remembered: {error:#}");
    }
    eprintln!("asked {pending_ticket} {assistant_name} in {workdir}");
    if args.detach {
        println!("ticket {pending_ticket}");
        return Ok(());
    }

    // Everything above prints first: an armed phase holds the terminal raw,
    // where a plain newline does not return the carriage.
    let mut working = interrupt.asking(pending_ticket);
    let reply = pending.reply(|stage| {
        let ticket = stage.ticket;
        match &stage.kind {
            ChatStageKind::Working(step) => {
                working.show(format!("{ticket} {assistant_name} · {step}"));
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
    let reply = match reply {
        Err(AskError::Failed {
            code: RpcErrorCode::Cancelled,
            ..
        }) if working.cancelled() => std::process::exit(INTERRUPTED),
        reply => reply?,
    };
    match reply.delivery_error {
        Some(code) => {
            let reason = code.words();
            eprintln!("warning: the reply was not delivered ({reason}); printing it instead");
            println!("{}", reply.text);
        }
        None if args.mode.is_none() => println!("{}", reply.text),
        None => {}
    }
    Ok(())
}

fn chat_request_from_settings(
    text: String,
    assistant_id: AssistantId,
) -> anyhow::Result<ChatRequest> {
    let home = aria_settings::config::home_directory()?;
    let settings = aria_settings::load(&home)?;
    let config = aria_daemon::config::load_chat(&settings)?;
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
                Command::Ask {
                    assistant_id: Some(_),
                    ..
                }
            ));
        }
        assert!(Cli::try_parse_from(["aria", "assistant", "unknown"]).is_err());
    }
}
