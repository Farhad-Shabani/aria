//! `aria setup`: the guided first run. `inspect` looks at what this
//! machine has. `apply` fetches the models the chosen engines need plus the
//! ones `aria listen` uses, writes the engines to config.yaml, and installs
//! the daemon as a service. Neither asks anything. `run` is the terminal front
//! over them; Aria.app drives the same two through `aria setup inspect` and
//! `aria setup apply`. Every answer defaults to what is configured, and
//! `--yes` keeps every configured engine, so the unattended rerun the
//! installer makes never rewrites a working choice.
//!
//! The terminal run then ends on something that works rather than on a
//! list of what was written: a voice to recognize, and one press proving
//! the microphone and the speech engine both do their job. Both need a
//! person at a terminal, so both skip themselves under `--yes` or a pipe.

use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::config::{self, ConfigError, FileConfig, ServerEndpoint};
use crate::daemon::{
    DaemonSession, EnrollmentRequest, InferencePaths, PairedServer, SpeakerName, StopMode, guided,
};
use crate::ipc::{HOTKEY_HEALTH_SERVICE, Liveness, probe_daemon};
use crate::primitives::error_chain;
use crate::primitives::process::ChildProcessError;
use crate::primitives::user_home::{self, UserHomeError};
use crate::runtime::PreparedRuntimePaths;
use crate::speech::SpeechRequest;
use crate::supervisor::{self, DaemonState, Job, Status, Supervisor, SupervisorError};
use crate::synthesis::SpeechText;
use aria_kit::backend::ModelError;

mod assets;
mod backends;
mod config_file;
mod download;
pub mod models;
mod prompt;
pub mod wire;

use assets::{Asset, EXTRAS, SPEAKER as SPEAKER_ASSET};
pub use backends::{Availability, Choice};
use backends::{Backend, Chat, Host, Stt, Tts, choices, stt_choices};
pub use config_file::{ConfigWrite, Existing, Selections};
use models::OnDisk;
use prompt::Prompts;

/// Where setup installs the wake-word models, inside the models directory.
const WAKEWORD_DIR: &str = "wakeword";
const WAKEWORD_FILES: [&str; 3] = ["aria.onnx", "melspectrogram.onnx", "embedding_model.onnx"];
/// Where the wake-word models sit relative to the binary: the app bundle,
/// the Linux tarball, and a source checkout.
const BUNDLED_WAKEWORD_DIRS: [&str; 3] = [
    "../Resources/models/wakeword",
    "../share/aria/wakeword",
    "../../models/wakeword",
];
/// The listener re-checks the grant every `PERMISSION_RETRY` (3 s). A longer
/// wait only delays a user who has not granted yet.
const GRANT_WAIT: Duration = Duration::from_secs(6);
const GRANT_POLL: Duration = Duration::from_millis(500);
/// What the test press answers with. Short, so a slow engine is not the
/// last thing setup makes someone sit through.
const SPOKEN_REPLY: &str = "Aria is ready.";

pub struct Options {
    pub yes: bool,
    pub skip_models: bool,
    /// Whether an answer can be read; false for a pipe.
    pub interactive: bool,
    /// Whether the output stream shows ANSI styling.
    pub color: bool,
}

/// What this machine has, before anything is asked or changed.
pub struct Inspection {
    pub config_path: PathBuf,
    pub macos: bool,
    pub existing: Existing,
    pub stt: Vec<Choice>,
    pub tts: Vec<Choice>,
    pub chat: Vec<Choice>,
    pub models: Vec<ModelState>,
    pub wakeword_installed: bool,
    pub service: Status,
    pub grant: Grant,
    /// Whether wake-word models ship beside the binary.
    wakeword_bundled: bool,
    pairing: Pairing,
    configured_server: Option<String>,
    home: PathBuf,
    exe: PathBuf,
}

enum Pairing {
    Unpaired,
    Paired(PairedServer),
    /// The pin file can't be read; pairing again needs it removed.
    Unusable,
}

/// One model setup can fetch, and whether it already is on disk.
#[derive(Clone)]
pub struct ModelState {
    pub name: &'static str,
    pub title: &'static str,
    /// The engine that wants it; `None` for one every install wants.
    pub engine: Option<&'static str>,
    pub bytes: u64,
    /// Whether it is fetched when nobody answers.
    pub default: bool,
    pub on_disk: OnDisk,
    /// Config points its engine at files elsewhere, so setup has nothing to
    /// fetch.
    pub external: bool,
    asset: &'static Asset,
}

/// One thing a run still has to put in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    Config,
    Wakeword,
    Model(&'static str),
    Service,
    /// A serving daemon whose hotkey listener has not armed.
    Grant,
}

/// The steps `apply` announces as it starts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Config,
    Wakeword,
    Models,
    Service,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupEvent {
    Started(Step),
    Download {
        model: &'static str,
        received: u64,
        total: u64,
    },
}

#[derive(Debug)]
pub struct ApplyOptions {
    /// The models to fetch, by name; any other wanted one is skipped.
    pub downloads: Vec<String>,
    /// Whether curl draws its own bar on the terminal.
    pub transfer_meter: bool,
}

pub struct Summary {
    pub selections: Selections,
    pub config: ConfigWrite,
    /// Where the wake-word models went; `None` when none ship beside the binary.
    pub wakeword: Option<PathBuf>,
    pub models: Vec<(&'static str, Outcome)>,
    pub legacy_agent: Option<PathBuf>,
    pub service: Status,
    pub enrolled: Enrolled,
    pub tried: TryOut,
    pub grant: Grant,
}

/// What setup did about the person's own voice.
#[derive(Debug, Clone, PartialEq)]
pub enum Enrolled {
    /// Nobody was asked: no model, no terminal, `--yes`, no daemon to record
    /// through, or a voice already enrolled.
    NotOffered,
    Declined,
    Done {
        name: String,
    },
    /// The room was quiet, the daemon refused, or the samples disagreed.
    /// Setup carries on: a voice is not what makes an install work.
    Failed(String),
}

/// What the test press proved.
#[derive(Debug, Clone, PartialEq)]
pub enum TryOut {
    /// No terminal, `--yes`, or no daemon answering.
    NotOffered,
    Declined,
    /// What the microphone heard, and whether the voice answered back.
    Done {
        heard: String,
        spoke: bool,
    },
    Failed(String),
}

/// What setup did about one model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Downloaded,
    Present,
    Skipped,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Downloaded => "downloaded",
            Self::Present => "already there",
            Self::Skipped => "skipped",
        }
    }
}

/// Whether the daemon's hotkey listener armed, which needs the macOS
/// Accessibility grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    NotNeeded,
    Armed,
    /// The daemon runs but the listener has not armed; the grant is owed.
    Waiting,
}

pub fn set_phone_pairing(enabled: bool) -> Result<ConfigWrite, SetupError> {
    config_file::set_phone_pairing(&config_path()?, enabled)
}

fn config_path() -> Result<PathBuf, SetupError> {
    config::default_path().map_err(|source| SetupError::Config {
        path: PathBuf::from("~/.aria/config.yaml"),
        source,
    })
}

pub fn inspect() -> Result<Inspection, SetupError> {
    let home = user_home::resolve().map_err(SetupError::Home)?;
    let models_dir = config::models_dir(&home);
    // `current_exe` may answer the symlink an installer put on PATH; the
    // bundled files sit beside the real binary.
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(SetupError::CurrentExe)?;
    let config_path = config_path()?;
    let host = Host::probe();
    let file = config_file::read(&config_path)?;
    let pairing = match crate::daemon::paired_server(&home) {
        Ok(Some(paired)) => Pairing::Paired(paired),
        Ok(None) => Pairing::Unpaired,
        Err(error) => {
            tracing::warn!(error = %error_chain(&error), "inference pairing is unusable");
            Pairing::Unusable
        }
    };
    let existing = Existing::of(&file);
    let service = Supervisor::open()
        .and_then(|supervisor| supervisor.status())
        .map_err(SetupError::Service)?;
    let grant = if existing.hotkey == Some(true) {
        hotkey_grant(&service.socket, Duration::ZERO)
    } else {
        Grant::NotNeeded
    };
    Ok(Inspection {
        config_path,
        macos: host.macos,
        stt: stt_choices(&file, matches!(pairing, Pairing::Paired(_)), &host),
        tts: choices::<Tts>(existing.tts.as_deref(), &host),
        chat: choices::<Chat>(existing.chat.as_deref(), &host),
        models: model_states(&home, &file),
        wakeword_installed: WAKEWORD_FILES
            .iter()
            .all(|file| models_dir.join(WAKEWORD_DIR).join(file).is_file()),
        wakeword_bundled: bundled_wakeword_dir(&exe).is_some(),
        pairing,
        configured_server: file.transcription.server.clone(),
        existing,
        service,
        grant,
        home,
        exe,
    })
}

impl Inspection {
    /// The models the chosen engines want, plus the ones every install
    /// wants, that are not on disk yet.
    pub fn downloads_for<'a>(&'a self, selections: &'a Selections) -> Vec<&'a ModelState> {
        self.wanted(selections)
            .filter(|model| model.on_disk != OnDisk::Complete)
            .collect()
    }

    fn wanted<'a>(
        &'a self,
        selections: &'a Selections,
    ) -> impl Iterator<Item = &'a ModelState> + 'a {
        self.models.iter().filter(move |model| {
            !model.external
                && model
                    .engine
                    .is_none_or(|engine| engine == selections.stt || engine == selections.tts)
        })
    }

    /// `aria remote pair` replaces the pin but not the config's copy of its
    /// address.
    fn server_address_is_stale(&self) -> bool {
        let Pairing::Paired(paired) = &self.pairing else {
            return false;
        };
        self.existing.stt.as_deref() == Some(config::backend::SERVER)
            && self.configured_server != Some(paired.endpoint.to_string())
    }

    /// `None` for a local engine.
    fn server_endpoint(
        &self,
        selections: &Selections,
    ) -> Result<Option<&ServerEndpoint>, SetupError> {
        let wants_server = selections.stt == config::backend::SERVER;
        if !wants_server {
            return Ok(None);
        }
        match &self.pairing {
            Pairing::Paired(paired) => Ok(Some(&paired.endpoint)),
            Pairing::Unpaired => Err(SetupError::ServerNotPaired),
            Pairing::Unusable => Err(SetupError::PairingUnusable {
                path: InferencePaths::in_home(&self.home).client_identity(),
            }),
        }
    }

    /// What a run still has to put in place; empty once setup is done.
    /// A daemon that is down or still loading says nothing about the
    /// grant, so only a serving one can be missing it.
    pub fn missing(&self) -> Vec<Missing> {
        let existing = &self.existing;
        let mut missing = Vec::new();
        let configured = existing.stt.is_some()
            && existing.tts.is_some()
            && existing.chat.is_some()
            && existing.wake.is_some()
            && existing.hotkey.is_some();
        if !configured || self.server_address_is_stale() {
            missing.push(Missing::Config);
        }
        if self.wakeword_bundled && !self.wakeword_installed {
            missing.push(Missing::Wakeword);
        }
        let configured_engine = |engine: &str| {
            existing.stt.as_deref() == Some(engine) || existing.tts.as_deref() == Some(engine)
        };
        missing.extend(
            self.models
                .iter()
                .filter(|model| {
                    model.on_disk != OnDisk::Complete
                        && !model.external
                        && model.engine.is_some_and(configured_engine)
                })
                .map(|model| Missing::Model(model.name)),
        );
        if self.service.job == Job::Absent {
            missing.push(Missing::Service);
        }
        let serving = matches!(self.service.daemon, DaemonState::Ready { .. });
        if existing.hotkey == Some(true) && serving && self.grant != Grant::Armed {
            missing.push(Missing::Grant);
        }
        missing
    }
}

/// Every model setup knows, in the order it offers them: what the speech
/// engines want first, then what every install wants.
fn model_states(home: &Path, file: &FileConfig) -> Vec<ModelState> {
    let models_dir = config::models_dir(home);
    let loaded = config::loaded_models(file, home).models;
    // Only the asset's own section counts. Listen's custom moonshine path
    // does not change where transcription looks.
    let external = |asset: &Asset, engine: Option<&str>, section: &str| {
        let mut section = loaded.iter().filter(|model| model.section == section);
        let configured = section.clone().any(|model| model.engine == engine);
        configured && !section.any(|model| assets::holds(&models_dir, asset, &model.path))
    };
    fn by_engine<B: Backend>(
        section: &'static str,
    ) -> impl Iterator<Item = (&'static Asset, Option<&'static str>, &'static str)> {
        B::ALL.iter().flat_map(move |backend| {
            backend
                .model()
                .assets()
                .iter()
                .map(move |asset| (asset, Some(backend.key()), section))
        })
    }
    by_engine::<Stt>("transcription")
        .chain(by_engine::<Tts>("tts"))
        .chain(EXTRAS.iter().map(|asset| (asset, None, "speaker")))
        .map(|(asset, engine, section)| ModelState {
            name: asset.name,
            title: asset.title,
            engine,
            bytes: asset.total_bytes(),
            default: asset.default,
            on_disk: OnDisk::of(&models_dir, asset),
            external: external(asset, engine, section),
            asset,
        })
        .collect()
}

impl Selections {
    /// What `--yes` answers: a configured value verbatim, even one setup
    /// does not know, since it is still the user's choice; otherwise the
    /// default choice.
    pub fn defaults(inspection: &Inspection) -> Self {
        let existing = &inspection.existing;
        let configured = |current: &Option<String>, choices: &[Choice]| {
            current
                .clone()
                .unwrap_or_else(|| default_choice(choices).key.to_owned())
        };
        Self {
            stt: configured(&existing.stt, &inspection.stt),
            tts: configured(&existing.tts, &inspection.tts),
            chat: configured(&existing.chat, &inspection.chat),
            wake: existing.wake.unwrap_or(false),
            hotkey: inspection.macos && existing.hotkey.unwrap_or(true),
        }
    }
}

fn default_choice(choices: &[Choice]) -> &Choice {
    choices
        .iter()
        .find(|choice| choice.default)
        .unwrap_or(&choices[0])
}

/// Installs the wake-word models, fetches the accepted downloads, writes the
/// config, registers the service, and waits for the hotkey to arm.
/// `observe` gets each step and download as it happens; an error from it
/// ends the run at that point, a download included. The voice and the
/// test press are left for the caller: each needs a person.
pub fn apply(
    selections: &Selections,
    options: &ApplyOptions,
    inspection: &Inspection,
    observe: &mut dyn FnMut(SetupEvent) -> Result<(), SetupError>,
) -> Result<Summary, SetupError> {
    if let Some(unknown) = options.downloads.iter().find(|name| {
        !inspection
            .wanted(selections)
            .any(|model| model.name == name.as_str())
    }) {
        return Err(SetupError::UnknownModel {
            name: unknown.clone(),
        });
    }
    let endpoint = inspection.server_endpoint(selections)?;
    let _lock = lock_setup(&inspection.config_path)?;

    observe(SetupEvent::Started(Step::Config))?;
    // Staged here so a bad config fails before any download. Staged again
    // after the models, from a fresh read, so a failed download leaves the
    // config untouched and the write keeps edits made meanwhile.
    selections.prepare(&inspection.config_path, endpoint)?;

    let models_dir = config::models_dir(&inspection.home);
    observe(SetupEvent::Started(Step::Wakeword))?;
    let wakeword = install_wakeword(&inspection.exe, &models_dir.join(WAKEWORD_DIR))?;

    observe(SetupEvent::Started(Step::Models))?;
    let mut models = Vec::new();
    for model in inspection.wanted(selections) {
        // Check the disk again: a model may have been removed since inspection.
        let outcome = if assets::present(&models_dir, model.asset) {
            Outcome::Present
        } else if options.downloads.iter().any(|name| name == model.name) {
            assets::obtain(
                &models_dir,
                model.asset,
                options.transfer_meter,
                &mut |received| {
                    observe(SetupEvent::Download {
                        model: model.name,
                        received,
                        total: model.bytes,
                    })
                },
            )?;
            Outcome::Downloaded
        } else {
            Outcome::Skipped
        };
        models.push((model.name, outcome));
    }
    let config = selections
        .prepare(&inspection.config_path, endpoint)?
        .commit()?;

    observe(SetupEvent::Started(Step::Service))?;
    let legacy_agent =
        supervisor::remove_legacy_wake_agent(&inspection.home).map_err(SetupError::Service)?;
    let service = Supervisor::open()
        .and_then(|supervisor| supervisor.install())
        .map_err(SetupError::Service)?;
    let grant = if selections.hotkey {
        hotkey_grant(&service.socket, GRANT_WAIT)
    } else {
        Grant::NotNeeded
    };

    Ok(Summary {
        selections: selections.clone(),
        config,
        wakeword,
        models,
        legacy_agent,
        service,
        enrolled: Enrolled::NotOffered,
        tried: TryOut::NotOffered,
        grant,
    })
}

pub fn run(
    options: Options,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Summary, SetupError> {
    let inspection = inspect()?;
    let mut prompts = Prompts::new(
        input,
        output,
        options.yes,
        options.interactive,
        options.color,
    );

    prompts.heading("aria setup")?;
    prompts.note(&format!(
        "Answers are written to {}; Enter keeps the current one.",
        inspection.config_path.display()
    ))?;
    let selections = if options.yes {
        Selections::defaults(&inspection)
    } else {
        ask_selections(&mut prompts, &inspection)?
    };
    // Checked before the download questions, so none are asked for nothing.
    inspection.server_endpoint(&selections)?;

    let mut downloads = Vec::new();
    if !options.skip_models {
        for model in inspection.downloads_for(&selections) {
            // Only the transfer knows how much a resumed download has left.
            let bytes = match model.on_disk {
                OnDisk::Partial => format!("up to {}", size(model.bytes)),
                OnDisk::Complete | OnDisk::Absent => size(model.bytes),
            };
            let question = format!("Download {} ({bytes})?", model.title);
            if prompts.confirm(&question, model.default)? {
                downloads.push(model.name.to_owned());
            }
        }
    }
    let apply_options = ApplyOptions {
        downloads,
        transfer_meter: options.interactive,
    };
    let mut summary = apply(&selections, &apply_options, &inspection, &mut |_| Ok(()))?;

    // Both remaining steps need a person and the daemon that owns the
    // microphone. `--yes` is the installer's unattended rerun, which must
    // never sit waiting for someone to speak.
    let hands_on = options.interactive && !options.yes;
    let session = if hands_on {
        live_daemon(&mut prompts)?
    } else {
        None
    };
    (summary.enrolled, summary.tried) = match &session {
        Some(session) if voice_model_present(&summary.models) => (
            enroll_voice(&mut prompts, session)?,
            try_it_out(&mut prompts, session)?,
        ),
        Some(session) => (Enrolled::NotOffered, try_it_out(&mut prompts, session)?),
        None => (Enrolled::NotOffered, TryOut::NotOffered),
    };
    Ok(summary)
}

fn ask_selections(
    prompts: &mut Prompts<'_, impl BufRead, impl Write>,
    inspection: &Inspection,
) -> Result<Selections, SetupError> {
    let existing = &inspection.existing;
    Ok(Selections {
        stt: prompts.pick(Stt::TITLE, &inspection.stt)?.key.to_owned(),
        tts: prompts.pick(Tts::TITLE, &inspection.tts)?.key.to_owned(),
        chat: prompts.pick(Chat::TITLE, &inspection.chat)?.key.to_owned(),
        // With no model to load, a yes would leave the daemon unable to start
        // dictation. A hand-written choice stays as it is.
        wake: if inspection.wakeword_bundled || inspection.wakeword_installed {
            prompts.confirm("Listen for the wake word?", existing.wake.unwrap_or(false))?
        } else {
            existing.wake.unwrap_or(false)
        },
        hotkey: inspection.macos
            && prompts.confirm(
                "Start dictation with a hotkey (needs Accessibility access)?",
                existing.hotkey.unwrap_or(true),
            )?,
    })
}

/// Formats a byte count for display, like "2.5 GB".
pub fn size(bytes: u64) -> String {
    const GB: u64 = 1_000_000_000;
    const MB: u64 = 1_000_000;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else {
        format!("{} MB", bytes.div_ceil(MB))
    }
}

/// Whether the embedding model an enrollment needs is on disk. Without it
/// there is nothing to record a voice against.
fn voice_model_present(models: &[(&'static str, Outcome)]) -> bool {
    models
        .iter()
        .any(|(name, outcome)| *name == SPEAKER_ASSET && *outcome != Outcome::Skipped)
}

/// The daemon setup just installed, once it answers. `None` is ordinary:
/// the steps that need one skip themselves rather than fail the run.
fn live_daemon(
    prompts: &mut Prompts<'_, impl BufRead, impl Write>,
) -> Result<Option<DaemonSession>, SetupError> {
    let paths = match PreparedRuntimePaths::for_current_user() {
        Ok(paths) => paths,
        Err(error) => {
            prompts.note(&format!(
                "Skipping the voice steps: {}",
                error_chain(&error)
            ))?;
            return Ok(None);
        }
    };
    match DaemonSession::connect_at(&paths) {
        Ok(session) => Ok(Some(session)),
        Err(error) => {
            prompts.note(&format!(
                "Skipping the voice steps: {}",
                error_chain(&error)
            ))?;
            Ok(None)
        }
    }
}

/// Asks for a voice and records it through the daemon that was just
/// installed. Every way this can go wrong is an answer, not an error —
/// setup's job is the config, and it is already written.
fn enroll_voice(
    prompts: &mut Prompts<'_, impl BufRead, impl Write>,
    session: &DaemonSession,
) -> Result<Enrolled, SetupError> {
    // A rerun must not nag someone who already has a voice stored.
    match session.list_speakers().as_deref() {
        Ok([]) => {}
        Ok(_) => return Ok(Enrolled::NotOffered),
        Err(error) => {
            return Ok(Enrolled::Failed(format!(
                "could not list the enrolled voices: {}",
                error_chain(error)
            )));
        }
    }

    prompts.heading("Your voice")?;
    prompts.note("Aria can tell who is speaking once it knows your voice.")?;
    if !prompts.confirm("Record it now?", true)? {
        return Ok(Enrolled::Declined);
    }
    // A bare Enter, or end of input, is a second chance to back out.
    let name = prompts.line("Your name, as transcripts should show it:")?;
    if name.is_empty() {
        return Ok(Enrolled::Declined);
    }
    let name = match SpeakerName::new(name) {
        Ok(name) => name,
        Err(error) => return Ok(Enrolled::Failed(error.to_string())),
    };

    let request = EnrollmentRequest {
        name,
        // The daemon gives the first voice on a machine owner, and every one
        // after guest. That rule is what setup wants.
        role: None,
        samples: guided::DEFAULT_SAMPLES,
        replace: false,
    };
    Ok(match guided::enroll(session, request) {
        Ok(enrolled) => Enrolled::Done {
            name: enrolled.name.to_string(),
        },
        Err(error) => Enrolled::Failed(error.to_string()),
    })
}

/// One press and one reply, so the run ends on something that worked rather
/// than on a list of what was written. Nothing is typed anywhere: the
/// transcript comes back here and is printed.
fn try_it_out(
    prompts: &mut Prompts<'_, impl BufRead, impl Write>,
    session: &DaemonSession,
) -> Result<TryOut, SetupError> {
    prompts.heading("Try it")?;
    prompts.note("Nothing is typed into other apps.")?;
    if !prompts.confirm("Record a test press?", true)? {
        return Ok(TryOut::Declined);
    }

    prompts.note("Say a few words.")?;
    let heard = match session.transcribe(StopMode::OnSilence, |_stage| {}) {
        Ok(Some(heard)) => heard.text,
        // This request only stopped a recording already running, so nothing was heard.
        Ok(None) => return Ok(TryOut::Failed("the recording was stopped".to_owned())),
        Err(error) => return Ok(TryOut::Failed(error.to_string())),
    };
    prompts.note(&format!("heard: {heard}"))?;

    let reply = SpeechText::new(SPOKEN_REPLY)
        .map_err(|error| error_chain(&error))
        .and_then(|text| {
            session
                .speak(SpeechRequest {
                    text,
                    voice: None,
                    rate: None,
                })
                .map_err(|error| error_chain(&error))
        });
    let spoke = match reply {
        Ok(_) => true,
        Err(reason) => {
            prompts.note(&format!("The voice did not play: {reason}"))?;
            false
        }
    };
    Ok(TryOut::Done { heard, spoke })
}

/// Whether the daemon's hotkey listener has armed, giving it `wait` to do
/// so. Anything short of an armed listener reads as the grant being owed.
pub fn hotkey_grant(socket: &Path, wait: Duration) -> Grant {
    let deadline = Instant::now() + wait;
    loop {
        if let Ok(Liveness::Ready(mut client)) = probe_daemon(socket)
            && client.check_service_ready(HOTKEY_HEALTH_SERVICE).is_ok()
        {
            return Grant::Armed;
        }
        if Instant::now() >= deadline {
            return Grant::Waiting;
        }
        std::thread::sleep(GRANT_POLL);
    }
}

/// Copies the bundled wake-word models into `dest`, or does nothing when
/// none ship beside the binary. A file already there is left alone: the
/// classifier path is the one a user retrains, and a rerun of setup must
/// not overwrite their model.
/// Two runs would race on the config and the service.
fn lock_setup(config_path: &Path) -> Result<fs::File, SetupError> {
    let path = config_path.with_file_name("setup.lock");
    let io_error = |source| SetupError::Io {
        action: "lock",
        path: path.clone(),
        source,
    };
    if let Some(directory) = path.parent() {
        fs::create_dir_all(directory).map_err(io_error)?;
    }
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&path)
        .map_err(io_error)?;
    lock.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => SetupError::Busy,
        fs::TryLockError::Error(source) => io_error(source),
    })?;
    Ok(lock)
}

fn install_wakeword(exe: &Path, dest: &Path) -> Result<Option<PathBuf>, SetupError> {
    let Some(source) = bundled_wakeword_dir(exe) else {
        return Ok(None);
    };
    prepare(dest)?;
    for file in WAKEWORD_FILES {
        let to = dest.join(file);
        if to.exists() {
            continue;
        }
        let from = source.join(file);
        fs::copy(&from, &to).map_err(|source| SetupError::Io {
            action: "copy",
            path: from,
            source,
        })?;
    }
    Ok(Some(dest.to_path_buf()))
}

fn bundled_wakeword_dir(exe: &Path) -> Option<PathBuf> {
    let beside = exe.parent()?;
    BUNDLED_WAKEWORD_DIRS
        .iter()
        .map(|relative| beside.join(relative))
        .find(|dir| dir.join(WAKEWORD_FILES[0]).is_file())
}

fn prepare(dir: &Path) -> Result<PathBuf, SetupError> {
    fs::create_dir_all(dir).map_err(|source| SetupError::Io {
        action: "create",
        path: dir.to_path_buf(),
        source,
    })?;
    Ok(dir.to_path_buf())
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error(transparent)]
    Home(UserHomeError),

    #[error("failed to locate the running aria binary")]
    CurrentExe(#[source] io::Error),

    #[error("failed to watch for Ctrl-C")]
    Signal(#[source] io::Error),

    #[error("failed to {action} {}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to read the answer")]
    Prompt(#[source] io::Error),

    #[error("setup knows no model named {name}")]
    UnknownModel { name: String },

    #[error("no inference server is paired; run `aria remote pair` first")]
    ServerNotPaired,

    #[error("the inference pairing in {} is unusable; remove it, then pair again", path.display())]
    PairingUnusable { path: PathBuf },

    #[error("another setup is already running")]
    Busy,

    #[error("{name} is in use by `{section}` in config.yaml; change that section first")]
    ModelInUse {
        name: &'static str,
        section: &'static str,
    },

    #[error("`{section}` in config.yaml does not load; fix it first (`aria doctor` says why)")]
    ConfigUnresolved { section: &'static str },

    #[error("{name} is not on disk")]
    ModelNotInstalled { name: &'static str },

    #[error("failed to read the setup request")]
    Request(#[source] io::Error),

    #[error("the setup request is malformed")]
    Decode(#[source] prost::DecodeError),

    #[error("the setup request is invalid: {0}")]
    Invalid(&'static str),

    #[error("failed to write the setup message")]
    Respond(#[source] io::Error),

    #[error("nothing can answer a prompt here; pass --yes to take every default")]
    NotATerminal,

    #[error("failed to run {command}; is it installed?")]
    Spawn {
        command: String,
        #[source]
        source: io::Error,
    },

    #[error("failed to wait for {command}")]
    Wait {
        command: String,
        #[source]
        source: ChildProcessError,
    },

    #[error("{command} exited with {status}")]
    Command { command: String, status: ExitStatus },

    #[error("interrupted; rerun `aria setup` to resume {}", part.display())]
    Interrupted { part: PathBuf },

    #[error(
        "the download from {url} has SHA-256 {actual}, not {expected}; rerun `aria setup` to fetch it again"
    )]
    WrongHash {
        url: &'static str,
        expected: &'static str,
        actual: String,
    },

    #[error("downloaded {} failed the model trust check", path.display())]
    Untrusted {
        path: PathBuf,
        #[source]
        source: ModelError,
    },

    #[error(transparent)]
    Service(SupervisorError),

    #[error("failed to read {}", path.display())]
    Config {
        path: PathBuf,
        #[source]
        source: ConfigError,
    },

    #[error("failed to write {}", path.display())]
    ConfigWrite {
        path: PathBuf,
        #[source]
        source: serde_saphyr::ser::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    /// Setup must put the speaker, segmentation, and wake-word models where the
    /// daemon loads them.
    #[test]
    fn shared_models_land_where_the_daemon_loads_them() {
        use crate::config::{
            DaemonConfig, Environment, SpeakerConfig, WakeConfig, WakeWordOverride,
        };

        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".aria/config.yaml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "wake: {enabled: true}\n").unwrap();
        let environment = Environment::in_home(home.path().to_path_buf());
        let extras: Vec<PathBuf> = EXTRAS
            .iter()
            .flat_map(Asset::files)
            .map(|(relative, _)| config::models_dir(home.path()).join(relative))
            .collect();

        let speaker = SpeakerConfig::load(&environment).unwrap();
        assert!(
            extras.contains(&speaker.model),
            "{}",
            speaker.model.display()
        );
        assert!(extras.contains(&speaker.segmentation_model));

        let daemon = DaemonConfig::load(&environment, WakeWordOverride::Inherit).unwrap();
        let WakeConfig::Enabled { classifier, .. } = daemon.wake else {
            panic!("the config enables the wake word");
        };
        assert_eq!(
            classifier.parent(),
            Some(config::models_dir(home.path()).join(WAKEWORD_DIR).as_path())
        );
        assert!(WAKEWORD_FILES.iter().any(|file| classifier.ends_with(file)));
    }

    #[test]
    fn bundled_models_are_found_in_the_app_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("Aria.app/Contents/MacOS/aria");
        let models = dir
            .path()
            .join("Aria.app/Contents/Resources/models/wakeword");
        touch(&exe);
        touch(&models.join("aria.onnx"));

        let found = bundled_wakeword_dir(&exe).unwrap();

        assert_eq!(
            found.canonicalize().unwrap(),
            models.canonicalize().unwrap()
        );
    }

    #[test]
    fn bundled_models_are_found_in_a_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("target/release/aria");
        let models = dir.path().join("models/wakeword");
        touch(&exe);
        touch(&models.join("aria.onnx"));

        let found = bundled_wakeword_dir(&exe).unwrap();

        assert_eq!(
            found.canonicalize().unwrap(),
            models.canonicalize().unwrap()
        );
    }

    #[test]
    fn a_bare_binary_has_no_bundled_models() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aria");
        touch(&exe);

        assert!(bundled_wakeword_dir(&exe).is_none());
    }

    #[test]
    fn install_copies_every_wakeword_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("opt/aria/bin/aria");
        touch(&exe);
        for file in WAKEWORD_FILES {
            touch(&dir.path().join("opt/aria/share/aria/wakeword").join(file));
        }
        let dest = dir.path().join("home/models/wakeword");

        assert_eq!(install_wakeword(&exe, &dest).unwrap(), Some(dest.clone()));

        for file in WAKEWORD_FILES {
            assert!(dest.join(file).is_file(), "{file}");
        }
    }

    #[test]
    fn install_skips_when_no_models_ship() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("aria");
        touch(&exe);
        let dest = dir.path().join("home/models/wakeword");

        assert_eq!(install_wakeword(&exe, &dest).unwrap(), None);
        assert!(!dest.exists());
    }

    #[test]
    fn a_user_trained_wake_model_survives_a_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("opt/aria/bin/aria");
        touch(&exe);
        let bundled = dir.path().join("opt/aria/share/aria/wakeword");
        fs::create_dir_all(&bundled).unwrap();
        for file in WAKEWORD_FILES {
            fs::write(bundled.join(file), b"bundled").unwrap();
        }
        let dest = dir.path().join("home/models/wakeword");
        fs::create_dir_all(&dest).unwrap();
        fs::write(dest.join("aria.onnx"), b"mine").unwrap();

        install_wakeword(&exe, &dest).unwrap();

        assert_eq!(fs::read(dest.join("aria.onnx")).unwrap(), b"mine");
        assert_eq!(
            fs::read(dest.join("melspectrogram.onnx")).unwrap(),
            b"bundled"
        );
    }

    fn bare_host() -> Host {
        Host {
            macos: true,
            system_voice: Availability::Ready,
        }
    }

    /// A machine with nothing on it but what `existing` says.
    fn inspection(existing: Existing) -> Inspection {
        let host = bare_host();
        let home = PathBuf::from("/nonexistent");
        Inspection {
            config_path: PathBuf::from("/nonexistent/config.yaml"),
            macos: true,
            stt: choices::<Stt>(existing.stt.as_deref(), &host),
            tts: choices::<Tts>(existing.tts.as_deref(), &host),
            chat: choices::<Chat>(existing.chat.as_deref(), &host),
            models: model_states(&home, &FileConfig::default()),
            wakeword_installed: false,
            wakeword_bundled: true,
            pairing: Pairing::Unpaired,
            configured_server: None,
            existing,
            service: Status {
                unit: PathBuf::from("/nonexistent/aria.plist"),
                app_agent: false,
                job: Job::Absent,
                socket: PathBuf::from("/nonexistent/daemon.sock"),
                daemon: supervisor::DaemonState::NotRunning,
            },
            grant: Grant::NotNeeded,
            home,
            exe: PathBuf::from("/nonexistent/aria"),
        }
    }

    fn writable_inspection(home: &Path) -> Inspection {
        let mut inspection = inspection(configured());
        inspection.config_path = home.join(".aria/config.yaml");
        inspection
    }

    fn no_downloads() -> ApplyOptions {
        ApplyOptions {
            downloads: Vec::new(),
            transfer_meter: false,
        }
    }

    fn configured() -> Existing {
        Existing {
            stt: Some("whisper".to_owned()),
            tts: Some("kokoro".to_owned()),
            chat: Some("codex".to_owned()),
            wake: Some(false),
            hotkey: Some(true),
        }
    }

    /// The installer reruns `setup --yes` on every upgrade. A choice the
    /// file already carries has to survive that, model on disk or not.
    #[test]
    fn yes_keeps_a_configured_engine_whose_model_is_absent() {
        let existing = Existing {
            tts: Some("qwen".to_owned()),
            ..Existing::default()
        };

        assert_eq!(Selections::defaults(&inspection(existing)).tts, "qwen");
    }

    /// A backend from a newer build, or a hand-written name, is still the
    /// user's; setup has no standing to replace it unattended.
    #[test]
    fn yes_keeps_an_engine_setup_does_not_know() {
        let existing = Existing {
            tts: Some("future-tts".to_owned()),
            ..Existing::default()
        };

        assert_eq!(
            Selections::defaults(&inspection(existing)).tts,
            "future-tts"
        );
    }

    #[test]
    fn yes_takes_the_default_choice_when_nothing_is_configured() {
        let defaults = Selections::defaults(&inspection(Existing::default()));

        assert_eq!(defaults.stt, config::backend::DEFAULT);
        assert_eq!(defaults.tts, config::tts_backend::DEFAULT);
        assert!(!defaults.wake);
        assert!(defaults.hotkey);
    }

    #[test]
    fn server_selection_needs_no_local_transcription_download() {
        let mut inspection = inspection(Existing {
            stt: Some("server".to_owned()),
            tts: Some("system".to_owned()),
            ..configured()
        });
        let file = serde_saphyr::from_str(
            "transcription: {backend: server, server: https://example.test:9851}",
        )
        .unwrap();
        inspection.models = model_states(&inspection.home, &file);
        let selections = Selections::defaults(&inspection);
        assert_eq!(selections.stt, "server");
        assert_eq!(
            inspection
                .stt
                .iter()
                .find(|choice| choice.default)
                .unwrap()
                .key,
            "server"
        );
        assert!(
            inspection
                .downloads_for(&selections)
                .iter()
                .all(|model| model.engine.is_none())
        );
        assert!(
            !inspection
                .missing()
                .iter()
                .any(|missing| matches!(missing, Missing::Model(_)))
        );
    }

    #[test]
    fn an_engine_setup_does_not_know_asks_for_nothing_to_download() {
        let inspection = inspection(Existing::default());
        let names = |selections: &Selections| -> Vec<&str> {
            inspection
                .downloads_for(selections)
                .into_iter()
                .map(|model| model.name)
                .collect()
        };
        let mut selections = Selections::defaults(&inspection);

        assert_eq!(
            names(&selections),
            [config::backend::DEFAULT, SPEAKER_ASSET, "segmentation"]
        );

        selections.stt = "future-stt".to_owned();
        selections.tts = "kokoro".to_owned();
        assert_eq!(
            names(&selections),
            ["kokoro", SPEAKER_ASSET, "segmentation"]
        );
    }

    #[test]
    fn a_fresh_machine_is_missing_everything_but_the_grant() {
        let inspection = inspection(Existing::default());

        assert_eq!(
            inspection.missing(),
            [Missing::Config, Missing::Wakeword, Missing::Service]
        );
    }

    /// Only the configured engines' models count: the extras are optional,
    /// and an engine nobody chose owes nothing.
    #[test]
    fn a_configured_machine_is_missing_its_models() {
        let inspection = inspection(configured());

        assert_eq!(
            inspection.missing(),
            [
                Missing::Wakeword,
                Missing::Model("whisper"),
                Missing::Model("kokoro"),
                Missing::Service,
            ]
        );
    }

    /// A model config points elsewhere is the user's: setup neither fetches it
    /// nor calls it missing. Kokoro still needs its default voices, so it
    /// stays wanted.
    #[test]
    fn an_engine_with_its_own_model_path_wants_no_download() {
        let mut inspection = inspection(Existing {
            stt: Some("parakeet".to_owned()),
            ..configured()
        });
        let file = serde_saphyr::from_str(
            "transcription: {backend: parakeet, model: /elsewhere/tdt}\n\
             tts: {backend: kokoro, model: /elsewhere/kokoro.onnx}",
        )
        .unwrap();
        inspection.models = model_states(&inspection.home, &file);
        let selections = Selections::defaults(&inspection);
        let wanted: Vec<_> = inspection
            .downloads_for(&selections)
            .iter()
            .map(|model| model.name)
            .collect();

        assert!(!wanted.contains(&"parakeet"), "{wanted:?}");
        assert!(!inspection.missing().contains(&Missing::Model("parakeet")));
        assert!(inspection.missing().contains(&Missing::Model("kokoro")));
    }

    /// Listen's custom moonshine path does not cover transcription, which
    /// loads the default one.
    #[test]
    fn a_model_kept_elsewhere_for_listen_still_downloads_for_transcription() {
        let mut inspection = inspection(Existing {
            stt: Some("moonshine".to_owned()),
            ..configured()
        });
        let file = serde_saphyr::from_str(
            "transcription: {backend: whisper}\n\
             listen: {backend: moonshine, model: /elsewhere/moonshine}",
        )
        .unwrap();
        inspection.models = model_states(&inspection.home, &file);
        let selections = Selections::defaults(&inspection);

        let wanted: Vec<_> = inspection
            .downloads_for(&selections)
            .iter()
            .map(|model| model.name)
            .collect();

        assert!(wanted.contains(&"moonshine"), "{wanted:?}");
    }

    #[test]
    fn the_wake_word_is_offered_only_with_a_model() {
        // Enter keeps the three engines, then two yeses.
        let answers = b"\n\n\ny\ny\n";
        let ask = |inspection: &Inspection| {
            let (mut input, mut output) = (&answers[..], Vec::new());
            let mut prompts = Prompts::new(&mut input, &mut output, false, true, false);
            ask_selections(&mut prompts, inspection).unwrap()
        };

        let mut inspection = inspection(Existing::default());
        assert!(ask(&inspection).wake);

        inspection.wakeword_bundled = false;
        let selections = ask(&inspection);
        assert!(!selections.wake);
        assert!(selections.hotkey);
    }

    /// Setup cannot install models that don't ship, so it doesn't owe them.
    #[test]
    fn unbundled_wake_models_are_not_missing() {
        let mut inspection = inspection(Existing::default());
        inspection.wakeword_bundled = false;

        assert!(!inspection.missing().contains(&Missing::Wakeword));
    }

    /// A daemon that is down says nothing about the grant; a serving one
    /// without an armed hotkey owes it.
    #[test]
    fn only_a_serving_daemon_can_be_missing_the_grant() {
        let mut inspection = inspection(configured());
        inspection.service.daemon = DaemonState::Starting { pid: None };
        assert!(!inspection.missing().contains(&Missing::Grant));

        inspection.service.daemon = DaemonState::Ready { pid: None };
        assert!(inspection.missing().contains(&Missing::Grant));

        inspection.grant = Grant::Armed;
        assert!(!inspection.missing().contains(&Missing::Grant));
    }

    #[test]
    fn nothing_is_missing_once_everything_is_in_place() {
        let mut inspection = inspection(configured());
        for model in &mut inspection.models {
            model.on_disk = OnDisk::Complete;
        }
        inspection.wakeword_installed = true;
        inspection.service.job = Job::Idle;
        inspection.grant = Grant::Armed;

        assert!(inspection.missing().is_empty());
    }

    /// Refused before anything is written: a name setup has never heard
    /// of, and one the chosen engines do not want.
    #[test]
    fn apply_refuses_a_model_it_does_not_want() {
        let inspection = inspection(configured());
        let selections = Selections::defaults(&inspection);
        let refused = |name: &str| {
            let options = ApplyOptions {
                downloads: vec!["whisper".to_owned(), name.to_owned()],
                transfer_meter: false,
            };
            let result = apply(&selections, &options, &inspection, &mut |_| Ok(()));
            matches!(result, Err(SetupError::UnknownModel { name: ref unknown }) if unknown == name)
        };

        assert!(refused("gpt"));
        assert!(refused("qwen"), "wanted only by an engine nobody chose");
    }

    #[test]
    fn apply_refuses_the_server_until_one_is_usably_paired() {
        let home = tempfile::tempdir().unwrap();
        let mut inspection = writable_inspection(home.path());
        let mut selections = Selections::defaults(&inspection);
        selections.stt = "server".to_owned();

        let result = apply(&selections, &no_downloads(), &inspection, &mut |_| Ok(()));
        assert!(matches!(result, Err(SetupError::ServerNotPaired)));

        inspection.pairing = Pairing::Unusable;
        let result = apply(&selections, &no_downloads(), &inspection, &mut |_| Ok(()));
        assert!(matches!(result, Err(SetupError::PairingUnusable { .. })));

        inspection.pairing = paired("https://box.test:9851");
        let mut steps = Vec::new();
        let _ = apply(&selections, &no_downloads(), &inspection, &mut |event| {
            steps.push(event);
            Err(SetupError::Invalid("stop after the config check"))
        });
        assert_eq!(steps, [SetupEvent::Started(Step::Config)]);
    }

    #[test]
    fn a_second_apply_is_refused_while_one_runs() {
        let home = tempfile::tempdir().unwrap();
        let inspection = writable_inspection(home.path());
        let _running = lock_setup(&inspection.config_path).unwrap();

        let result = apply(
            &Selections::defaults(&inspection),
            &no_downloads(),
            &inspection,
            &mut |_| Ok(()),
        );
        assert!(matches!(result, Err(SetupError::Busy)));
    }

    #[test]
    fn a_config_naming_another_server_than_the_pin_needs_writing() {
        let mut inspection = inspection(Existing {
            stt: Some("server".to_owned()),
            ..configured()
        });
        inspection.pairing = paired("https://new.test:9851");

        inspection.configured_server = Some("https://old.test:9851".to_owned());
        assert!(inspection.missing().contains(&Missing::Config));

        inspection.configured_server = Some("https://new.test:9851".to_owned());
        assert!(!inspection.missing().contains(&Missing::Config));
    }

    fn paired(endpoint: &str) -> Pairing {
        Pairing::Paired(PairedServer {
            endpoint: ServerEndpoint::parse(endpoint).unwrap(),
            client_fingerprint: "ab".to_owned(),
        })
    }

    /// The observer is the app watching; when it is gone, the run stops
    /// where it is instead of finishing unwatched.
    #[test]
    fn apply_stops_when_the_observer_fails() {
        let home = tempfile::tempdir().unwrap();
        let inspection = writable_inspection(home.path());
        let mut seen = Vec::new();

        let result = apply(
            &Selections::defaults(&inspection),
            &no_downloads(),
            &inspection,
            &mut |event| {
                seen.push(event);
                Err(SetupError::Respond(io::Error::from(
                    io::ErrorKind::BrokenPipe,
                )))
            },
        );

        assert!(matches!(result, Err(SetupError::Respond(_))));
        assert_eq!(seen, [SetupEvent::Started(Step::Config)]);
        assert!(!inspection.config_path.exists(), "nothing was written");
    }

    #[test]
    fn a_run_stopped_at_the_models_leaves_the_config_alone() {
        let home = tempfile::tempdir().unwrap();
        let inspection = writable_inspection(home.path());

        let result = apply(
            &Selections::defaults(&inspection),
            &no_downloads(),
            &inspection,
            &mut |event| match event {
                SetupEvent::Started(Step::Models) => Err(SetupError::Respond(io::Error::from(
                    io::ErrorKind::BrokenPipe,
                ))),
                _ => Ok(()),
            },
        );

        assert!(matches!(result, Err(SetupError::Respond(_))));
        assert!(
            !inspection.config_path.exists(),
            "the config waits for the models"
        );
    }

    #[test]
    fn an_edit_during_the_downloads_survives_the_config_write() {
        let home = tempfile::tempdir().unwrap();
        let inspection = writable_inspection(home.path());

        let result = apply(
            &Selections::defaults(&inspection),
            &no_downloads(),
            &inspection,
            &mut |event| match event {
                SetupEvent::Started(Step::Models) => {
                    fs::write(&inspection.config_path, "tts:\n  rate: 1.5\n").unwrap();
                    Ok(())
                }
                SetupEvent::Started(Step::Service) => Err(SetupError::Respond(io::Error::from(
                    io::ErrorKind::BrokenPipe,
                ))),
                _ => Ok(()),
            },
        );

        assert!(matches!(result, Err(SetupError::Respond(_))));
        let text = fs::read_to_string(&inspection.config_path).unwrap();
        assert!(text.contains("rate: 1.5"), "{text}");
    }

    #[test]
    fn sizes_read_like_the_download_page() {
        assert_eq!(size(874_188_075), "875 MB");
        assert_eq!(size(1_500_000_000), "1.5 GB");
        assert_eq!(size(6_958_444), "7 MB");
    }

    fn options(yes: bool, interactive: bool) -> Options {
        Options {
            yes,
            skip_models: false,
            interactive,
            color: false,
        }
    }

    /// Recording needs someone to speak, so every unattended shape skips
    /// both hands-on steps on their behalf.
    #[test]
    fn nobody_is_asked_to_speak_without_a_terminal_to_ask_at() {
        let hands_on = |options: Options| options.interactive && !options.yes;

        assert!(hands_on(options(false, true)));
        assert!(!hands_on(options(false, false)), "a pipe");
        assert!(!hands_on(options(true, true)), "--yes");
    }

    #[test]
    fn no_model_means_no_voice_to_record_against() {
        assert!(voice_model_present(&[(SPEAKER_ASSET, Outcome::Downloaded)]));
        assert!(voice_model_present(&[(SPEAKER_ASSET, Outcome::Present)]));

        assert!(!voice_model_present(&[(SPEAKER_ASSET, Outcome::Skipped)]));
        assert!(!voice_model_present(&[]), "nothing wanted, nothing there");
    }
}
