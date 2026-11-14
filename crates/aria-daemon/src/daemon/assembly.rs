//! Concrete assembly and execution of the Aria daemon.

use crate::daemon::config::{DaemonConfig, HotkeyConfig, RelayConfig, WakeWordOverride};
use aria_assistant::memory::Lexicon;
use aria_kit::audio::{WakeWordMonitor, remove_all_spools};
use aria_platform::runtime::PreparedRuntimePaths;
use aria_primitives::error_chain;
use aria_primitives::local_socket::{ExclusiveListener, LocalSocketError};
use aria_settings::config::{ConfigError, memory_directory};
use aria_settings::{CheckedSettings, SettingsCache, SettingsError};
use aria_settings::{SettingsStore, StoreError};
use aria_voice::config::SpeakerConfig;
use aria_voice::enrollment::{Enroller, EnrollmentEventEmitter, Speakers};
use aria_voice::microphone::Gate;

use std::sync::Arc;

use tokio::sync::mpsc;

use super::capture::MicrophoneFollower;
use super::chat;
use super::chat::ChatRuntime;
use super::context::Context;
use super::dictation;
use super::dictation::{DictationInitializationError, DictationRuntime, DictationSetup};
use super::history::HistoryStore;
use super::host::DaemonLifetime;
use super::hotkey::HotkeyTrigger;
use super::memory::{self, MemoryBacklog};
use super::server::{Actors, Relay, Server, ServerError, Services};
use super::service::{
    ChatRpc, DictationRpc, EventsService, HistoryRpc, MicrophoneRpc, RelayRpc, SettingsRpc,
    SpeakerRpc, SpeechRpc, SystemTranscriptionRpc, VocabularyRpc,
};
use super::settings;
use super::shutdown::ShutdownError;
use super::speaker;
use super::speaker::SpeakerRuntime;
use super::speech;
use super::speech::SpeechRuntime;
use super::subscriptions::Subscriptions;
use super::system_transcription::SystemTranscriptionState;
use super::tasks::RuntimeTasks;
use super::wake::WakeTrigger;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Fatal at startup, with the line and column.
    #[error("failed to load the settings")]
    Settings(#[source] SettingsError),

    #[error("failed to load the user cache")]
    Store(#[source] StoreError),

    #[error("failed to initialize dictation")]
    Dictation(#[source] DictationInitializationError),

    #[error("failed to detect the daemon run context")]
    Context(#[source] ConfigError),

    #[error("failed to resolve the daemon settings")]
    Configuration(#[source] ConfigError),

    #[error("failed to acquire daemon socket ownership")]
    Bind(#[source] LocalSocketError),

    #[error("failed to prepare daemon shutdown handling")]
    Shutdown(#[source] ShutdownError),

    #[error("failed to start the daemon thread")]
    SpawnThread(#[source] std::io::Error),

    #[error("daemon server terminated with an error")]
    Run(#[source] ServerError),
}

impl DaemonError {
    /// The settings fault that stopped startup, and where it is. Its message
    /// can quote the file, so a log records only the place.
    pub fn settings_position(&self) -> Option<Option<aria_settings::Position>> {
        match self {
            Self::Settings(error) => Some(error.position()),
            Self::Store(_)
            | Self::Dictation(_)
            | Self::Context(_)
            | Self::Configuration(_)
            | Self::Bind(_)
            | Self::Shutdown(_)
            | Self::SpawnThread(_)
            | Self::Run(_) => None,
        }
    }

    /// Whether the bind failed because another daemon owns the socket —
    /// the caller should connect to that daemon instead.
    pub fn is_socket_owned_elsewhere(&self) -> bool {
        matches!(self, Self::Bind(LocalSocketError::AlreadyOwned))
    }
}

pub(super) fn build(
    paths: &PreparedRuntimePaths,
    lifetime: DaemonLifetime,
    wake_word: WakeWordOverride,
) -> Result<Server, DaemonError> {
    compose(paths, lifetime, wake_word, |_| {})
}

#[cfg(test)]
pub(super) fn build_scripted(
    paths: &PreparedRuntimePaths,
    executor: Box<dyn aria_voice::dictation::DictationExecutor>,
) -> Result<Server, DaemonError> {
    compose(
        paths,
        DaemonLifetime::OnDemand,
        WakeWordOverride::Inherit,
        |workers| {
            Arc::get_mut(&mut workers.dictation)
                .expect("nothing shares the dictation runtime before the server")
                .execute_with(executor);
        },
    )
}

/// `adjust` sees the workers before anything shares them.
fn compose(
    paths: &PreparedRuntimePaths,
    lifetime: DaemonLifetime,
    wake_word: WakeWordOverride,
    adjust: impl FnOnce(&mut Workers),
) -> Result<Server, DaemonError> {
    let socket = ExclusiveListener::bind(paths.socket_path(), paths.lock_path())
        .map_err(DaemonError::Bind)?;
    let context = Context::detect(paths, lifetime).map_err(DaemonError::Context)?;
    // A daemon starts owning no audio: whatever a run that already ended left
    // behind goes before this one can record over it.
    remove_all_spools(context.paths.spool_dir());
    let cache = Arc::new(open_settings(&context)?);
    // The startup values: restart keys keep these for the daemon's life.
    let startup = cache.effective();
    let settings = Arc::new(SettingsStore::new(Arc::clone(&cache)));
    let subscriptions = Arc::new(Subscriptions::new());
    let history_store = Arc::new(HistoryStore::open(&context.home.join(".aria/history")));
    let microphone = Gate::new();
    let relay_microphone = Arc::clone(&microphone);
    let tasks = RuntimeTasks::new();
    let lexicon = Some(Lexicon::new(&memory_directory(
        &startup.memory,
        &context.home,
    )));
    let (vocabulary, mut vocabulary_worker) = super::vocabulary::Runtime::new(
        context.home.join(".aria/vocabulary"),
        lexicon,
        matches!(context.lifetime, DaemonLifetime::Persistent),
        Arc::clone(&settings),
        aria_voice::vocabulary::discover::LocalCollector::new(
            aria_voice::vocabulary::extract::LocalExtractor,
        ),
    );
    if let Err(error) = vocabulary_worker.initialize_personal() {
        tracing::warn!(%error, "personal vocabulary is unavailable at startup");
    }
    vocabulary_worker.follow(cache.project_vocabulary());
    let initial = settings.snapshot().map_err(DaemonError::Store)?;
    if initial.settings.vocabulary_enabled {
        vocabulary.prepare(initial.settings.vocabulary_dir);
    }
    let (memory_wake, memory_woken) = memory::wake_channel();
    let mut workers = build_workers(
        &context,
        &Dependencies {
            subscriptions: &subscriptions,
            settings: &settings,
            cache: &cache,
            startup: &startup,
            history_store: &history_store,
        },
        wake_word,
        microphone,
        memory_wake,
        Arc::clone(&vocabulary),
    )?;
    adjust(&mut workers);
    let Workers {
        dictation,
        speech,
        chat,
        speaker: speaker_runtime,
        monitor,
        engine,
        microphone: microphone_follower,
        hotkey,
        relay_port,
    } = workers;
    // A daemon that lives on catches up on what the last one left and keeps
    // up with the owner's chats; one serving a single press has no time for
    // an LLM call.
    let memory_backlog = matches!(context.lifetime, DaemonLifetime::Persistent).then(|| {
        MemoryBacklog::new(
            context.home.clone(),
            Arc::clone(&cache),
            memory_woken,
            Arc::clone(&vocabulary),
        )
    });
    let dictation_runtime = Arc::clone(&dictation);
    let relay_runtime = Arc::clone(&dictation);
    let chat_runtime = Arc::clone(&chat);
    let (dictation, dictation_actor) = dictation::channel(dictation);
    let (speech, speech_actor) = speech::channel(speech);
    let (chat, chat_actor) = chat::channel(chat);
    let aria_key = settings.aria_key();
    let (settings_handle, settings_actor) = settings::channel(Arc::clone(&settings));
    let (speaker, speaker_actor) = speaker::channel(speaker_runtime);
    // The router rebuilds the serving half from this reporter's shared state.
    let (health, _) = tonic_health::server::health_reporter();
    let wake = monitor.map(|monitor| WakeTrigger::new(monitor, dictation.clone(), health.clone()));
    let hotkey = match hotkey {
        HotkeyConfig::Enabled => Some(HotkeyTrigger::new(
            aria_key,
            dictation.clone(),
            chat.clone(),
            Arc::clone(&dictation_runtime),
            Arc::clone(&subscriptions),
            health.clone(),
        )),
        HotkeyConfig::Disabled => None,
    };
    let (history, history_actor) =
        HistoryRpc::new(Arc::clone(&history_store), Arc::clone(&dictation_runtime));
    let settings_rpc = SettingsRpc::new(settings_handle, cache.status());
    // Without usable pairing the relay stays off, but the Mac still dictates.
    let relay = relay_port.and_then(|port| {
        let path = context.home.join(super::relay::PAIRING_RELATIVE_PATH);
        let instance = super::relay::instance_name();
        let pairing = super::relay::load_pairing(&path, &instance)
            .inspect_err(|error| {
                tracing::error!(
                    error = %error_chain(error),
                    path = %path.display(),
                    "relay is off: pairing material is unusable"
                );
            })
            .ok()?;
        Some(Relay {
            rpc: RelayRpc::new(
                relay_runtime,
                relay_microphone,
                context.paths.spool_dir(),
                tasks.cancellation(),
            ),
            port,
            settings: settings_rpc.clone(),
            pairing,
            instance,
        })
    });
    let services = Services {
        history,
        dictation: DictationRpc::new(dictation, dictation_runtime),
        speech: SpeechRpc::new(speech),
        chat: ChatRpc::new(chat, chat_runtime, context.lifetime, tasks.cancellation()),
        system_transcription: SystemTranscriptionRpc::new(Arc::new(SystemTranscriptionState::new(
            context.lifetime,
            context.home.clone(),
            Arc::clone(&cache),
            Arc::clone(&settings),
            system_transcription_event_emitter(Arc::clone(&subscriptions)),
            Arc::clone(&vocabulary),
            Arc::clone(&history_store),
        ))),
        settings: settings_rpc,
        vocabulary: VocabularyRpc::new(Arc::clone(&vocabulary)),
        speaker: SpeakerRpc::new(speaker),
        microphone: MicrophoneRpc::new(Arc::clone(&cache)),
        events: EventsService::new(Arc::clone(&subscriptions)),
        relay,
        health,
    };
    let settings_poll =
        aria_settings::SettingsPoll::spawn(Arc::clone(&cache)).map_err(DaemonError::Settings)?;
    let actors = Actors {
        history: history_actor,
        dictation: dictation_actor,
        speech: speech_actor,
        chat: chat_actor,
        settings: settings_actor,
        settings_status: settings::StatusBroadcast::new(cache.status(), Arc::clone(&subscriptions)),
        speaker: speaker_actor,
        memory: memory_backlog,
        vocabulary: vocabulary_worker,
        settings_poll,
        engine,
        microphone: microphone_follower,
    };
    Ok(Server::new(
        socket,
        context.lifetime,
        services,
        tasks,
        actors,
        wake,
        hotkey,
    ))
}

/// An invalid file stops startup here, with its position.
fn open_settings(context: &Context<'_>) -> Result<SettingsCache, DaemonError> {
    let home = &context.home;
    aria_settings::write_schema_beside(&context.settings_path);
    SettingsCache::open(context.settings_path.clone(), home.to_path_buf())
        .map_err(DaemonError::Settings)
}

/// What every worker shares, borrowed for its construction.
struct Dependencies<'a> {
    subscriptions: &'a Arc<Subscriptions>,
    settings: &'a Arc<SettingsStore>,
    cache: &'a Arc<SettingsCache>,
    /// The settings as the daemon started with them.
    startup: &'a CheckedSettings,
    history_store: &'a Arc<HistoryStore>,
}

/// Everything the daemon runs for as long as it lives: a runtime behind each
/// request-serving service, plus the wake-word monitor and hotkey when armed.
struct Workers {
    dictation: Arc<DictationRuntime>,
    speech: Arc<SpeechRuntime>,
    chat: Arc<ChatRuntime>,
    speaker: Arc<SpeakerRuntime>,
    monitor: Option<WakeWordMonitor>,
    /// `None` for an on-demand daemon, which serves one press and follows
    /// nothing.
    engine: Option<super::dictation::EngineFollower>,
    /// `None` for an on-demand daemon, as for `engine`.
    microphone: Option<MicrophoneFollower>,
    hotkey: HotkeyConfig,
    /// The relay's delivery adapter and its port, read from the
    /// settings a persistent daemon loads. An on-demand one — which
    /// never advertises and exits after one press — carries none and serves
    /// no relay.
    relay_port: Option<u16>,
}

fn enrollment_event_emitter(subscriptions: Arc<Subscriptions>) -> EnrollmentEventEmitter {
    EnrollmentEventEmitter::subscribed_by(aria_voice::enrollment::EVENT_THREAD, move |stage| {
        subscriptions.broadcast(stage);
    })
}

fn speech_event_emitter(
    subscriptions: Arc<Subscriptions>,
) -> aria_voice::speech::SpeechEventEmitter {
    aria_voice::speech::SpeechEventEmitter::subscribed_by(
        aria_voice::speech::EVENT_THREAD,
        move |stage| {
            subscriptions.broadcast(stage);
        },
    )
}

fn system_transcription_event_emitter(
    subscriptions: Arc<Subscriptions>,
) -> aria_voice::system_transcription::SystemTranscriptionEventEmitter {
    aria_voice::system_transcription::SystemTranscriptionEventEmitter::subscribed_by(
        aria_voice::system_transcription::EVENT_THREAD,
        move |stage| {
            subscriptions.broadcast(stage);
        },
    )
}

fn build_workers(
    context: &Context<'_>,
    dependencies: &Dependencies<'_>,
    wake_word: WakeWordOverride,
    microphone: Arc<Gate>,
    memory_wake: mpsc::Sender<()>,
    vocabulary: Arc<super::vocabulary::Runtime>,
) -> Result<Workers, DaemonError> {
    match context.lifetime {
        DaemonLifetime::Persistent => eager_workers(
            context,
            dependencies,
            wake_word,
            microphone,
            memory_wake,
            vocabulary,
        ),
        DaemonLifetime::OnDemand => Ok(deferred_workers(
            context,
            dependencies,
            microphone,
            vocabulary,
        )),
    }
}

/// Settings, the speech model, and the wake-word model load here, so a
/// daemon missing one says so before it accepts a press.
fn eager_workers(
    context: &Context<'_>,
    dependencies: &Dependencies<'_>,
    wake_word: WakeWordOverride,
    microphone: Arc<Gate>,
    memory_wake: mpsc::Sender<()>,
    vocabulary: Arc<super::vocabulary::Runtime>,
) -> Result<Workers, DaemonError> {
    let Dependencies {
        subscriptions,
        settings,
        cache,
        startup,
        history_store,
    } = *dependencies;
    let DaemonConfig {
        dictation: dictation_config,
        wake,
        relay,
    } = DaemonConfig::load(startup, &context.home, wake_word)
        .map_err(DaemonError::Configuration)?;
    let relay_port = match relay {
        RelayConfig::Disabled => None,
        RelayConfig::Enabled { port } => Some(port),
    };
    let speech = SpeechRuntime::new(
        context,
        speech_event_emitter(Arc::clone(subscriptions)),
        Arc::clone(cache),
    );
    // Dictation doesn't need synthesis, so a failed load retries at the next request to speak.
    if let Err(error) = speech.load() {
        tracing::error!(
            error = %error_chain(&error),
            "speech is unavailable until an utterance can load it"
        );
    }
    let speaker_config = SpeakerConfig::load(&startup.speaker, &context.home);
    // Subscribed before the capture reads its first device, so no change
    // falls between the two.
    let mut chosen_microphone = cache.microphone();
    let input = super::capture::input(&chosen_microphone.borrow_and_update());
    let (dictation, monitor, recorder, engine) = DictationRuntime::eager(
        context,
        Arc::clone(subscriptions),
        Arc::clone(settings),
        Arc::clone(cache),
        Arc::clone(history_store),
        DictationSetup {
            dictation: dictation_config,
            wake,
            speaker: speaker_config.clone(),
            vocabulary,
            delivery: context.delivery.clone(),
            input,
        },
        Arc::clone(&microphone),
    )
    .map_err(DaemonError::Dictation)?;
    let microphone_follower =
        MicrophoneFollower::new(Arc::clone(&recorder.capture), chosen_microphone);
    let speakers = Arc::new(Speakers::open(&speaker_config));
    let enroller = Enroller::new(
        recorder.capture,
        microphone,
        recorder.wake,
        Arc::clone(&speakers),
        &speaker_config,
    );
    Ok(Workers {
        dictation: Arc::new(dictation),
        speech: Arc::new(speech),
        chat: Arc::new(ChatRuntime::new(
            context,
            Arc::clone(subscriptions),
            Arc::clone(settings),
            Arc::clone(cache),
            Some(memory_wake),
            Arc::clone(history_store),
        )),
        speaker: Arc::new(SpeakerRuntime::new(
            Some(speakers),
            Some(enroller),
            enrollment_event_emitter(Arc::clone(subscriptions)),
            Arc::clone(settings),
        )),
        monitor,
        engine: Some(engine),
        microphone: Some(microphone_follower),
        hotkey: context.hotkey(),
        relay_port,
    })
}

/// Nothing is read until the first request, so construction cannot fail.
fn deferred_workers(
    context: &Context<'_>,
    dependencies: &Dependencies<'_>,
    microphone: Arc<Gate>,
    vocabulary: Arc<super::vocabulary::Runtime>,
) -> Workers {
    let Dependencies {
        subscriptions,
        settings,
        cache,
        startup,
        history_store,
    } = *dependencies;
    // Reading and forgetting profiles is filesystem work, so a daemon with no
    // microphone of its own still answers those.
    let speakers = Some(Arc::new(Speakers::open(&SpeakerConfig::load(
        &startup.speaker,
        &context.home,
    ))));
    Workers {
        dictation: Arc::new(DictationRuntime::deferred(
            context,
            Arc::clone(subscriptions),
            Arc::clone(settings),
            Arc::clone(cache),
            Arc::clone(history_store),
            microphone,
            vocabulary,
        )),
        speech: Arc::new(SpeechRuntime::new(
            context,
            speech_event_emitter(Arc::clone(subscriptions)),
            Arc::clone(cache),
        )),
        // An on-demand daemon serves one press; its turns wait for a
        // persistent one to extract them.
        chat: Arc::new(ChatRuntime::new(
            context,
            Arc::clone(subscriptions),
            Arc::clone(settings),
            Arc::clone(cache),
            None,
            Arc::clone(history_store),
        )),
        // An on-demand daemon builds its microphone for the one press it
        // serves, so there is none to enroll through.
        speaker: Arc::new(SpeakerRuntime::new(
            speakers,
            None,
            enrollment_event_emitter(Arc::clone(subscriptions)),
            Arc::clone(settings),
        )),
        monitor: None,
        // An on-demand daemon serves one request and exits. A hotkey, like
        // the wake word and the relay, needs a persistent daemon.
        hotkey: HotkeyConfig::Disabled,
        engine: None,
        microphone: None,
        relay_port: None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use aria_platform::runtime::RuntimePaths;

    /// Runtime paths whose spool directory is the test daemon's home.
    fn prepared() -> (tempfile::TempDir, PreparedRuntimePaths) {
        let directory = tempfile::tempdir().expect("a temp dir");
        let paths = RuntimePaths::in_directory(directory.path().to_path_buf())
            .prepare()
            .expect("the runtime directory is usable");
        fs::create_dir_all(paths.spool_dir().join(".aria")).expect("the cache directory");
        (directory, paths)
    }

    #[test]
    fn an_invalid_settings_file_stops_startup_with_its_position() {
        let (_directory, paths) = prepared();
        let cache = aria_settings::default_path(paths.spool_dir());
        fs::write(&cache, "[tts]\nrate = 9.0\n").expect("written");

        let error = build(&paths, DaemonLifetime::OnDemand, WakeWordOverride::Inherit)
            .err()
            .expect("an invalid file is fatal at startup");

        assert!(matches!(error, DaemonError::Settings(_)), "{error}");
        let message = error_chain(&error);
        assert!(message.contains("line 2, column 1: tts.rate"), "{message}");
    }
}
