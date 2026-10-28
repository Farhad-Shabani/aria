//! Concrete assembly and execution of the Aria daemon.

use crate::audio::{WakeWordMonitor, remove_all_spools};
use crate::config::ConfigError;
use crate::config::{DaemonConfig, HotkeyConfig, RelayConfig, SpeakerConfig, WakeWordOverride};
use crate::delivery::PlatformTextDelivery;
use crate::enrollment::{Enroller, EnrollmentEventEmitter, Speakers};
use crate::memory::Lexicon;
use crate::microphone::Gate;
use crate::preferences::{PreferenceStore, PreferenceStoreError};
use crate::primitives::error_chain;
use crate::primitives::local_socket::{ExclusiveListener, LocalSocketError};
use crate::runtime::PreparedRuntimePaths;

use std::sync::Arc;

use tokio::sync::mpsc;

use super::chat;
use super::chat::ChatRuntime;
use super::context::Context;
use super::dictation;
use super::dictation::{DictationInitializationError, DictationRuntime, DictationSetup};
use super::host::DaemonLifetime;
use super::hotkey::HotkeyTrigger;
use super::listen::ListenState;
use super::memory::{self, MemoryBacklog};
use super::preferences;
use super::server::{Actors, Relay, Server, ServerError, Services};
use super::service::{
    ChatRpc, DictationRpc, EventsService, HistoryRpc, ListenRpc, PreferencesRpc, RelayRpc,
    SpeakerRpc, SpeechRpc, VocabularyRpc,
};
use super::shutdown::ShutdownError;
use super::speaker;
use super::speaker::SpeakerRuntime;
use super::speech;
use super::speech::{SpeechInitializationError, SpeechRuntime};
use super::subscriptions::Subscriptions;
use super::tasks::RuntimeTasks;
use super::transcripts::TranscriptStore;
use super::wake::WakeTrigger;

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("failed to load shared user preferences")]
    Preferences(#[source] PreferenceStoreError),

    #[error("failed to initialize dictation")]
    Dictation(#[source] DictationInitializationError),

    #[error("failed to initialize speech")]
    Speech(#[source] SpeechInitializationError),

    #[error("failed to detect the daemon run context")]
    Context(#[source] ConfigError),

    #[error("failed to load daemon configuration")]
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
    executor: Box<dyn crate::dictation::DictationExecutor>,
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
    let preference_store = Arc::new(
        PreferenceStore::open(context.preferences_path.clone())
            .map_err(DaemonError::Preferences)?,
    );
    let subscriptions = Arc::new(Subscriptions::new());
    let transcripts = Arc::new(TranscriptStore::open(
        &context.config_environment.home().join(".aria/history"),
    ));
    let microphone = Gate::new();
    let relay_microphone = Arc::clone(&microphone);
    let tasks = RuntimeTasks::new();
    let (vocabulary, mut vocabulary_worker) = super::vocabulary::Runtime::new(
        context.config_environment.home().join(".aria/vocabulary"),
        Lexicon::configured(&context.config_environment),
        matches!(context.lifetime, DaemonLifetime::Persistent),
        crate::vocabulary::discover::LocalCollector::new(
            crate::vocabulary::extract::LocalExtractor,
        ),
    );
    if let Err(error) = vocabulary_worker.initialize_personal() {
        tracing::warn!(%error, "personal vocabulary is unavailable at startup");
    }
    let initial = preference_store
        .snapshot()
        .map_err(DaemonError::Preferences)?;
    if initial.preferences.project_vocabulary {
        vocabulary.prepare(initial.preferences.vocabulary_directory);
    }
    let (memory_wake, memory_woken) = memory::wake_channel();
    let mut workers = build_workers(
        &context,
        &subscriptions,
        &preference_store,
        &transcripts,
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
        hotkey,
        relay_delivery,
    } = workers;
    // A daemon that lives on catches up on what the last one left and keeps
    // up with the owner's asks; one serving a single press has no time for
    // an LLM call.
    let memory_backlog = matches!(context.lifetime, DaemonLifetime::Persistent).then(|| {
        MemoryBacklog::new(
            context.config_environment.clone(),
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
    let dictation_hotkey = preference_store.dictation_hotkey();
    let (preferences, preferences_actor) = preferences::channel(
        Arc::clone(&preference_store),
        context.config_environment.clone(),
    );
    let (speaker, speaker_actor) = speaker::channel(speaker_runtime);
    // The router rebuilds the serving half from this reporter's shared state.
    let (health, _) = tonic_health::server::health_reporter();
    let wake = monitor.map(|monitor| WakeTrigger::new(monitor, dictation.clone(), health.clone()));
    let hotkey = match hotkey {
        HotkeyConfig::Enabled => Some(HotkeyTrigger::new(
            dictation_hotkey,
            dictation.clone(),
            Arc::clone(&dictation_runtime),
            health.clone(),
        )),
        HotkeyConfig::Disabled => None,
    };
    let (history, history_actor) = HistoryRpc::new(transcripts, Arc::clone(&dictation_runtime));
    let preferences_rpc = PreferencesRpc::new(preferences, Arc::clone(&vocabulary));
    // Without usable pairing the relay stays off, but the Mac still dictates.
    let relay = relay_delivery.and_then(|(delivery, port)| {
        let path = context
            .config_environment
            .home()
            .join(super::relay::PAIRING_RELATIVE_PATH);
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
                delivery,
                context.paths.spool_dir(),
                tasks.cancellation(),
            ),
            port,
            preferences: preferences_rpc.clone(),
            pairing,
            codes: Arc::new(super::relay::PairingCodes::default()),
            instance,
        })
    });
    let services = Services {
        history,
        dictation: DictationRpc::new(dictation, dictation_runtime),
        speech: SpeechRpc::new(speech),
        chat: ChatRpc::new(chat, chat_runtime, context.lifetime, tasks.cancellation()),
        listen: ListenRpc::new(Arc::new(ListenState::new(
            context.lifetime,
            context.config_environment.clone(),
            listen_event_emitter(Arc::clone(&subscriptions)),
            Arc::clone(&vocabulary),
        ))),
        preferences: preferences_rpc,
        vocabulary: VocabularyRpc::new(Arc::clone(&vocabulary), preference_store),
        speaker: SpeakerRpc::new(speaker),
        events: EventsService::new(subscriptions),
        relay,
        health,
    };
    let actors = Actors {
        history: history_actor,
        dictation: dictation_actor,
        speech: speech_actor,
        chat: chat_actor,
        preferences: preferences_actor,
        speaker: speaker_actor,
        memory: memory_backlog,
        vocabulary: vocabulary_worker,
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

/// Everything the daemon runs for as long as it lives: a runtime behind each
/// request-serving service, plus the wake-word monitor and hotkey when armed.
struct Workers {
    dictation: Arc<DictationRuntime>,
    speech: Arc<SpeechRuntime>,
    chat: Arc<ChatRuntime>,
    speaker: Arc<SpeakerRuntime>,
    monitor: Option<WakeWordMonitor>,
    hotkey: HotkeyConfig,
    /// The relay's delivery adapter and its port, read from the
    /// configuration a persistent daemon loads. An on-demand one — which
    /// never advertises and exits after one press — carries none and serves
    /// no relay.
    relay_delivery: Option<(PlatformTextDelivery, u16)>,
}

fn enrollment_event_emitter(subscriptions: Arc<Subscriptions>) -> EnrollmentEventEmitter {
    EnrollmentEventEmitter::subscribed_by(crate::enrollment::EVENT_THREAD, move |stage| {
        subscriptions.broadcast(stage);
    })
}

fn listen_event_emitter(subscriptions: Arc<Subscriptions>) -> crate::listen::ListenEventEmitter {
    crate::listen::ListenEventEmitter::subscribed_by(crate::listen::EVENT_THREAD, move |stage| {
        subscriptions.broadcast(stage);
    })
}

#[allow(clippy::too_many_arguments)]
fn build_workers(
    context: &Context<'_>,
    subscriptions: &Arc<Subscriptions>,
    preferences: &Arc<PreferenceStore>,
    transcripts: &Arc<TranscriptStore>,
    wake_word: WakeWordOverride,
    microphone: Arc<Gate>,
    memory_wake: mpsc::Sender<()>,
    vocabulary: Arc<super::vocabulary::Runtime>,
) -> Result<Workers, DaemonError> {
    match context.lifetime {
        DaemonLifetime::Persistent => eager_workers(
            context,
            subscriptions,
            preferences,
            transcripts,
            wake_word,
            microphone,
            memory_wake,
            vocabulary,
        ),
        DaemonLifetime::OnDemand => Ok(deferred_workers(
            context,
            subscriptions,
            preferences,
            transcripts,
            microphone,
            vocabulary,
        )),
    }
}

/// Configuration, the model, and wake-word monitoring are all read here, so a
/// daemon that cannot serve a press says so before it accepts one.
#[allow(clippy::too_many_arguments)]
fn eager_workers(
    context: &Context<'_>,
    subscriptions: &Arc<Subscriptions>,
    preferences: &Arc<PreferenceStore>,
    transcripts: &Arc<TranscriptStore>,
    wake_word: WakeWordOverride,
    microphone: Arc<Gate>,
    memory_wake: mpsc::Sender<()>,
    vocabulary: Arc<super::vocabulary::Runtime>,
) -> Result<Workers, DaemonError> {
    let DaemonConfig {
        dictation: dictation_config,
        wake,
        hotkey,
        relay,
        tts,
    } = DaemonConfig::load(&context.config_environment, wake_word)
        .map_err(DaemonError::Configuration)?;
    // Its own adapter, not the dictation pipeline's: that one is wrapped in a
    // press's lifecycle, and a relayed recording has no press around it. The
    // adapter is stateless, so a second costs a timeout and nothing else.
    let relay_delivery = match relay {
        RelayConfig::Disabled => None,
        RelayConfig::Enabled { port } => Some((
            PlatformTextDelivery::new(dictation_config.dictation_timeouts.delivery)
                .map_err(DictationInitializationError::Delivery)
                .map_err(DaemonError::Dictation)?,
            port,
        )),
    };
    let speech =
        SpeechRuntime::eager(Arc::clone(subscriptions), tts).map_err(DaemonError::Speech)?;
    let speaker_config =
        SpeakerConfig::load(&context.config_environment).map_err(DaemonError::Configuration)?;
    let (dictation, monitor, recorder) = DictationRuntime::eager(
        context,
        Arc::clone(subscriptions),
        Arc::clone(preferences),
        Arc::clone(transcripts),
        DictationSetup {
            dictation: dictation_config,
            wake,
            speaker: speaker_config.clone(),
            vocabulary,
        },
        Arc::clone(&microphone),
    )
    .map_err(DaemonError::Dictation)?;
    let enroller = Enroller::new(recorder.capture, microphone, recorder.wake, &speaker_config);
    let speakers = Speakers::open(&speaker_config);
    Ok(Workers {
        dictation: Arc::new(dictation),
        speech: Arc::new(speech),
        chat: Arc::new(ChatRuntime::new(
            context,
            Arc::clone(subscriptions),
            Arc::clone(preferences),
            Some(memory_wake),
        )),
        speaker: Arc::new(SpeakerRuntime::new(
            Some(speakers),
            Some(enroller),
            enrollment_event_emitter(Arc::clone(subscriptions)),
        )),
        monitor,
        hotkey,
        relay_delivery,
    })
}

/// Nothing is read until the first request, so construction cannot fail.
fn deferred_workers(
    context: &Context<'_>,
    subscriptions: &Arc<Subscriptions>,
    preferences: &Arc<PreferenceStore>,
    transcripts: &Arc<TranscriptStore>,
    microphone: Arc<Gate>,
    vocabulary: Arc<super::vocabulary::Runtime>,
) -> Workers {
    // Reading and forgetting profiles is filesystem work, so a daemon with no
    // microphone of its own still answers those.
    let speakers = match SpeakerConfig::load(&context.config_environment) {
        Ok(config) => Some(Speakers::open(&config)),
        Err(error) => {
            tracing::warn!(%error, "no speaker configuration; enrolled voices cannot be listed");
            None
        }
    };
    Workers {
        dictation: Arc::new(DictationRuntime::deferred(
            context,
            Arc::clone(subscriptions),
            Arc::clone(preferences),
            Arc::clone(transcripts),
            microphone,
            vocabulary,
        )),
        speech: Arc::new(SpeechRuntime::deferred(context, Arc::clone(subscriptions))),
        // An on-demand daemon serves one press; its turns wait for a
        // persistent one to extract them.
        chat: Arc::new(ChatRuntime::new(
            context,
            Arc::clone(subscriptions),
            Arc::clone(preferences),
            None,
        )),
        // An on-demand daemon builds its microphone for the one press it
        // serves, so there is none to enroll through.
        speaker: Arc::new(SpeakerRuntime::new(
            speakers,
            None,
            enrollment_event_emitter(Arc::clone(subscriptions)),
        )),
        monitor: None,
        // An on-demand daemon serves one request and exits. A hotkey, like
        // the wake word and the relay, needs a persistent daemon.
        hotkey: HotkeyConfig::Disabled,
        relay_delivery: None,
    }
}
