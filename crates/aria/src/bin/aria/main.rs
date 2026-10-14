use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use aria::daemon::guided::DEFAULT_SAMPLES;
use aria::daemon::{
    AskError, ChatAgent, ChatRequest, ChatStage, ChatStageKind, Clarify, Daemon, DaemonEvent,
    DaemonSession, DeliveryMode, DictationOutcome, DictationRequest, DictationStage,
    EnrollmentRequest, EnrollmentStage, ListenStage, OutputMode, Role, Routing, SpeakerName,
    SpeechOutcome, SpeechRequest, SpeechStage, StopMode, Ticket, WakeWordStatus, guided,
    open_log_writer, read_log_tail,
};
use aria::doctor::Status;
use aria::primitives::error_chain;
use aria::rpc::RpcErrorCode;
use aria::runtime::RuntimePaths;
use aria::setup::{ConfigWrite, Enrolled, Grant, Options as SetupOptions, TryOut};
use aria::supervisor::{DaemonState, Job, Status as ServiceStatus, Supervisor};
use aria::synthesis::{SpeakingRate, SpeechText, Voice};
use clap::{Parser, Subcommand, ValueEnum};

mod cli;

use cli::{INTERRUPTED, Interrupt};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer};

// The phonemizer logs each word it cannot read. Every dependency's log lines
// land in the daemon log, so the ones that see personal content are silenced.
const DEFAULT_LOG_FILTER: &str =
    "info,ort=warn,whisper_rs=warn,misaki_rs=off,espeak_rs=off,language_tokenizer=off";

#[derive(Parser)]
#[command(name = "aria", version, about = "Aria local voice assistant")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    Daemon,
    /// Check the configuration, models, and daemon on this machine
    Doctor,
    /// Inspect the configuration the daemon loads
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Watch the running daemon, one line per dictation stage
    Events,
    /// Manage saved dictations and retained recordings
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    /// Inspect what the assistant remembers and the words it corrects
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Select and refresh project spellings for dictation
    Vocabulary {
        #[command(subcommand)]
        command: VocabularyCommand,
    },
    /// Run or inspect hands-free wake-word dictation
    Wake {
        #[command(subcommand)]
        command: WakeCommand,
    },
    /// Pick engines, fetch their models, and register the daemon
    Setup {
        #[command(subcommand)]
        command: Option<SetupCommand>,
        /// Take every prompt's default
        #[arg(long)]
        yes: bool,
        /// Leave the speech models for later
        #[arg(long)]
        skip_models: bool,
    },
    /// Keep the daemon running across logins
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
    /// Record one utterance and deliver the transcript
    Dictate {
        #[arg(long, value_enum)]
        mode: Option<CliOutputMode>,
        /// Record until stopped by hand, instead of stopping on silence
        #[arg(long)]
        toggle: bool,
        /// Insert finalized transcript segments while recording continues
        #[arg(long)]
        stream: bool,
        /// Disable project spellings for this recording
        #[arg(long)]
        no_project: bool,
    },
    /// Ask a coding agent a question and print its reply
    Ask {
        /// The question. Omit it to record one, or pipe it on stdin
        text: Option<String>,
        /// Bind recording vocabulary and the answering agent to this directory
        #[arg(long)]
        project: Option<PathBuf>,
        /// Which agent answers; defaults to `chat.agent`
        #[arg(long, value_enum)]
        agent: Option<CliAgent>,
        /// A model name the agent's CLI accepts; defaults to the agent's own
        #[arg(long)]
        model: Option<String>,
        /// The conversation to continue, by name; defaults to this directory's
        #[arg(long)]
        session: Option<String>,
        /// Start a fresh conversation instead of continuing the last one
        #[arg(long)]
        new: bool,
        /// Print the ticket and return; the reply shows up in `aria events`
        #[arg(long)]
        detach: bool,
        /// Deliver the reply like a transcript instead of printing it
        #[arg(long, value_enum)]
        mode: Option<CliOutputMode>,
        /// Record until stopped by hand, instead of stopping on silence
        #[arg(long)]
        toggle: bool,
        /// Cancel a pending ask by its ticket instead of asking
        #[arg(
            long,
            value_name = "TICKET",
            conflicts_with_all = ["text", "agent", "model", "session", "new", "detach", "mode", "toggle", "project"]
        )]
        cancel: Option<Ticket>,
    },
    /// Transcribe the machine's own audio output to a file
    Listen {
        #[command(subcommand)]
        command: ListenCommand,
    },
    /// Enroll a voice so Aria can tell who is speaking
    Speaker {
        #[command(subcommand)]
        command: SpeakerCommand,
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

#[derive(Subcommand)]
enum SpeakerCommand {
    /// Record a few utterances and store the voice they agree on
    Enroll {
        /// The person's name, as transcripts should show it
        name: String,
        /// Owner or guest of this machine. Defaults to owner for the first voice
        /// enrolled and guest for every one after
        #[arg(long, value_enum)]
        role: Option<CliRole>,
        /// How many utterances to record, 2 to 10
        #[arg(long, default_value_t = DEFAULT_SAMPLES)]
        samples: u32,
        /// Replace an existing profile of this name
        #[arg(long)]
        replace: bool,
    },
    /// Show who is enrolled
    List,
    /// Forget one voice
    Remove { name: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum CliRole {
    Owner,
    Guest,
}

impl From<CliRole> for Role {
    fn from(role: CliRole) -> Self {
        match role {
            CliRole::Owner => Self::Owner,
            CliRole::Guest => Self::Guest,
        }
    }
}

#[derive(Subcommand)]
enum ListenCommand {
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

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print every setting and where it came from, as this shell would load
    /// it. A daemon started by `aria service` sees the file, not this
    /// shell's variables, and reads it only at start
    Show,
}

#[derive(Subcommand)]
enum HistoryCommand {
    /// List saved dictations and recording availability
    List,
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
    Retry {
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i64::MAX as u64))]
        id: u64,
        #[arg(long)]
        engine: Option<String>,
        #[arg(long)]
        language: Option<aria::transcription::Language>,
    },
    /// Retain successful recordings for up to 24 hours
    RetainAudio {
        #[arg(action = clap::ArgAction::Set)]
        enabled: bool,
    },
}

#[derive(Subcommand)]
enum VocabularyCommand {
    /// Enable local filename/text collection and select the default directory
    Use { directory: PathBuf },
    /// Disable project spellings without deleting the catalog
    Disable,
    /// Show whether a directory has a prepared vocabulary
    Status { directory: Option<PathBuf> },
    /// Rescan local filenames and text to prepare a directory's vocabulary
    Refresh { directory: Option<PathBuf> },
    /// Remove a directory's cached vocabulary
    Forget { directory: Option<PathBuf> },
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// List remembered facts
    List {
        /// Show facts waiting for review instead
        #[arg(long)]
        pending: bool,
    },
    /// Show where a fact came from
    Why { id: i64 },
    /// Delete a fact for good
    Forget { id: i64 },
    /// Approve or reject each fact waiting for review
    Review,
    /// Extract facts from every session with new turns, now
    Consolidate,
    /// Show the word list and the substitutions learned from it
    Lexicon {
        /// Delete one learned substitution by id
        #[arg(long)]
        forget_pair: Option<i64>,
    },
}

#[derive(Subcommand)]
enum WakeCommand {
    /// Run wake-word monitoring in the foreground
    Run,
    /// Report whether wake-word monitoring is listening
    Status,
    /// Show the latest daemon log entries
    Log,
}

/// Aria.app's side of setup: `aria.setup.v1` protobuf frames on stdio,
/// never a terminal.
#[derive(Subcommand)]
enum SetupCommand {
    /// Write one Inspection
    #[command(hide = true)]
    Inspect,
    /// Read one ApplyRequest, write ApplyResponses until the summary
    #[command(hide = true)]
    Apply,
}

#[derive(Subcommand, Clone, Copy)]
enum ServiceCommand {
    /// Register the daemon with launchd or systemd and start it
    Install,
    /// Stop the daemon and remove the registration
    Uninstall,
    /// Start the registered daemon
    Start,
    /// Stop the registered daemon; it comes back at next login
    Stop,
    /// Report the registration and whether the daemon answers
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliOutputMode {
    Insert,
    InsertAndSubmit,
    InsertOrCopy,
    Copy,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliAgent {
    ClaudeCode,
    Codex,
}

impl From<CliAgent> for ChatAgent {
    fn from(agent: CliAgent) -> Self {
        match agent {
            CliAgent::ClaudeCode => Self::ClaudeCode,
            CliAgent::Codex => Self::Codex,
        }
    }
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

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Daemon => run_daemon(),
        Command::Doctor => run_doctor(),
        Command::Config {
            command: ConfigCommand::Show,
        } => run_config_show(),
        Command::Events => run_events(),
        Command::History { command } => run_history(command),
        Command::Memory {
            command: MemoryCommand::Consolidate,
        } => run_memory_consolidate(),
        Command::Memory { command } => run_memory(command),
        Command::Vocabulary { command } => run_vocabulary(command),
        Command::Wake {
            command: WakeCommand::Run,
        } => run_wake(),
        Command::Wake {
            command: WakeCommand::Status,
        } => run_wake_status(),
        Command::Wake {
            command: WakeCommand::Log,
        } => run_wake_log(),
        Command::Service { command } => run_service(command),
        Command::Setup {
            command: None,
            yes,
            skip_models,
        } => run_setup(yes, skip_models),
        Command::Setup {
            command: Some(command),
            ..
        } => run_setup_for_app(command),
        Command::Dictate {
            mode,
            toggle,
            stream,
            no_project,
        } => run_dictate(mode, toggle, stream, no_project),
        Command::Ask {
            text,
            project,
            agent,
            model,
            session,
            new,
            detach,
            mode,
            toggle,
            cancel,
        } => run_ask(AskArgs {
            text,
            project,
            agent,
            model,
            session,
            new,
            detach,
            mode,
            toggle,
            cancel,
        }),
        Command::Listen { command } => run_listen(command),
        Command::Speaker { command } => run_speaker(command),
        Command::Say {
            text,
            voice,
            rate,
            stop,
        } => run_say(text, voice, rate, stop),
    }
}

fn run_speaker(command: SpeakerCommand) -> anyhow::Result<()> {
    let paths = RuntimePaths::for_current_user()
        .prepare()
        .context("failed to prepare daemon runtime paths")?;
    // Enrollment records through the daemon's microphone, so it needs the
    // daemon that owns one; a temporary on-demand daemon has none.
    let session = DaemonSession::connect_at(&paths)?;
    match command {
        SpeakerCommand::Enroll {
            name,
            role,
            samples,
            replace,
        } => enroll(&session, name, role, samples, replace),
        SpeakerCommand::List => list_speakers(&session),
        SpeakerCommand::Remove { name } => remove_speaker(&session, name),
    }
}

fn enroll(
    session: &DaemonSession,
    name: String,
    role: Option<CliRole>,
    samples: u32,
    replace: bool,
) -> anyhow::Result<()> {
    let enrolled = guided::enroll(
        session,
        EnrollmentRequest {
            name: SpeakerName::new(name)?,
            role: role.map(Role::from),
            samples,
            replace,
        },
    )?;
    println!(
        "enrolled {} as {} (consistency {:.2})",
        enrolled.name, enrolled.role, enrolled.consistency
    );
    Ok(())
}

fn list_speakers(session: &DaemonSession) -> anyhow::Result<()> {
    let speakers = session.list_speakers()?;
    if speakers.is_empty() {
        println!("nobody is enrolled");
        println!("enroll with: aria speaker enroll <name>");
        return Ok(());
    }
    for speaker in speakers {
        println!(
            "  {} ({}) — enrolled {}",
            speaker.name,
            speaker.role,
            enrolled_on(speaker.enrolled_at)
        );
    }
    Ok(())
}

/// The day a profile was recorded, in UTC. A profile whose moment does not
/// convert reads as unknown rather than failing the listing: the name and
/// role are what the reader came for.
fn enrolled_on(seconds: u64) -> String {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).ok())
        .and_then(|moment| moment.format(&format).ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn remove_speaker(session: &DaemonSession, name: String) -> anyhow::Result<()> {
    let name = SpeakerName::new(name)?;
    session.remove_speaker(&name)?;
    println!("removed {name}");
    Ok(())
}

fn run_say(
    text: Option<String>,
    voice: Option<String>,
    rate: Option<f32>,
    stop: bool,
) -> anyhow::Result<()> {
    let paths = RuntimePaths::for_current_user()
        .prepare()
        .context("failed to prepare daemon runtime paths")?;
    init_daemon_logging(paths.log_path())?;
    let session = DaemonSession::open_at(&paths)?;

    if stop {
        let was_speaking = session.stop_speaking()?;
        if !was_speaking {
            println!("nothing was playing");
        }
        return Ok(());
    }

    let text = match text {
        Some(text) => text,
        None => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .context("failed to read the text from stdin")?;
            text
        }
    };
    let request = SpeechRequest {
        text: SpeechText::new(text)?,
        voice: voice.map(Voice::new).transpose()?,
        rate: rate.map(SpeakingRate::new).transpose()?,
    };
    if session.speak(request)? == SpeechOutcome::Stopped {
        println!("stopped before the end");
    }
    Ok(())
}

fn run_wake_status() -> anyhow::Result<()> {
    let status = DaemonSession::wake_word_status()?;
    let (message, success) = wake_status_message(status);
    println!("{message}");
    if !success {
        std::process::exit(1);
    }
    Ok(())
}

fn run_wake() -> anyhow::Result<()> {
    let daemon = Daemon::open_wake_word().context("failed to prepare daemon runtime paths")?;

    init_daemon_logging(daemon.log_path())?;
    tracing::info!(socket = %daemon.socket_path().display(), "starting Aria wake-word daemon");

    daemon
        .run()
        .context("Aria wake-word daemon terminated with an error")
}

fn run_wake_log() -> anyhow::Result<()> {
    let paths = RuntimePaths::for_current_user()
        .prepare()
        .context("failed to prepare daemon runtime paths")?;
    let tail = read_log_tail(paths.log_path().to_path_buf())?;
    if tail.is_empty() {
        println!("daemon log is empty");
    } else {
        println!("{tail}");
    }
    Ok(())
}

fn wake_status_message(status: WakeWordStatus) -> (&'static str, bool) {
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

fn run_service(command: ServiceCommand) -> anyhow::Result<()> {
    let supervisor = Supervisor::open().context("failed to prepare the service")?;
    let status = match command {
        ServiceCommand::Install => supervisor.install()?,
        ServiceCommand::Uninstall => supervisor.uninstall()?,
        ServiceCommand::Start => supervisor.start()?,
        ServiceCommand::Stop => supervisor.stop()?,
        ServiceCommand::Status => supervisor.status()?,
    };
    println!("{}", service_status_message(&status));
    let inspect_only = matches!(command, ServiceCommand::Status);
    if inspect_only && !matches!(status.daemon, DaemonState::Ready { .. }) {
        std::process::exit(1);
    }
    Ok(())
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "on" } else { "off" }
}

fn config_write_message(write: &ConfigWrite) -> String {
    match write {
        ConfigWrite::Unchanged => "unchanged".to_owned(),
        ConfigWrite::Written { backup: None } => "written".to_owned(),
        ConfigWrite::Written {
            backup: Some(backup),
        } => format!("written (previous copy at {})", backup.display()),
    }
}

fn service_status_message(status: &ServiceStatus) -> String {
    let job = match status.job {
        Job::Absent => "not registered".to_owned(),
        Job::Idle => "registered, idle".to_owned(),
        Job::Running(pid) => format!("registered, pid {pid}"),
    };
    let daemon = match status.daemon {
        DaemonState::Ready { pid } => {
            format!("ready{}, {}", pid_suffix(pid), status.socket.display())
        }
        DaemonState::Starting { pid } => format!("starting{}", pid_suffix(pid)),
        DaemonState::NotRunning => match status.job {
            Job::Absent => "stopped".to_owned(),
            Job::Idle | Job::Running(_) => {
                "stopped; run `aria service start` or open Aria".to_owned()
            }
        },
    };
    let unit = if status.app_agent {
        "Aria.app's login agent".to_owned()
    } else {
        status.unit.display().to_string()
    };
    format!("unit     {unit} ({job})\ndaemon   {daemon}")
}

fn run_setup(yes: bool, skip_models: bool) -> anyhow::Result<()> {
    let options = SetupOptions {
        yes,
        skip_models,
        interactive: std::io::stdin().is_terminal(),
        color: std::io::stdout().is_terminal(),
    };
    let summary = aria::setup::run(
        options,
        &mut std::io::stdin().lock(),
        &mut std::io::stdout(),
    )
    .context("setup did not finish")?;
    let selections = &summary.selections;
    println!();
    println!("speech       {}", selections.stt);
    println!("voice        {}", selections.tts);
    println!("chat         {}", selections.chat);
    println!("wake word    {}", on_off(selections.wake));
    println!("hotkey       {}", on_off(selections.hotkey));
    println!("config       {}", config_write_message(&summary.config));
    match &summary.wakeword {
        Some(dir) => println!("wake model   {}", dir.display()),
        None => println!("wake model   none shipped; set wake.model to your own"),
    }
    for (name, outcome) in &summary.models {
        println!("{name:<12} {}", outcome.as_str());
    }
    if let Some(unit) = &summary.legacy_agent {
        println!("removed      {} (the Python wake agent)", unit.display());
    }
    println!("{}", service_status_message(&summary.service));
    if let Some(voice) = enrolled_message(&summary.enrolled) {
        println!("voice        {voice}");
    }
    if let Some(press) = try_out_message(&summary.tried) {
        println!("test press   {press}");
    }
    if summary.grant == Grant::Waiting {
        println!(
            "next         grant Accessibility: System Settings → Privacy & Security → \
             Accessibility → + → {}; the hotkey arms within ~5s. Typing needs \
             `aria service stop && aria service start` as well",
            aria::doctor::aria_binary_label()
        );
    }
    if cfg!(target_os = "macos") {
        println!("next         open Aria for the menu bar, then run `aria doctor`");
    } else {
        println!(
            "next         `loginctl enable-linger` keeps the daemon across logouts; run `aria doctor`"
        );
    }
    Ok(())
}

fn run_setup_for_app(command: SetupCommand) -> anyhow::Result<()> {
    anyhow::ensure!(
        !std::io::stdout().is_terminal(),
        "this writes protobuf for Aria.app; run `aria setup` instead"
    );
    let mut stdout = std::io::stdout().lock();
    match command {
        SetupCommand::Inspect => aria::setup::wire::inspect(&mut stdout),
        SetupCommand::Apply => aria::setup::wire::apply(&mut std::io::stdin().lock(), &mut stdout),
    }
    .context("setup did not finish")
}

/// What to say about the voice, or nothing when there is nothing to report.
/// A run that never asked stays quiet: the reason is usually that no model
/// was fetched, and setup already printed that line.
fn enrolled_message(enrolled: &Enrolled) -> Option<String> {
    match enrolled {
        Enrolled::NotOffered => None,
        Enrolled::Declined => {
            Some("skipped; enroll later with `aria speaker enroll <name>`".into())
        }
        Enrolled::Done { name, consistency } => {
            Some(format!("{name} enrolled (consistency {consistency:.2})"))
        }
        Enrolled::Failed(why) => Some(format!("not enrolled: {why}")),
    }
}

/// What the test press proved, or nothing when there was none.
fn try_out_message(tried: &TryOut) -> Option<String> {
    match tried {
        TryOut::NotOffered => None,
        TryOut::Declined => Some("skipped; try one with `aria dictate`".into()),
        TryOut::Done { heard, spoke } if *spoke => Some(format!("heard and answered: {heard:?}")),
        TryOut::Done { heard, .. } => Some(format!("heard {heard:?}, but the voice did not play")),
        TryOut::Failed(why) => Some(format!("did not work: {why}")),
    }
}

fn pid_suffix(pid: Option<u32>) -> String {
    pid.map(|pid| format!(", pid {pid}")).unwrap_or_default()
}

fn run_doctor() -> anyhow::Result<()> {
    // The runtime directory must stay uncreated until the check for it runs,
    // so no daemon logging starts here.
    let report = aria::doctor::diagnose();
    let width = report
        .checks
        .iter()
        .map(|check| check.name.len())
        .max()
        .unwrap_or_default();

    for check in &report.checks {
        let status = match check.status {
            Status::Ok => "OK",
            Status::Warning => "WARN",
            Status::Failed => "FAIL",
        };
        println!(
            "{name:width$}  {status:4}  {detail}",
            name = check.name,
            detail = check.detail
        );
    }

    if report.has_failure() {
        std::process::exit(1);
    }
    Ok(())
}

fn run_config_show() -> anyhow::Result<()> {
    let report = aria::config::show()?;
    if report.path.is_file() {
        println!("{}", report.path.display());
    } else {
        println!("no {}", report.path.display());
    }
    println!();

    let lines: Vec<(&str, String, String)> = report
        .rows
        .iter()
        .map(|row| match row {
            Ok(row) => (row.key, row.value.clone(), row.source.to_string()),
            Err(failed) => (
                failed.section,
                error_chain(&failed.error),
                "error".to_owned(),
            ),
        })
        .collect();
    let key_width = lines
        .iter()
        .map(|(key, _, _)| key.len())
        .max()
        .unwrap_or_default();
    let value_width = lines
        .iter()
        .map(|(_, value, _)| value.len())
        .max()
        .unwrap_or_default();
    for (key, value, source) in &lines {
        println!("{key:key_width$}  {value:value_width$}  {source}");
    }

    Ok(())
}

fn run_memory_consolidate() -> anyhow::Result<()> {
    // Extraction reports what it skipped as warnings; here they belong on
    // the terminal, not in a daemon log.
    tracing_subscriber::fmt()
        .with_target(false)
        .without_time()
        .compact()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize logging: {error}"))?;
    let report = aria::memory::consolidate_now()?;
    let results = report.results;
    if results.is_empty() && report.failed == 0 {
        println!("Nothing to consolidate (or memory disabled)");
        return Ok(());
    }
    let added: u32 = results.iter().map(|result| result.added).sum();
    let pending: u32 = results.iter().map(|result| result.pending).sum();
    println!(
        "Consolidated {} session(s): {added} added, {pending} pending",
        results.len()
    );
    if pending > 0 {
        println!("Run `aria memory review` to triage pending facts");
    }
    anyhow::ensure!(
        report.failed == 0,
        "{} session(s) could not be consolidated; see warnings above",
        report.failed
    );
    Ok(())
}

fn run_memory(command: MemoryCommand) -> anyhow::Result<()> {
    let (store, dir) = aria::memory::open()?;
    match command {
        MemoryCommand::List { pending } => {
            let facts = match pending {
                true => store.pending_facts()?,
                false => store.active_facts()?,
            };
            if facts.is_empty() {
                println!(
                    "{}",
                    if pending {
                        "No pending facts"
                    } else {
                        "No memories yet"
                    }
                );
            }
            for fact in facts {
                println!("{:>4}  [{}]  {}", fact.id, fact.kind, fact.text);
            }
        }
        MemoryCommand::Why { id } => {
            let fact = store
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("No fact with id {id}"))?;
            println!("[{}] {}", fact.kind, fact.text);
            println!("  said: \"{}\"", fact.source_quote);
            println!(
                "  session: {}  confidence: {}",
                fact.source_session, fact.confidence
            );
            println!("  status: {}  created: {}", fact.status, fact.created_at);
        }
        MemoryCommand::Forget { id } => {
            let fact = store
                .get(id)?
                .ok_or_else(|| anyhow::anyhow!("No fact with id {id}"))?;
            if answer(&format!("Forget \"{}\"? [y/N] ", fact.text))? != "y" {
                return Ok(());
            }
            store.forget(id)?;
            aria::memory::write_profile(&store, &dir)?;
            println!("Forgotten");
        }
        MemoryCommand::Review => {
            let pending = store.pending_facts()?;
            if pending.is_empty() {
                println!("Nothing to review");
                return Ok(());
            }
            for fact in pending {
                println!(
                    "\n[{}] {}\n  said: \"{}\"",
                    fact.kind, fact.text, fact.source_quote
                );
                match answer("  (a)pprove / (r)eject / (s)kip [s]: ")?.as_str() {
                    "a" => {
                        store.approve(fact.id)?;
                    }
                    "r" => {
                        store.forget(fact.id)?;
                    }
                    _ => {}
                }
            }
            aria::memory::write_profile(&store, &dir)?;
        }
        MemoryCommand::Lexicon { forget_pair } => {
            if let Some(id) = forget_pair {
                anyhow::ensure!(store.forget_pair(id)?, "No pair with id {id}");
                println!("Pair forgotten");
                return Ok(());
            }
            let path = dir.join(aria::memory::LEXICON_FILE);
            let terms = aria::memory::load_terms(&path);
            if terms.is_empty() {
                println!("No lexicon terms ({} missing or empty)", path.display());
            } else {
                println!("Terms ({}):", path.display());
                for term in terms {
                    println!("  {term}");
                }
            }
            let pairs = store.list_pairs()?;
            if !pairs.is_empty() {
                println!("Derived pairs:");
                for pair in pairs {
                    println!(
                        "  {:>4}  {} → {}  [{}]",
                        pair.id, pair.misheard, pair.correct, pair.source
                    );
                }
            }
        }
        MemoryCommand::Consolidate => run_memory_consolidate()?,
    }
    Ok(())
}

/// One line from the terminal, trimmed and lowercased.
fn answer(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

fn run_daemon() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let login_path = adopt_login_shell_path();
    let daemon = Daemon::open().context("failed to prepare daemon runtime paths")?;

    init_daemon_logging(daemon.log_path())?;
    tracing::info!(socket = %daemon.socket_path().display(), "starting Aria daemon");
    #[cfg(target_os = "macos")]
    if let Err(error) = login_path {
        tracing::warn!(
            error = %error_chain(error.as_ref()),
            "kept the inherited PATH; agents and tools outside it will not start"
        );
    }

    daemon
        .run()
        .context("Aria daemon terminated with an error")?;
    Ok(())
}

/// Restarts the daemon in place with the PATH a terminal would have, so it
/// finds `claude`, `codex`, and `ffmpeg` however launchd started it. Returns
/// only when it could not; the restarted daemon's PATH is no longer bare.
#[cfg(target_os = "macos")]
fn adopt_login_shell_path() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;

    if !aria::daemon::needs_login_shell_path(std::env::var_os("PATH").as_deref()) {
        return Ok(());
    }
    let path = aria::daemon::login_shell_path()?;
    // Same pid, so launchd still tracks the daemon.
    let error = std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env("PATH", path)
        .exec();
    Err(error.into())
}

fn run_events() -> anyhow::Result<()> {
    let paths = RuntimePaths::for_current_user()
        .prepare()
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
                agent,
                kind,
            }) => {
                let agent = agent.name();
                match kind {
                    ChatStageKind::Working(step) => {
                        writeln!(stdout, "chat {ticket} {agent} {step}")
                    }
                    ChatStageKind::Replied(reply) => {
                        writeln!(
                            stdout,
                            "chat {ticket} {agent} replied {}",
                            reply.text.lines().next().unwrap_or("")
                        )
                    }
                    ChatStageKind::Failed { code, message } => {
                        writeln!(
                            stdout,
                            "chat {ticket} {agent} failed {}: {message}",
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

fn vocabulary_directory(path: PathBuf) -> anyhow::Result<aria::vocabulary::Directory> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
        Err(error) => return Err(error.into()),
    };
    Ok(aria::vocabulary::Directory::new(path)?)
}

fn run_history(command: HistoryCommand) -> anyhow::Result<()> {
    use aria::history::{HistoryQuery, RetryOptions};
    use aria::preferences::{PreferencePatch, UpdateResult};

    if let HistoryCommand::Clear { yes } = &command
        && !confirm_history_clear(*yes, std::io::stdin().is_terminal(), answer)?
    {
        println!("History clear cancelled.");
        return Ok(());
    }

    let paths = RuntimePaths::for_current_user().prepare()?;
    let session = DaemonSession::connect_at(&paths)?;
    match command {
        HistoryCommand::Clear { .. } => {
            let result = session.clear_history().map_err(history_clear_error)?;
            println!("{}", history_clear_message(result)?);
        }
        HistoryCommand::List => {
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
                if before_id.is_none() && page.entries.is_empty() {
                    println!("no dictation history");
                }
                for entry in page.entries {
                    println!(
                        "{}\t{:?}\taudio={}\t{}",
                        entry.id,
                        entry.status,
                        history_audio_label(entry.audio),
                        entry.text.replace(['\n', '\r', '\t'], " ")
                    );
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
        HistoryCommand::RetainAudio { enabled } => {
            let snapshot = session.get_preferences()?;
            let patch = PreferencePatch {
                history_audio_retention: Some(enabled),
                ..PreferencePatch::default()
            };
            anyhow::ensure!(
                matches!(
                    session.update_preferences(snapshot.revision, patch)?,
                    UpdateResult::Updated(_)
                ),
                "preferences changed; retry the command"
            );
            println!("history audio retention: {}", on_off(enabled));
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

fn history_clear_message(result: aria::history::ClearHistoryResult) -> anyhow::Result<String> {
    anyhow::ensure!(
        !result.cleanup_pending,
        "Dictation text cleared. Some recordings could not be removed; Aria will retry."
    );
    Ok(format!(
        "Dictation history cleared ({} entries).",
        result.deleted_entries
    ))
}

fn history_clear_error(error: aria::daemon::ClientError) -> anyhow::Error {
    use aria::daemon::ClientError;
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

fn history_audio_label(audio: aria::history::AudioAvailability) -> &'static str {
    use aria::history::AudioAvailability;
    match audio {
        AudioAvailability::Available { .. } => "Available",
        AudioAvailability::Expired => "Expired",
        AudioAvailability::Unavailable => "Unavailable",
    }
}

fn run_vocabulary(command: VocabularyCommand) -> anyhow::Result<()> {
    use aria::preferences::{PreferencePatch, UpdateResult};
    let paths = RuntimePaths::for_current_user().prepare()?;
    let session = DaemonSession::connect_at(&paths)?;
    let status =
        match command {
            VocabularyCommand::Use { directory } => {
                let directory = vocabulary_directory(
                    directory
                        .canonicalize()
                        .context("failed to resolve the vocabulary directory")?,
                )?;
                anyhow::ensure!(
                    directory.as_path().is_dir(),
                    "the vocabulary path is not a directory"
                );
                let snapshot = session.get_preferences()?;
                let patch = PreferencePatch {
                    project_vocabulary: Some(true),
                    vocabulary_directory: Some(Some(directory.clone())),
                    ..PreferencePatch::default()
                };
                anyhow::ensure!(
                    matches!(
                        session.update_preferences(snapshot.revision, patch)?,
                        UpdateResult::Updated(_)
                    ),
                    "preferences changed; retry the command"
                );
                session.vocabulary_status(Some(&directory))?
            }
            VocabularyCommand::Disable => {
                let snapshot = session.get_preferences()?;
                let patch = PreferencePatch {
                    project_vocabulary: Some(false),
                    ..PreferencePatch::default()
                };
                anyhow::ensure!(
                    matches!(
                        session.update_preferences(snapshot.revision, patch)?,
                        UpdateResult::Updated(_)
                    ),
                    "preferences changed; retry the command"
                );
                session.vocabulary_status(None)?
            }
            VocabularyCommand::Status { directory } => session
                .vocabulary_status(directory.map(vocabulary_directory).transpose()?.as_ref())?,
            VocabularyCommand::Refresh { directory } => session
                .vocabulary_refresh(directory.map(vocabulary_directory).transpose()?.as_ref())?,
            VocabularyCommand::Forget { directory } => session
                .vocabulary_forget(directory.map(vocabulary_directory).transpose()?.as_ref())?,
        };
    println!(
        "{:?}: {} terms, catalog revision {}, runtime revision {}",
        status.state, status.terms, status.catalog_revision, status.runtime_revision
    );
    if status.state != aria::vocabulary::State::Disabled && !status.error.is_empty() {
        anyhow::bail!(
            "vocabulary {} failed; the previous snapshot is retained when available",
            status.error
        );
    }
    Ok(())
}

fn run_dictate(
    mode: Option<CliOutputMode>,
    toggle: bool,
    stream: bool,
    no_project: bool,
) -> anyhow::Result<()> {
    let paths = RuntimePaths::for_current_user()
        .prepare()
        .context("failed to prepare daemon runtime paths")?;
    init_daemon_logging(paths.log_path())?;
    let session = Arc::new(DaemonSession::open_at(&paths)?);
    let project = if no_project || !session.get_preferences()?.preferences.project_vocabulary {
        aria::vocabulary::Context::Disabled
    } else {
        aria::vocabulary::Context::Directory(aria::vocabulary::Directory::new(
            std::env::current_dir()?,
        )?)
    };
    let mut interrupt = Interrupt::install(&session)?;
    // Only `--stream` streams; the delivery preference never applies here.
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
            stop_mode: toggle.then_some(StopMode::Manual),
            transcript_only: false,
            project,
        },
        |stage| recording.follow(stage),
    );
    if let Some(error) = recording.take_failure() {
        eprintln!("the recording could not be stopped: {error:#}");
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

struct AskArgs {
    text: Option<String>,
    project: Option<PathBuf>,
    agent: Option<CliAgent>,
    model: Option<String>,
    session: Option<String>,
    new: bool,
    detach: bool,
    mode: Option<CliOutputMode>,
    toggle: bool,
    cancel: Option<Ticket>,
}

fn run_ask(args: AskArgs) -> anyhow::Result<()> {
    let paths = RuntimePaths::for_current_user()
        .prepare()
        .context("failed to prepare daemon runtime paths")?;
    init_daemon_logging(paths.log_path())?;

    let session = Arc::new(DaemonSession::connect_at(&paths)?);
    if let Some(ticket) = args.cancel {
        if session.cancel(ticket)? {
            println!("cancelled {ticket}");
        } else {
            println!("{ticket} is not pending");
        }
        return Ok(());
    }
    let project = args
        .project
        .map(|path| {
            let path = path
                .canonicalize()
                .context("failed to resolve the project directory")?;
            anyhow::ensure!(path.is_dir(), "the project path is not a directory");
            Ok::<_, anyhow::Error>(aria::vocabulary::Directory::new(path)?)
        })
        .transpose()?;
    let stop_mode = args.toggle.then_some(StopMode::Manual);
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
                    aria::vocabulary::Context::Directory(project.clone())
                } else if session.get_preferences()?.preferences.project_vocabulary {
                    match session.route(String::new(), Clarify::Refuse)? {
                        Routing::Ready { request, .. } => aria::vocabulary::Context::Directory(
                            vocabulary_directory(request.workdir)?,
                        ),
                        Routing::Stopped => return Ok(()),
                    }
                } else {
                    aria::vocabulary::Context::Disabled
                };
                let heard =
                    session.transcribe_in(stop_mode, context, |stage| recording.follow(stage));
                if let Some(error) = recording.take_failure() {
                    eprintln!("the recording could not be stopped: {error:#}");
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
            ..ChatRequest::configured(text.clone())?
        };
        (request, None)
    } else {
        match session.route(text.clone(), clarify) {
            Ok(Routing::Ready { request, focus }) => (request, focus),
            Ok(Routing::Stopped) => return Ok(()),
            // What was said is not lost with the routing that failed.
            Err(error) if spoken.is_some() => {
                eprintln!("you said: {text}");
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        }
    };
    let request = ChatRequest {
        agent: args.agent.map_or(configured.agent, Into::into),
        model: args.model,
        session: args.session,
        new: args.new,
        output_mode: args.mode.map(Into::into),
        own_voice: spoken.unwrap_or(false),
        ..configured
    };
    let agent = request.agent.name();
    let workdir = request.workdir.display().to_string();
    let pending = match session.ask(request) {
        Ok(pending) => pending,
        // What was said is not lost with the ask that failed.
        Err(error) if spoken.is_some() => {
            eprintln!("you said: {text}");
            return Err(error.into());
        }
        Err(error) => return Err(error.into()),
    };
    let pending_ticket = pending.ticket();
    if let Some(error) = focus.and_then(|focus| focus.store().err()) {
        eprintln!("the project was not remembered: {error:#}");
    }
    eprintln!("asked {pending_ticket} {agent} in {workdir}");
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
            ChatStageKind::Working(step) => working.show(format!("{ticket} {agent} · {step}")),
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
        eprintln!("{pending_ticket} could not be cancelled: {error:#}");
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
            eprintln!("the reply was not delivered ({code}); printing it instead");
            println!("{}", reply.text);
        }
        None if args.mode.is_none() => println!("{}", reply.text),
        None => {}
    }
    Ok(())
}

fn run_listen(command: ListenCommand) -> anyhow::Result<()> {
    let paths = RuntimePaths::for_current_user()
        .prepare()
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
            ListenStage::SegmentTranscribed {
                text,
                label,
                at_seconds,
            } => {
                let now = std::time::Instant::now();
                let label = label.as_deref().map(str::to_owned);
                let mut stdout = std::io::stdout().lock();
                if open_label.as_ref() != Some(&label)
                    || now.duration_since(last_segment) >= aria::listen::GAP_BREAK
                {
                    if open_label.is_some() {
                        writeln!(stdout)?;
                        writeln!(stdout)?;
                    }
                    write!(stdout, "{} ", aria::listen::stamp(at_seconds))?;
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

fn init_daemon_logging(log_path: &Path) -> anyhow::Result<()> {
    let log = open_log_writer(log_path.to_path_buf()).context("failed to open the daemon log")?;
    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .compact()
        .with_ansi(false)
        .with_writer(log)
        .with_filter(log_filter());
    let terminal_layer = std::env::var_os("RUST_LOG").is_some().then(|| {
        tracing_subscriber::fmt::layer()
            .with_target(true)
            .compact()
            .with_writer(std::io::stderr)
            .with_filter(log_filter())
    });
    let subscriber = tracing_subscriber::registry()
        .with(file_layer)
        .with(terminal_layer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| anyhow::anyhow!("failed to initialize daemon logging: {error}"))?;
    // HTTP traces can contain dictated text, even when application errors are redacted.
    tracing_log::LogTracer::builder()
        .ignore_crate("ureq")
        .ignore_crate("ureq_proto")
        .init()
        .map_err(|error| anyhow::anyhow!("failed to initialize daemon logging: {error}"))?;
    aria_stt::whisper::install_logging_hooks();
    Ok(())
}

/// The daemon's log filter, for the file and the terminal alike. `RUST_LOG`
/// has to reach the file: the daemon runs detached, so its terminal goes
/// nowhere and a filter the file ignores is a switch that does nothing.
fn log_filter() -> EnvFilter {
    match std::env::var("RUST_LOG") {
        Ok(overrides) => EnvFilter::new(format!("{DEFAULT_LOG_FILTER},{overrides}")),
        Err(_) => EnvFilter::new(DEFAULT_LOG_FILTER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_logging_excludes_http_payloads_under_trace_overrides() {
        use aria_primitives::process::{ChildProcess, WaitOutcome};
        use std::io::Read;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CHILD_LOG_PATH: &str = "ARIA_TEST_HTTP_LOG_PATH";
        if let Some(path) = std::env::var_os(CHILD_LOG_PATH) {
            init_daemon_logging(Path::new(&path)).unwrap();
            tracing_log::log::trace!(target: "ureq_proto::util", "private-response-marker");
            tracing_log::log::debug!(target: "ureq::body", "private-header-marker");
            tracing::info!(target: "aria", "application-marker");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.log");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tests::daemon_logging_excludes_http_payloads_under_trace_overrides",
                "--nocapture",
            ])
            .env(CHILD_LOG_PATH, &path)
            .env("RUST_LOG", "trace,ureq_proto::util=trace,ureq::body=trace")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = ChildProcess::spawn(&mut command).unwrap();
        let stderr = child.take_stderr().unwrap();
        let (outcome, stderr) = std::thread::scope(|scope| {
            let reader = scope.spawn(move || {
                let mut bytes = Vec::new();
                stderr.take(16 * 1024).read_to_end(&mut bytes).unwrap();
                bytes
            });
            let outcome = child.wait_until(Instant::now() + Duration::from_secs(5), || false);
            child.terminate_in_place().unwrap();
            (outcome.unwrap(), reader.join().unwrap())
        });
        assert!(matches!(outcome, WaitOutcome::Exited(status) if status.success()));
        for output in [
            std::fs::read_to_string(path).unwrap(),
            String::from_utf8(stderr).unwrap(),
        ] {
            assert!(output.contains("application-marker"));
            assert!(!output.contains("private-response-marker"));
            assert!(!output.contains("private-header-marker"));
        }
    }

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
                command: HistoryCommand::List
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
                    id: 42, engine: Some(engine), language: Some(aria::transcription::Language::Automatic)
                }
            } if engine == "whisper"
        ));
        for (value, expected) in [("true", true), ("false", false)] {
            assert!(matches!(
                Cli::try_parse_from(["aria", "history", "retain-audio", value]).unwrap().command,
                Command::History { command: HistoryCommand::RetainAudio { enabled } }
                    if enabled == expected
            ));
        }
        for arguments in [
            vec!["aria", "history", "retry", "0"],
            vec!["aria", "history", "retry", "9223372036854775808"],
            vec!["aria", "history", "retry", "42", "--language", "fr_CA"],
            vec!["aria", "history", "retain-audio", "maybe"],
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
        use aria::daemon::ClientError;
        use aria::history::ClearHistoryResult;

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
                .contains("Some recordings could not be removed")
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
        use aria::history::AudioAvailability;
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

    #[test]
    fn vocabulary_commands_and_recording_context_parse() {
        for arguments in [
            vec!["aria", "vocabulary", "use", "/projects/aria"],
            vec!["aria", "vocabulary", "status"],
            vec!["aria", "vocabulary", "refresh", "/projects/aria"],
            vec!["aria", "vocabulary", "forget", "/deleted/project"],
            vec!["aria", "vocabulary", "disable"],
        ] {
            assert!(matches!(
                Cli::try_parse_from(arguments).unwrap().command,
                Command::Vocabulary { .. }
            ));
        }
        assert!(matches!(
            Cli::try_parse_from(["aria", "dictate", "--no-project"])
                .unwrap()
                .command,
            Command::Dictate {
                no_project: true,
                ..
            }
        ));
        assert!(
            matches!(Cli::try_parse_from(["aria", "ask", "--project", "/projects/aria", "hello"]).unwrap().command, Command::Ask { project: Some(path), .. } if path == Path::new("/projects/aria"))
        );
        assert!(
            Cli::try_parse_from([
                "aria",
                "ask",
                "--project",
                "/projects/aria",
                "--cancel",
                "1"
            ])
            .is_err()
        );
    }

    #[test]
    fn setup_takes_its_two_flags() {
        let parse = |arguments: &[&str]| {
            let cli = Cli::try_parse_from(arguments).expect("the setup command should parse");
            let Command::Setup {
                command: None,
                yes,
                skip_models,
            } = cli.command
            else {
                panic!("a setup invocation should parse as Setup");
            };
            (yes, skip_models)
        };

        assert_eq!(parse(&["aria", "setup"]), (false, false));
        assert_eq!(
            parse(&["aria", "setup", "--yes", "--skip-models"]),
            (true, true)
        );
    }

    #[test]
    fn setup_has_hidden_subcommands_for_the_app() {
        let cli = Cli::try_parse_from(["aria", "setup", "inspect"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Setup {
                command: Some(SetupCommand::Inspect),
                ..
            }
        ));
        let cli = Cli::try_parse_from(["aria", "setup", "apply"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Setup {
                command: Some(SetupCommand::Apply),
                ..
            }
        ));
    }

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
            assert_eq!(wake_status_message(status).1, expected);
        }
    }
}
