//! `aria setup`: the guided first run. `inspect` looks at what this
//! machine has. `apply` fetches the models the chosen engines need plus the
//! ones `aria listen` uses, writes the engines to settings.toml, and installs
//! the daemon as a service. Neither asks anything. `run` is the terminal front
//! over them; Aria.app drives the same two through `aria setup inspect` and
//! `aria setup apply`. Every answer defaults to what is configured, and
//! `--yes` keeps every configured engine, so the unattended rerun the
//! installer makes never rewrites a working choice. `transcription_engine`
//! sets the speech-to-text engine alone, and `transcription_runs_on` where
//! it runs, each outside the rest of setup.
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

use crate::primitives::error_chain;
use crate::primitives::process::ChildProcessError;
use crate::primitives::user_home::{self, UserHomeError};
use crate::supervisor::{self, DaemonState, Job, Status, Supervisor, SupervisorError};
use crate::synthesis::SpeechText;
use aria_daemon::ipc::{HOTKEY_HEALTH_SERVICE, Liveness, probe_daemon};
use aria_daemon::{
    DaemonSession, EnrollmentRequest, Forgotten, InferencePaths, PairedServer, SpeakerName,
    StopMode, guided,
};
use aria_kit::backend::ModelError;
use aria_platform::hotkey::PlatformHotkeyListener;
use aria_platform::runtime::PreparedRuntimePaths;
use aria_settings::config::{backend, tts_backend};
use aria_settings::{CheckedSettings, RunsOn, SettingsError};
use aria_voice::config;
use aria_voice::speech::SpeechRequest;

mod assets;
mod backends;
mod download;
pub mod models;
mod prompt;
mod settings_file;
pub mod transcription_engine;
pub mod transcription_runs_on;
pub mod wire;

use assets::{Asset, EXTRAS, SPEAKER as SPEAKER_ASSET};
pub use backends::{Availability, Choice};
use backends::{Backend, Host, Stt, Tts, choices};
use models::ModelFiles;
use prompt::Prompts;
pub use settings_file::{Existing, Selections, SettingsWrite};

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
    /// Where the settings live: `~/.aria/settings.toml`.
    pub settings_path: PathBuf,
    pub macos: bool,
    pub existing: Existing,
    pub stt: Vec<Choice>,
    pub tts: Vec<Choice>,
    pub models: Vec<ModelState>,
    pub wakeword_installed: bool,
    pub service: Status,
    pub grant: Grant,
    pub key_remapped: bool,
    pub transcription_runs_on: RunsOn,
    /// Whether wake-word models ship beside the binary.
    wakeword_bundled: bool,
    pairing: Pairing,
    home: PathBuf,
    exe: PathBuf,
}

enum Pairing {
    Unpaired,
    Paired(PairedServer),
    /// The pin file can't be read; pairing again needs it removed.
    Unusable,
}

impl Pairing {
    fn read(home: &Path) -> Self {
        match aria_daemon::paired_server(home) {
            Ok(Some(paired)) => Self::Paired(paired),
            Ok(None) => Self::Unpaired,
            Err(error) => {
                tracing::warn!(error = %error_chain(&error), "inference pairing is unusable");
                Self::Unusable
            }
        }
    }

    fn check_paired(&self, home: &Path) -> Result<(), SetupError> {
        match self {
            Self::Paired(_) => Ok(()),
            Self::Unpaired => Err(SetupError::ServerNotPaired),
            Self::Unusable => Err(SetupError::PairingUnusable {
                path: InferencePaths::in_home(home).client_identity(),
            }),
        }
    }
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
    pub files: ModelFiles,
    pub used_by: Vec<config::Section>,
    pub label: &'static str,
    asset: &'static Asset,
}

/// One thing a run still has to put in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    Settings,
    Wakeword,
    Model(&'static str),
    Service,
    /// A serving daemon whose hotkey listener has not armed.
    Grant,
    /// Modifier Keys turns the Aria key into another key.
    KeyRemapped,
}

/// The steps `apply` announces as it starts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Settings,
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
    pub settings: SettingsWrite,
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

/// Hotkey readiness after requesting Accessibility access on macOS or keyboard access on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grant {
    Armed,
    /// The daemon runs but the listener has not armed; the grant is owed.
    Waiting,
}

/// Under setup's lock, so a run that switches to the server cannot slip in
/// between the check and the removal.
pub fn forget_server() -> Result<Forgotten, SetupError> {
    let home = user_home::resolve().map_err(SetupError::Home)?;
    forget_server_at(&home)
}

fn forget_server_at(home: &Path) -> Result<Forgotten, SetupError> {
    let _lock = lock_setup(&aria_settings::default_path(home))?;
    check_server_unused(home)?;
    aria_daemon::forget_paired_server(home).map_err(SetupError::Forget)
}

/// Pairs with the server an invitation from `input` names. The pin holds
/// the server's address, so it never changes under a daemon that uses it.
/// The invitation is read before the lock, which a person pasting it would
/// otherwise hold.
pub fn pair_server(input: impl io::Read) -> Result<String, SetupError> {
    let home = user_home::resolve().map_err(SetupError::Home)?;
    let invitation = aria_daemon::read_invitation(input).map_err(SetupError::Pair)?;
    pair_server_at(&home, &invitation)
}

fn pair_server_at(home: &Path, invitation: &str) -> Result<String, SetupError> {
    let _lock = lock_setup(&aria_settings::default_path(home))?;
    check_server_unused(home)?;
    aria_daemon::pair_server(home, invitation).map_err(SetupError::Pair)
}

fn check_server_unused(home: &Path) -> Result<(), SetupError> {
    match settings_file::read(home)?.choices().stt_runs_on {
        RunsOn::Server => Err(SetupError::ServerInUse),
        RunsOn::Local => Ok(()),
    }
}

pub fn inspect() -> Result<Inspection, SetupError> {
    let home = user_home::resolve().map_err(SetupError::Home)?;
    let models_dir = aria_settings::config::models_dir(&home);
    // `current_exe` may answer the symlink an installer put on PATH; the
    // bundled files sit beside the real binary.
    let exe = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(SetupError::CurrentExe)?;
    let settings_path = aria_settings::default_path(&home);
    let host = Host::probe();
    let file = settings_file::read(&home)?;
    let pairing = Pairing::read(&home);
    let existing = Existing::of(&file);
    let service = Supervisor::open()
        .and_then(|supervisor| supervisor.status())
        .map_err(SetupError::Service)?;
    let grant = hotkey_grant(&service.socket, Duration::ZERO);
    Ok(Inspection {
        settings_path,
        macos: host.macos,
        stt: choices::<Stt>(existing.stt.as_deref(), &host),
        tts: choices::<Tts>(existing.tts.as_deref(), &host),
        models: model_states(&home, &file)?,
        wakeword_installed: WAKEWORD_FILES
            .iter()
            .all(|file| models_dir.join(WAKEWORD_DIR).join(file).is_file()),
        wakeword_bundled: bundled_wakeword_dir(&exe).is_some(),
        pairing,
        transcription_runs_on: file.choices().stt_runs_on,
        key_remapped: PlatformHotkeyListener::is_remapped(file.aria_key()),
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
            .filter(|model| model.files.needs_download())
            .collect()
    }

    fn wanted<'a>(
        &'a self,
        selections: &'a Selections,
    ) -> impl Iterator<Item = &'a ModelState> + 'a {
        let stt = self.local_stt(&selections.stt);
        self.models.iter().filter(move |model| {
            model.files != ModelFiles::External
                && model
                    .engine
                    .is_none_or(|engine| Some(engine) == stt || engine == selections.tts)
        })
    }

    /// The local engine, while it transcribes. On the server its models
    /// wait: going back fetches them.
    fn local_stt<'a>(&self, engine: &'a str) -> Option<&'a str> {
        (self.transcription_runs_on == RunsOn::Local).then_some(engine)
    }

    /// The server transcribes, but no usable pin says where it is.
    fn server_is_unusable(&self) -> bool {
        self.transcription_runs_on == RunsOn::Server && !matches!(self.pairing, Pairing::Paired(_))
    }

    /// What a run still has to put in place; empty once setup is done.
    /// A daemon that is down or still loading says nothing about the
    /// grant, so only a serving one can be missing it.
    pub fn missing(&self) -> Vec<Missing> {
        let existing = &self.existing;
        let mut missing = Vec::new();
        let configured =
            existing.stt.is_some() && existing.tts.is_some() && existing.wake.is_some();
        if !configured || self.server_is_unusable() {
            missing.push(Missing::Settings);
        }
        if self.wakeword_bundled && !self.wakeword_installed {
            missing.push(Missing::Wakeword);
        }
        // An unset engine still runs as its default, so its model counts.
        let stt = self.local_stt(existing.stt.as_deref().unwrap_or(backend::DEFAULT));
        let tts = existing.tts.as_deref().unwrap_or(tts_backend::DEFAULT);
        let runs = |engine: &str| Some(engine) == stt || engine == tts;
        missing.extend(
            self.models
                .iter()
                .filter(|model| model.files.needs_download() && model.engine.is_some_and(runs))
                .map(|model| Missing::Model(model.name)),
        );
        if self.service.job == Job::Absent {
            missing.push(Missing::Service);
        }
        let serving = matches!(self.service.daemon, DaemonState::Ready { .. });
        if serving && self.grant != Grant::Armed {
            missing.push(Missing::Grant);
        }
        if self.key_remapped {
            missing.push(Missing::KeyRemapped);
        }
        missing
    }
}

/// Every model setup knows, in the order it offers them: what the speech
/// engines want first, then what every install wants.
fn model_states(home: &Path, file: &CheckedSettings) -> Result<Vec<ModelState>, SetupError> {
    let models_dir = aria_settings::config::models_dir(home);
    let loaded = config::loaded_models(file, home);
    // Only the asset's own section counts. Listen's custom moonshine path
    // does not change where transcription looks.
    let external = |asset: &Asset, engine: Option<&str>, section: config::Section| {
        let mut section = loaded.iter().filter(|model| model.section == section);
        let configured = section.clone().any(|model| model.engine == engine);
        configured && !section.any(|model| assets::holds(&models_dir, asset, &model.path))
    };
    fn by_engine<B: Backend>(
        section: config::Section,
    ) -> impl Iterator<
        Item = (
            &'static Asset,
            Option<&'static str>,
            &'static str,
            config::Section,
        ),
    > {
        B::ALL.iter().flat_map(move |&backend| {
            backend
                .model()
                .assets()
                .iter()
                .map(move |asset| (asset, Some(backend.key()), backend.label(), section))
        })
    }
    Ok(by_engine::<Stt>(config::Section::Stt)
        .chain(by_engine::<Tts>(config::Section::Tts))
        .chain(
            EXTRAS
                .iter()
                .map(|asset| (asset, None, asset.title, config::Section::Speaker)),
        )
        .map(|(asset, engine, label, section)| ModelState {
            name: asset.name,
            title: asset.title,
            engine,
            bytes: asset.total_bytes(),
            default: asset.default,
            files: if external(asset, engine, section) {
                ModelFiles::External
            } else {
                ModelFiles::of(&models_dir, asset)
            },
            used_by: models::users(&loaded, &models_dir, asset),
            label,
            asset,
        })
        .collect())
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
            wake: existing.wake.unwrap_or(false),
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
/// settings file, registers the service, and waits for the hotkey to arm.
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
    let _lock = lock_setup(&inspection.settings_path)?;

    observe(SetupEvent::Started(Step::Settings))?;
    // Checked here so a refused write fails before any download. Written
    // after the models, so a failed download leaves the file untouched and
    // the write keeps edits made meanwhile.
    selections.check(&inspection.home)?;

    let models_dir = aria_settings::config::models_dir(&inspection.home);
    observe(SetupEvent::Started(Step::Wakeword))?;
    let wakeword = install_wakeword(&inspection.exe, &models_dir.join(WAKEWORD_DIR))?;

    observe(SetupEvent::Started(Step::Models))?;
    let models = fetch(
        &models_dir,
        inspection.wanted(selections),
        |model| options.downloads.iter().any(|name| name == model.name),
        options.transfer_meter,
        observe,
    )?;
    let settings = selections.write(&inspection.home)?;

    observe(SetupEvent::Started(Step::Service))?;
    let legacy_agent =
        supervisor::remove_legacy_wake_agent(&inspection.home).map_err(SetupError::Service)?;
    let service = Supervisor::open()
        .and_then(|supervisor| supervisor.install())
        .map_err(SetupError::Service)?;
    let grant = hotkey_grant(&service.socket, GRANT_WAIT);

    Ok(Summary {
        selections: selections.clone(),
        settings,
        wakeword,
        models,
        legacy_agent,
        service,
        enrolled: Enrolled::NotOffered,
        tried: TryOut::NotOffered,
        grant,
    })
}

/// Fetches each of `models` that `downloads` picks. The disk is checked
/// again, since a model may have been removed since inspection.
fn fetch<'a>(
    models_dir: &Path,
    models: impl Iterator<Item = &'a ModelState>,
    downloads: impl Fn(&ModelState) -> bool,
    transfer_meter: bool,
    observe: &mut dyn FnMut(SetupEvent) -> Result<(), SetupError>,
) -> Result<Vec<(&'static str, Outcome)>, SetupError> {
    let mut fetched = Vec::new();
    for model in models {
        let outcome = if assets::present(models_dir, model.asset) {
            Outcome::Present
        } else if downloads(model) {
            assets::obtain(models_dir, model.asset, transfer_meter, &mut |received| {
                observe(SetupEvent::Download {
                    model: model.name,
                    received,
                    total: model.bytes,
                })
            })?;
            Outcome::Downloaded
        } else {
            Outcome::Skipped
        };
        fetched.push((model.name, outcome));
    }
    Ok(fetched)
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
        inspection.settings_path.display()
    ))?;
    let selections = if options.yes {
        Selections::defaults(&inspection)
    } else {
        ask_selections(&mut prompts, &inspection)?
    };
    let mut downloads = Vec::new();
    if !options.skip_models {
        for model in inspection.downloads_for(&selections) {
            // Only the transfer knows how much a resumed download has left.
            let bytes = if model.files == ModelFiles::Partial {
                format!("up to {}", size(model.bytes))
            } else {
                size(model.bytes)
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
        // With no model to load, a yes would leave the daemon unable to start
        // dictation. A hand-written choice stays as it is.
        wake: if inspection.wakeword_bundled || inspection.wakeword_installed {
            prompts.confirm("Listen for the wake word?", existing.wake.unwrap_or(false))?
        } else {
            existing.wake.unwrap_or(false)
        },
    })
}

/// Settings tables for display, like "stt, listen".
pub fn tables(sections: &[config::Section]) -> String {
    sections
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
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
/// setup's job is the settings file, and it is already written.
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
        // The daemon makes a new voice the owner while nobody owns the
        // machine. That rule is what setup wants.
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
/// Two runs would race on the settings file and the service.
fn lock_setup(settings_path: &Path) -> Result<fs::File, SetupError> {
    let path = settings_path.with_file_name("setup.lock");
    let io_error = |source| SetupError::Io {
        action: "lock",
        path: path.clone(),
        source,
    };
    // Setup runs before the daemon ever has, so it makes `~/.aria` private
    // before downloads put anything in it.
    if let Some(directory) = path.parent() {
        crate::primitives::private_fs::prepare_directory(directory).map_err(|error| {
            SetupError::Io {
                action: "secure",
                path: directory.to_path_buf(),
                source: io::Error::other(error),
            }
        })?;
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

    #[error("setup knows no speech-to-text engine named {name}")]
    UnknownEngine { name: String },

    #[error("{name} can't be set in place; choose it in `aria setup`")]
    NotSettable { name: &'static str },

    #[error("no inference server is paired; run `aria remote pair` first")]
    ServerNotPaired,

    #[error("the inference pairing in {} is unusable; remove it, then pair again", path.display())]
    PairingUnusable { path: PathBuf },

    #[error("another setup is already running")]
    Busy,

    #[error("transcription runs on the paired server; run `aria setup --local` first")]
    ServerInUse,

    #[error(transparent)]
    Forget(aria_daemon::RemoteClientError),

    #[error(transparent)]
    Pair(aria_daemon::RemoteClientError),

    #[error("{name} is in use by the {} tables in settings.toml; change them first", tables(.sections))]
    ModelInUse {
        name: &'static str,
        sections: Vec<config::Section>,
    },

    #[error("settings.toml does not load")]
    Config(#[source] aria_settings::config::ConfigError),

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

    #[error("failed to read the settings")]
    Settings(#[source] SettingsError),

    #[error("settings.toml refused the change")]
    SettingsRefused(#[source] SettingsError),

    #[error("settings.toml kept changing; run setup again")]
    SettingsChanging,
}

impl From<SettingsError> for SetupError {
    fn from(error: SettingsError) -> Self {
        Self::Settings(error)
    }
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
        use aria_daemon::config::{DaemonConfig, WakeConfig, WakeWordOverride};
        use aria_voice::config::SpeakerConfig;

        let home = tempfile::tempdir().unwrap();
        let settings_path = aria_settings::default_path(home.path());
        fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        fs::write(&settings_path, "[wake]\nenabled = true\n").unwrap();
        let home_dir = home.path().to_path_buf();
        let settings = aria_settings::load(&home_dir).unwrap();
        let extras: Vec<PathBuf> = EXTRAS
            .iter()
            .flat_map(Asset::files)
            .map(|(relative, _)| aria_settings::config::models_dir(home.path()).join(relative))
            .collect();

        let speaker = SpeakerConfig::load(&settings.speaker, &home_dir);
        assert!(
            extras.contains(&speaker.model),
            "{}",
            speaker.model.display()
        );
        assert!(extras.contains(&speaker.segmentation_model));

        let daemon = DaemonConfig::load(&settings, &home_dir, WakeWordOverride::Inherit).unwrap();
        let WakeConfig::Enabled { classifier, .. } = daemon.wake else {
            panic!("the settings enable the wake word");
        };
        assert_eq!(
            classifier.parent(),
            Some(
                aria_settings::config::models_dir(home.path())
                    .join(WAKEWORD_DIR)
                    .as_path()
            )
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
    pub(super) fn inspection(existing: Existing) -> Inspection {
        let host = bare_host();
        let home = PathBuf::from("/nonexistent");
        Inspection {
            settings_path: PathBuf::from("/nonexistent/settings.toml"),
            macos: true,
            stt: choices::<Stt>(existing.stt.as_deref(), &host),
            tts: choices::<Tts>(existing.tts.as_deref(), &host),
            models: model_states(&home, &aria_settings::testing::file("")).unwrap(),
            wakeword_installed: false,
            wakeword_bundled: true,
            pairing: Pairing::Unpaired,
            transcription_runs_on: RunsOn::Local,
            existing,
            service: Status {
                unit: PathBuf::from("/nonexistent/aria.plist"),
                app_agent: false,
                job: Job::Absent,
                socket: PathBuf::from("/nonexistent/daemon.sock"),
                daemon: supervisor::DaemonState::NotRunning,
            },
            grant: Grant::Waiting,
            key_remapped: false,
            home,
            exe: PathBuf::from("/nonexistent/aria"),
        }
    }

    fn writable_inspection(home: &Path) -> Inspection {
        let mut inspection = inspection(configured());
        inspection.settings_path = aria_settings::default_path(home);
        inspection.home = home.to_path_buf();
        fs::create_dir_all(home.join(".aria")).unwrap();
        inspection
    }

    fn no_downloads() -> ApplyOptions {
        ApplyOptions {
            downloads: Vec::new(),
            transfer_meter: false,
        }
    }

    #[test]
    fn an_engine_is_settable_in_place_only_when_setup_can_fetch_it() {
        let inspection = inspection(configured());
        let settable = |key: &str| {
            inspection
                .stt
                .iter()
                .find(|choice| choice.key == key)
                .unwrap()
                .settable()
        };

        assert!(settable(backend::MOONSHINE));
        assert!(!settable(backend::PARAKEET_MLX), "it fetches its own model");
    }

    #[test]
    fn a_model_takes_its_engines_label_or_else_its_title() {
        let inspection = inspection(configured());
        let label = |name: &str| {
            inspection
                .models
                .iter()
                .find(|model| model.name == name)
                .unwrap()
                .label
        };

        assert_eq!(label(backend::WHISPER), Stt::Whisper.label());
        assert_eq!(label("speaker"), "Speaker verification");
    }

    pub(super) fn configured() -> Existing {
        Existing {
            stt: Some("whisper".to_owned()),
            tts: Some("kokoro".to_owned()),
            wake: Some(false),
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

        assert_eq!(defaults.stt, aria_settings::config::backend::DEFAULT);
        assert_eq!(defaults.tts, aria_settings::config::tts_backend::DEFAULT);
        assert!(!defaults.wake);
    }

    #[test]
    fn the_local_model_is_not_missing_while_the_server_transcribes() {
        let mut inspection = inspection(Existing {
            stt: Some(backend::MOONSHINE.to_owned()),
            ..configured()
        });
        let moonshine = Missing::Model("moonshine");
        assert!(inspection.missing().contains(&moonshine));

        inspection.transcription_runs_on = RunsOn::Server;
        inspection.pairing = paired("https://box.test:9851");

        assert!(!inspection.missing().contains(&moonshine));
    }

    #[test]
    fn the_local_model_is_not_downloaded_while_the_server_transcribes() {
        let mut inspection = inspection(Existing {
            stt: Some(backend::MOONSHINE.to_owned()),
            ..configured()
        });
        inspection.transcription_runs_on = RunsOn::Server;
        inspection.pairing = paired("https://box.test:9851");
        let selections = Selections::defaults(&inspection);

        assert_eq!(selections.stt, backend::MOONSHINE);
        assert!(
            inspection
                .downloads_for(&selections)
                .iter()
                .all(|model| model.name != "moonshine")
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
            [
                aria_settings::config::backend::DEFAULT,
                SPEAKER_ASSET,
                "segmentation"
            ]
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
            [
                Missing::Settings,
                Missing::Wakeword,
                Missing::Model(backend::DEFAULT),
                Missing::Service,
            ]
        );
    }

    /// Only models of engines that run count. Extras are optional, and an
    /// engine that doesn't run needs nothing.
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

    /// A model the settings point elsewhere is the user's: setup neither fetches it
    /// nor calls it missing. Kokoro still needs its default voices, so it
    /// stays wanted.
    #[test]
    fn an_engine_with_its_own_model_path_wants_no_download() {
        let mut inspection = inspection(Existing {
            stt: Some("parakeet".to_owned()),
            ..configured()
        });
        let file = aria_settings::testing::file(
            "[stt]\nbackend = \"parakeet\"\nmodel = \"/elsewhere/tdt\"\n\
             [tts]\nbackend = \"kokoro\"\n[tts.kokoro]\nmodel = \"/elsewhere/kokoro.onnx\"\n",
        );
        inspection.models = model_states(&inspection.home, &file).unwrap();
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
        let file = aria_settings::testing::file(
            "[stt]\nbackend = \"whisper\"\n\
             [listen]\nbackend = \"moonshine\"\nmodel = \"/elsewhere/moonshine\"\n",
        );
        inspection.models = model_states(&inspection.home, &file).unwrap();
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
        let answers = b"\n\ny\ny\n";
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
    fn a_remapped_key_is_missing_until_it_clears() {
        let mut inspection = inspection(configured());
        assert!(!inspection.missing().contains(&Missing::KeyRemapped));

        inspection.key_remapped = true;
        assert!(inspection.missing().contains(&Missing::KeyRemapped));
    }

    #[test]
    fn nothing_is_missing_once_everything_is_in_place() {
        let mut inspection = inspection(configured());
        for model in &mut inspection.models {
            model.files = ModelFiles::Complete;
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
    fn forgetting_waits_for_setup_and_refuses_a_server_in_use() {
        let home = tempfile::tempdir().unwrap();
        let config = aria_settings::default_path(home.path());
        aria_daemon::pair_server_for_tests(home.path(), "https://box.test:9851");

        let running = lock_setup(&config).unwrap();
        assert!(matches!(
            forget_server_at(home.path()),
            Err(SetupError::Busy)
        ));
        drop(running);

        fs::write(&config, "[stt]\nruns_on = \"server\"\n").unwrap();
        assert!(matches!(
            forget_server_at(home.path()),
            Err(SetupError::ServerInUse)
        ));

        fs::write(&config, "[stt]\nbackend = \"whisper\"\n").unwrap();
        assert!(matches!(
            forget_server_at(home.path()),
            Ok(Forgotten::Server { .. })
        ));
    }

    #[test]
    fn pairing_refuses_while_the_server_transcribes() {
        let home = tempfile::tempdir().unwrap();
        let config = aria_settings::default_path(home.path());
        fs::create_dir_all(home.path().join(".aria")).unwrap();

        fs::write(&config, "[stt]\nruns_on = \"server\"\n").unwrap();
        assert!(matches!(
            pair_server_at(home.path(), "{}"),
            Err(SetupError::ServerInUse)
        ));

        fs::write(&config, "").unwrap();
        assert!(
            matches!(pair_server_at(home.path(), "{}"), Err(SetupError::Pair(_))),
            "on this Mac it reaches the invitation"
        );
    }

    #[test]
    fn setup_makes_the_aria_folder_private() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let aria = home.path().join(".aria");
        fs::create_dir(&aria).unwrap();
        fs::set_permissions(&aria, fs::Permissions::from_mode(0o755)).unwrap();

        drop(lock_setup(&aria_settings::default_path(home.path())).unwrap());

        let mode = fs::metadata(&aria).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_second_apply_is_refused_while_one_runs() {
        let home = tempfile::tempdir().unwrap();
        let inspection = writable_inspection(home.path());
        let _running = lock_setup(&inspection.settings_path).unwrap();

        let result = apply(
            &Selections::defaults(&inspection),
            &no_downloads(),
            &inspection,
            &mut |_| Ok(()),
        );
        assert!(matches!(result, Err(SetupError::Busy)));
    }

    #[test]
    fn the_server_without_a_usable_pin_needs_setup() {
        let mut inspection = inspection(configured());
        inspection.transcription_runs_on = RunsOn::Server;

        assert!(inspection.missing().contains(&Missing::Settings));
        inspection.pairing = Pairing::Unusable;
        assert!(inspection.missing().contains(&Missing::Settings));
        inspection.pairing = paired("https://box.test:9851");
        assert!(!inspection.missing().contains(&Missing::Settings));
    }

    fn paired(endpoint: &str) -> Pairing {
        Pairing::Paired(PairedServer {
            endpoint: aria_settings::config::ServerEndpoint::parse(endpoint).unwrap(),
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
        assert_eq!(seen, [SetupEvent::Started(Step::Settings)]);
        assert!(!inspection.settings_path.exists(), "nothing was written");
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
            !inspection.settings_path.exists(),
            "the settings wait for the models"
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
                    fs::write(&inspection.settings_path, "[tts]\nrate = 1.5\n").unwrap();
                    Ok(())
                }
                SetupEvent::Started(Step::Service) => Err(SetupError::Respond(io::Error::from(
                    io::ErrorKind::BrokenPipe,
                ))),
                _ => Ok(()),
            },
        );

        assert!(matches!(result, Err(SetupError::Respond(_))));
        let text = fs::read_to_string(&inspection.settings_path).unwrap();
        assert!(text.contains("rate = 1.5"), "{text}");
    }

    #[test]
    fn sizes_read_like_the_download_page() {
        assert_eq!(size(874_188_075), "875 MB");
        assert_eq!(size(1_500_000_000), "1.5 GB");
        assert_eq!(size(6_958_444), "7 MB");
    }

    #[test]
    fn no_model_means_no_voice_to_record_against() {
        assert!(voice_model_present(&[(SPEAKER_ASSET, Outcome::Downloaded)]));
        assert!(voice_model_present(&[(SPEAKER_ASSET, Outcome::Present)]));

        assert!(!voice_model_present(&[(SPEAKER_ASSET, Outcome::Skipped)]));
        assert!(!voice_model_present(&[]), "nothing wanted, nothing there");
    }
}
