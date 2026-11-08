//! The `aria` command line: `Cli` parses it and `run` sends each command to its module.

use anyhow::Context;
use aria_daemon::{AssistantId, OutputMode};
use aria_platform::runtime::PreparedRuntimePaths;
use aria_settings::RunsOn;
use clap::{Parser, Subcommand, ValueEnum};

mod ask;
mod assistant;
mod daemon;
mod dictate;
mod doctor;
mod events;
mod history;
mod logging;
mod memory;
mod models;
mod prompt;
mod remote;
mod say;
mod service;
mod settings;
mod setup;
mod speaker;
mod transcribe;
mod vocabulary;
mod wake;

#[derive(Parser)]
#[command(name = "aria", version, about = "Aria local voice assistant")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    Daemon,
    #[command(about = "Host speech recognition for paired devices without opening audio devices")]
    Serve {
        #[arg(long)]
        bind: std::net::SocketAddr,
    },
    #[command(about = "Pair with or administer an inference server")]
    Remote {
        #[command(subcommand)]
        command: remote::RemoteCommand,
    },
    #[cfg(target_os = "macos")]
    #[command(name = aria_platform::accessibility::CHECK_SUBCOMMAND, hide = true)]
    AccessibilityCheck,
    /// Check the settings, models, and daemon on this machine
    Doctor,
    /// Read, change and check settings.toml
    Settings {
        #[command(subcommand)]
        command: settings::SettingsCommand,
    },
    /// Watch the running daemon, one line per dictation stage
    Events,
    /// Manage saved history and retained recordings
    History {
        #[command(subcommand)]
        command: history::HistoryCommand,
    },
    /// List, write, and remove the prompts Aria runs on what you say
    Prompt {
        #[command(subcommand)]
        command: prompt::PromptCommand,
    },
    /// Inspect what the assistant remembers and the words it corrects
    Memory {
        #[command(subcommand)]
        command: memory::MemoryCommand,
    },
    /// Check, refresh and forget project spellings for dictation
    Vocabulary {
        #[command(subcommand)]
        command: vocabulary::VocabularyCommand,
    },
    /// Run or inspect hands-free wake-word dictation
    Wake {
        #[command(subcommand)]
        command: wake::WakeCommand,
    },
    /// Pick engines, fetch their models, and register the daemon
    #[command(args_conflicts_with_subcommands = true)]
    Setup {
        #[command(subcommand)]
        command: Option<setup::SetupCommand>,
        /// Take every prompt's default
        #[arg(long)]
        yes: bool,
        /// Leave the speech models for later
        #[arg(long)]
        skip_models: bool,
        /// Set the speech model, keeping every other choice and downloading
        /// only its files
        #[arg(long, value_name = "MODEL", conflicts_with_all = ["yes", "skip_models"])]
        stt: Option<String>,
        /// Recognize speech on the paired server, keeping the local model and its
        /// settings for later
        #[arg(long, conflicts_with_all = ["yes", "skip_models", "stt", "local"])]
        server: bool,
        /// Recognize speech on this machine again
        #[arg(long, conflicts_with_all = ["yes", "skip_models", "stt"])]
        local: bool,
    },
    /// See and remove the models setup downloaded
    Models {
        #[command(subcommand)]
        command: models::ModelsCommand,
    },
    /// Keep the daemon running across logins
    Service {
        #[command(subcommand)]
        command: service::ServiceCommand,
    },
    /// Record one utterance and deliver its text
    Dictate {
        #[arg(long, value_enum)]
        mode: Option<CliOutputMode>,
        /// Record until you press Esc, run the command again, or Auto-stop ends it
        #[arg(long)]
        toggle: bool,
        /// Insert finalized segments while recording, where the output and model can stream
        #[arg(long)]
        stream: bool,
        /// Disable project spellings for this recording
        #[arg(long)]
        no_project: bool,
        /// Save the text to history without typing it
        #[arg(long, conflicts_with_all = ["mode", "stream"])]
        history_only: bool,
    },
    /// List what an assistant offers
    Assistant {
        #[command(subcommand)]
        command: assistant::AssistantCommand,
    },
    /// Ask an assistant a question and print its reply
    Ask(ask::AskArgs),
    /// Transcribe what this machine plays to a file
    Transcribe {
        #[command(subcommand)]
        command: transcribe::TranscribeCommand,
    },
    /// Enroll a voice so Aria can tell who is speaking
    Speaker {
        #[command(subcommand)]
        command: speaker::SpeakerCommand,
    },
    /// Speak text aloud; a new utterance replaces the one playing
    Say {
        /// What to say. Omit it to read from stdin, which keeps the text off
        /// this process's command line, where other users could read it
        #[arg(conflicts_with = "stop")]
        text: Option<String>,
        /// An engine voice name, such as "Samantha"
        #[arg(long, short, conflicts_with = "stop")]
        voice: Option<String>,
        /// A speaking-rate multiplier, 0.25 to 4
        #[arg(long, short, conflicts_with = "stop")]
        rate: Option<f32>,
        /// Stop the utterance playing instead of speaking one
        #[arg(long)]
        stop: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum CliOutputMode {
    Insert,
    InsertAndSubmit,
    InsertOrCopy,
    Copy,
}

fn assistant_parser() -> impl clap::builder::TypedValueParser<Value = AssistantId> {
    use clap::builder::{PossibleValuesParser, TypedValueParser};

    PossibleValuesParser::new(AssistantId::ALL.iter().map(|id| id.name()))
        .try_map(|name| name.parse::<AssistantId>())
}

impl From<CliOutputMode> for OutputMode {
    fn from(mode: CliOutputMode) -> Self {
        match mode {
            CliOutputMode::Insert => Self::Insert,
            CliOutputMode::InsertAndSubmit => Self::InsertAndSubmit,
            CliOutputMode::InsertOrCopy => Self::InsertOrCopy,
            CliOutputMode::Copy => Self::Copy,
        }
    }
}

pub fn run(cli: Cli) -> anyhow::Result<()> {
    let checking = matches!(
        cli.command,
        Command::Daemon
            | Command::Settings {
                command: settings::SettingsCommand::Check { .. }
            }
    );
    if !checking && let Some(banner) = aria_settings::command::banner() {
        eprintln!("{banner}");
    }
    match cli.command {
        Command::Daemon => daemon::run(),
        Command::Serve { bind } => remote::serve(bind),
        Command::Remote { command } => remote::run(command),
        #[cfg(target_os = "macos")]
        Command::AccessibilityCheck => {
            std::process::exit(aria_platform::accessibility::Trust::in_process().exit_code())
        }
        Command::Doctor => doctor::run(),
        Command::Settings { command } => settings::run(command),
        Command::Events => events::run(),
        Command::History { command } => history::run(command),
        Command::Prompt { command } => prompt::run(command),
        Command::Memory { command } => memory::run(command),
        Command::Vocabulary { command } => vocabulary::run(command),
        Command::Wake {
            command: wake::WakeCommand::Run,
        } => wake::run(),
        Command::Wake {
            command: wake::WakeCommand::Status,
        } => wake::status(),
        Command::Wake {
            command: wake::WakeCommand::Log,
        } => wake::log(),
        Command::Service { command } => service::run(command),
        Command::Setup {
            command: None,
            stt: Some(engine),
            ..
        } => setup::set_transcription_engine(&engine),
        Command::Setup {
            command: None,
            server: true,
            ..
        } => setup::set_transcription_runs_on(RunsOn::Server),
        Command::Setup {
            command: None,
            local: true,
            ..
        } => setup::set_transcription_runs_on(RunsOn::Local),
        Command::Setup {
            command: None,
            yes,
            skip_models,
            stt: None,
            server: false,
            local: false,
        } => setup::run(yes, skip_models),
        Command::Setup {
            command: Some(command),
            ..
        } => setup::run_for_app(command),
        Command::Models { command } => models::run(command),
        Command::Dictate {
            mode,
            toggle,
            stream,
            no_project,
            history_only,
        } => dictate::run(mode, toggle, stream, no_project, history_only),
        Command::Assistant {
            command: assistant::AssistantCommand::Models { assistant_id, json },
        } => assistant::models(assistant_id, json),
        Command::Ask(args) => ask::run(args),
        Command::Transcribe { command } => transcribe::run(command),
        Command::Speaker { command } => speaker::run(command),
        Command::Say {
            text,
            voice,
            rate,
            stop,
        } => say::run(text, voice, rate, stop),
    }
}

fn runtime_paths() -> anyhow::Result<PreparedRuntimePaths> {
    PreparedRuntimePaths::for_current_user().context("failed to prepare daemon runtime paths")
}

/// One line from the terminal, trimmed and lowercased.
fn answer(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_setting_commands_are_gone() {
        for args in [
            vec!["aria", "phone", "enable"],
            vec!["aria", "phone", "disable"],
            vec!["aria", "history", "retain-audio", "true"],
            vec!["aria", "vocabulary", "use", "/tmp"],
            vec!["aria", "vocabulary", "disable"],
            vec!["aria", "dictate", "auto-stop", "off"],
            vec!["aria", "dictate", "auto-stop"],
            vec!["aria", "memory", "enable"],
            vec!["aria", "wake", "enable"],
            vec!["aria", "config", "show"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
    }
}
