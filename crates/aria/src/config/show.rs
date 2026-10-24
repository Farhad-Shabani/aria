//! Resolved settings and their sources: environment, file, or default.
//! Section failures appear as rows without failing the command; dependent
//! sections are omitted to avoid repeating the same error.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::audio::ReplayPace;

use super::file::{self, FileConfig};
use super::load::{
    self, BACKEND_ENV, CAPTURE_BACKEND_ENV, CAPTURE_FILE_ENV, CAPTURE_PACE_ENV, CHAT_AGENT_ENV,
    CHAT_TIMEOUT_ENV, DEFAULT_CAPTURE_BACKEND, DEFAULT_CONFIG_RELATIVE_PATH, DELIVERY_TIMEOUT_ENV,
    DEVICE_ENV, DICTATION_BACKTRACK_ENV, DICTATION_CLEANUP_ENV, Environment, HOTKEY_ENABLED_ENV,
    INTERPRETER_ENV, LISTEN_BACKEND_ENV, LISTEN_DEVICE_ENV, LISTEN_MODEL_ENV,
    LISTEN_TRANSCRIPTS_DIR_ENV, MEMORY_DIR_ENV, MEMORY_ENABLED_ENV, MEMORY_EXTRACT_MODEL_ENV,
    MEMORY_MAX_FACTS_ENV, MODEL_ENV, OVERALL_TIMEOUT_ENV, RECORDING_TIMEOUT_ENV,
    SEGMENTATION_MODEL_ENV, SPEAKER_MODEL_ENV, SPEAKERS_DIR_ENV, TTS_BACKEND_ENV, TTS_FALLBACK_ENV,
    TTS_MODEL_ENV, TTS_RATE_ENV, TTS_TOKENIZER_ENV, TTS_VOICE_ENV, TTS_VOICES_ENV,
    WAKE_BACKEND_ENV, WAKE_ENABLED_ENV, WAKE_MODEL_ENV, WAKE_THRESHOLD_ENV,
    WAV_FILE_CAPTURE_BACKEND,
};
use super::{
    CaptureConfig, ConfigError, DictationTimeouts, HotkeyConfig, ListenTranscription, RelayConfig,
    TranscriptionConfig, TranscriptionDevice, TtsBackend, TtsFallback, TtsModel, WakeConfig,
    WakeWordOverride,
};

pub struct Report {
    pub path: PathBuf,
    pub rows: Vec<Result<Row, Failed>>,
}

pub struct Row {
    pub key: &'static str,
    pub value: String,
    pub source: Source,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    Environment(&'static str),
    /// Listen names no engine of its own, so it takes transcription's.
    Inherited,
}

impl Source {
    fn of(environment_set: bool, file_set: bool, variable: &'static str) -> Self {
        if environment_set {
            Self::Environment(variable)
        } else if file_set {
            Self::File
        } else {
            Self::Default
        }
    }

    fn file_or_default(file_set: bool) -> Self {
        if file_set { Self::File } else { Self::Default }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => formatter.write_str("default"),
            Self::File => formatter.write_str("file"),
            Self::Environment(variable) => write!(formatter, "env {variable}"),
            Self::Inherited => formatter.write_str("from transcription"),
        }
    }
}

pub struct Failed {
    pub section: &'static str,
    pub error: ConfigError,
}

/// What the daemon would load right now, from this process's environment.
pub fn show() -> Result<Report, ConfigError> {
    report(&Environment::capture()?)
}

fn report(environment: &Environment) -> Result<Report, ConfigError> {
    let path = environment.home().join(DEFAULT_CONFIG_RELATIVE_PATH);
    let file = file::read(&path)?;
    let (capture_rows, capture) = resolved(load::capture(&file, environment), |config| {
        capture(&file, environment, config)
    });
    let (transcription_rows, transcription) =
        resolved(load::transcription(&file, environment), |config| {
            transcription(&file, environment, config)
        });
    let (timeout_rows, timeouts) = resolved(load::timeouts(&file, environment), |config| {
        self::timeouts(&file, environment, config)
    });
    let sections = [
        ("capture", capture_rows),
        ("transcription", transcription_rows),
        ("dictation_timeouts", timeout_rows),
        ("dictation_cleanup", cleanup(&file, environment)),
        ("dictation_backtrack", backtrack(&file, environment)),
        ("dictation_cleanup_llm", cleanup_llm(&file, environment)),
        ("wake", wake(&file, environment, capture.as_ref())),
        ("hotkey", hotkey(&file, environment)),
        ("phone", Ok(relay(&file))),
        ("tts", tts(&file, environment)),
        ("chat", chat(&file, environment, timeouts.as_ref())),
        ("listen", listen(&file, environment, transcription.as_ref())),
        ("speaker", speaker(&file, environment)),
        ("memory", memory(&file, environment)),
    ];
    let rows = sections
        .into_iter()
        .flat_map(|(section, rows)| match rows {
            Ok(rows) => rows.into_iter().map(Ok).collect(),
            Err(error) => vec![Err(Failed { section, error })],
        })
        .collect();
    Ok(Report { path, rows })
}

/// Rows for a section other sections build on, and the value they build on.
fn resolved<T>(
    loaded: Result<T, ConfigError>,
    rows: impl FnOnce(&T) -> Vec<Row>,
) -> (Result<Vec<Row>, ConfigError>, Option<T>) {
    match loaded {
        Ok(config) => (Ok(rows(&config)), Some(config)),
        Err(error) => (Err(error), None),
    }
}

fn row(key: &'static str, value: impl ToString, source: Source) -> Row {
    Row {
        key,
        value: value.to_string(),
        source,
    }
}

fn seconds(duration: Duration) -> String {
    format!("{}s", duration.as_secs_f64())
}

fn capture(file: &FileConfig, environment: &Environment, config: &CaptureConfig) -> Vec<Row> {
    let backend = match config {
        CaptureConfig::Microphone => DEFAULT_CAPTURE_BACKEND,
        CaptureConfig::WavFile { .. } => WAV_FILE_CAPTURE_BACKEND,
    };
    let mut rows = vec![row(
        "capture.backend",
        backend,
        Source::of(
            environment.capture_backend.is_some(),
            file.capture.backend.is_some(),
            CAPTURE_BACKEND_ENV,
        ),
    )];
    if let CaptureConfig::WavFile { path, pace } = config {
        let pace = match pace {
            ReplayPace::Immediate => "immediate",
            ReplayPace::Realtime => "realtime",
        };
        rows.push(row(
            "capture.path",
            path.display(),
            Source::of(
                environment.capture_file.is_some(),
                file.capture.path.is_some(),
                CAPTURE_FILE_ENV,
            ),
        ));
        rows.push(row(
            "capture.pace",
            pace,
            Source::of(
                environment.capture_pace.is_some(),
                file.capture.pace.is_some(),
                CAPTURE_PACE_ENV,
            ),
        ));
    }
    rows
}

fn device_name(device: TranscriptionDevice) -> &'static str {
    match device {
        TranscriptionDevice::Cpu => "cpu",
        TranscriptionDevice::Gpu => "gpu",
    }
}

fn transcription(
    file: &FileConfig,
    environment: &Environment,
    config: &TranscriptionConfig,
) -> Vec<Row> {
    let model = Source::of(
        environment.transcription_model.is_some(),
        file.transcription.model.is_some(),
        MODEL_ENV,
    );
    let mut rows = vec![row(
        "transcription.backend",
        config.backend_name(),
        Source::of(
            environment.transcription_backend.is_some(),
            file.transcription.backend.is_some(),
            BACKEND_ENV,
        ),
    )];
    match config {
        TranscriptionConfig::Whisper { model: path }
        | TranscriptionConfig::WhisperCandle {
            model_directory: path,
        }
        | TranscriptionConfig::Moonshine {
            model_directory: path,
        }
        | TranscriptionConfig::Nemotron {
            model_directory: path,
        } => rows.push(row("transcription.model", path.display(), model)),
        TranscriptionConfig::Parakeet {
            model_directory,
            device,
        } => {
            rows.push(row("transcription.model", model_directory.display(), model));
            rows.push(row(
                "transcription.device",
                device_name(*device),
                Source::of(
                    environment.transcription_device.is_some(),
                    file.transcription.device.is_some(),
                    DEVICE_ENV,
                ),
            ));
        }
        TranscriptionConfig::ParakeetMlx {
            model: name,
            interpreter,
        } => {
            rows.push(row("transcription.model", name, model));
            rows.push(row(
                "transcription.interpreter",
                interpreter.display(),
                Source::of(
                    environment.transcription_interpreter.is_some(),
                    file.transcription.interpreter.is_some(),
                    INTERPRETER_ENV,
                ),
            ));
        }
    }
    rows
}

fn timeouts(file: &FileConfig, environment: &Environment, config: &DictationTimeouts) -> Vec<Row> {
    let timeouts = &file.dictation_timeouts;
    vec![
        row(
            "dictation_timeouts.recording",
            seconds(config.recording),
            Source::of(
                environment.recording_timeout.is_some(),
                timeouts.recording.is_some(),
                RECORDING_TIMEOUT_ENV,
            ),
        ),
        row(
            "dictation_timeouts.delivery",
            seconds(config.delivery),
            Source::of(
                environment.delivery_timeout.is_some(),
                timeouts.delivery.is_some(),
                DELIVERY_TIMEOUT_ENV,
            ),
        ),
        row(
            "dictation_timeouts.overall",
            seconds(config.overall),
            Source::of(
                environment.overall_timeout.is_some(),
                timeouts.overall.is_some(),
                OVERALL_TIMEOUT_ENV,
            ),
        ),
    ]
}

fn wake(
    file: &FileConfig,
    environment: &Environment,
    capture: Option<&CaptureConfig>,
) -> Result<Vec<Row>, ConfigError> {
    let Some(capture) = capture else {
        return Ok(Vec::new());
    };
    let config = load::wake(file, environment, capture, WakeWordOverride::Inherit)?;
    let mut rows = vec![row(
        "wake.enabled",
        !matches!(config, WakeConfig::Disabled),
        Source::of(
            environment.wake_enabled.is_some(),
            file.wake.enabled.is_some(),
            WAKE_ENABLED_ENV,
        ),
    )];
    if let WakeConfig::Enabled {
        backend,
        classifier,
        threshold,
    } = config
    {
        rows.push(row(
            "wake.backend",
            backend.name(),
            Source::of(
                environment.wake_backend.is_some(),
                file.wake.backend.is_some(),
                WAKE_BACKEND_ENV,
            ),
        ));
        rows.push(row(
            "wake.model",
            classifier.display(),
            Source::of(
                environment.wake_model.is_some(),
                file.wake.model.is_some(),
                WAKE_MODEL_ENV,
            ),
        ));
        rows.push(row(
            "wake.threshold",
            threshold,
            Source::of(
                environment.wake_threshold.is_some(),
                file.wake.threshold.is_some(),
                WAKE_THRESHOLD_ENV,
            ),
        ));
    }
    Ok(rows)
}

fn cleanup(file: &FileConfig, environment: &Environment) -> Result<Vec<Row>, ConfigError> {
    Ok(vec![row(
        "dictation_cleanup",
        load::cleanup(file, environment)?,
        Source::of(
            environment.dictation_cleanup.is_some(),
            file.dictation_cleanup.is_some(),
            DICTATION_CLEANUP_ENV,
        ),
    )])
}

fn backtrack(file: &FileConfig, environment: &Environment) -> Result<Vec<Row>, ConfigError> {
    Ok(vec![row(
        "dictation_backtrack",
        load::backtrack(file, environment)?,
        Source::of(
            environment.dictation_backtrack.is_some(),
            file.dictation_backtrack.is_some(),
            DICTATION_BACKTRACK_ENV,
        ),
    )])
}

fn cleanup_llm(file: &FileConfig, environment: &Environment) -> Result<Vec<Row>, ConfigError> {
    let Some(config) = load::cleanup_llm(file, environment)? else {
        return Ok(Vec::new());
    };
    Ok(vec![
        row(
            "dictation_cleanup_llm.endpoint",
            config.endpoint,
            Source::File,
        ),
        row("dictation_cleanup_llm.model", config.model, Source::File),
    ])
}

fn hotkey(file: &FileConfig, environment: &Environment) -> Result<Vec<Row>, ConfigError> {
    let config = load::hotkey(file, environment)?;
    Ok(vec![row(
        "hotkey.enabled",
        matches!(config, HotkeyConfig::Enabled),
        Source::of(
            environment.hotkey_enabled.is_some(),
            file.hotkey.enabled.is_some(),
            HOTKEY_ENABLED_ENV,
        ),
    )])
}

fn relay(file: &FileConfig) -> Vec<Row> {
    let config = load::relay(file);
    let mut rows = vec![row(
        "phone.enabled",
        !matches!(config, RelayConfig::Disabled),
        Source::file_or_default(file.phone.enabled.is_some()),
    )];
    if let RelayConfig::Enabled { port } = config {
        rows.push(row(
            "phone.port",
            port,
            Source::file_or_default(file.phone.port.is_some()),
        ));
    }
    rows
}

fn tts(file: &FileConfig, environment: &Environment) -> Result<Vec<Row>, ConfigError> {
    let config = load::tts(file, environment)?;
    let model = Source::of(
        environment.tts_model.is_some(),
        file.tts.model.is_some(),
        TTS_MODEL_ENV,
    );
    let mut rows = vec![row(
        "tts.backend",
        config.backend.name(),
        Source::of(
            environment.tts_backend.is_some(),
            file.tts.backend.is_some(),
            TTS_BACKEND_ENV,
        ),
    )];
    match &config.backend {
        TtsBackend::Model(TtsModel::Kokoro {
            model: path,
            voices,
        }) => {
            rows.push(row("tts.model", path.display(), model));
            rows.push(row(
                "tts.voices",
                voices.display(),
                Source::of(
                    environment.tts_voices.is_some(),
                    file.tts.voices.is_some(),
                    TTS_VOICES_ENV,
                ),
            ));
        }
        TtsBackend::Model(TtsModel::Qwen {
            model_dir,
            tokenizer_dir,
        }) => {
            rows.push(row("tts.model", model_dir.display(), model));
            rows.push(row(
                "tts.tokenizer",
                tokenizer_dir.display(),
                Source::of(
                    environment.tts_tokenizer.is_some(),
                    file.tts.tokenizer.is_some(),
                    TTS_TOKENIZER_ENV,
                ),
            ));
        }
        TtsBackend::System => {}
    }
    if let Some(voice) = &config.voice {
        rows.push(row(
            "tts.voice",
            voice.as_str(),
            Source::of(
                environment.tts_voice.is_some(),
                file.tts.voice.is_some(),
                TTS_VOICE_ENV,
            ),
        ));
    }
    rows.push(row(
        "tts.rate",
        config.rate.get(),
        Source::of(
            environment.tts_rate.is_some(),
            file.tts.rate.is_some(),
            TTS_RATE_ENV,
        ),
    ));
    let fallback = match config.fallback {
        TtsFallback::None => "none",
        TtsFallback::System => "system",
    };
    rows.push(row(
        "tts.fallback",
        fallback,
        Source::of(
            environment.tts_fallback.is_some(),
            file.tts.fallback.is_some(),
            TTS_FALLBACK_ENV,
        ),
    ));
    Ok(rows)
}

fn chat(
    file: &FileConfig,
    environment: &Environment,
    timeouts: Option<&DictationTimeouts>,
) -> Result<Vec<Row>, ConfigError> {
    // Chat borrows dictation's delivery timeout, so it cannot load without
    // the timeouts section.
    if timeouts.is_none() {
        return Ok(Vec::new());
    }
    let config = load::chat(file, environment)?;
    let mut rows = vec![row(
        "chat.agent",
        config.agent.name(),
        Source::of(
            environment.chat_agent.is_some(),
            file.chat.agent.is_some(),
            CHAT_AGENT_ENV,
        ),
    )];
    if let Some(workdir) = &config.workdir {
        rows.push(row("chat.workdir", workdir.display(), Source::File));
    }
    if !config.project_roots.is_empty() {
        let roots = config
            .project_roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        rows.push(row("chat.project_roots", roots, Source::File));
    }
    rows.push(row(
        "chat.timeout",
        seconds(config.timeout),
        Source::of(
            environment.chat_timeout.is_some(),
            file.chat.timeout.is_some(),
            CHAT_TIMEOUT_ENV,
        ),
    ));
    Ok(rows)
}

fn listen(
    file: &FileConfig,
    environment: &Environment,
    transcription: Option<&TranscriptionConfig>,
) -> Result<Vec<Row>, ConfigError> {
    // Without a backend of its own, listen takes the whole transcription
    // section, and its own model and device settings go unread.
    let own_backend = environment.listen_backend.is_some() || file.listen.backend.is_some();
    if !own_backend && transcription.is_none() {
        return Ok(Vec::new());
    }
    let config = load::listen(file, environment)?;
    let engine = |environment_set: bool, file_set: bool, variable: &'static str| {
        if own_backend {
            Source::of(environment_set, file_set, variable)
        } else {
            Source::Inherited
        }
    };
    let mut rows = vec![row(
        "listen.backend",
        config.transcription.backend_name(),
        engine(
            environment.listen_backend.is_some(),
            file.listen.backend.is_some(),
            LISTEN_BACKEND_ENV,
        ),
    )];
    let model = engine(
        environment.listen_model.is_some(),
        file.listen.model.is_some(),
        LISTEN_MODEL_ENV,
    );
    match &config.transcription {
        ListenTranscription::Parakeet {
            model_directory,
            device,
        } => {
            rows.push(row("listen.model", model_directory.display(), model));
            rows.push(row(
                "listen.device",
                device_name(*device),
                engine(
                    environment.listen_device.is_some(),
                    file.listen.device.is_some(),
                    LISTEN_DEVICE_ENV,
                ),
            ));
        }
        ListenTranscription::Moonshine { model_directory }
        | ListenTranscription::Nemotron { model_directory } => {
            rows.push(row("listen.model", model_directory.display(), model));
        }
    }
    rows.push(row(
        "listen.transcripts_dir",
        config.transcripts_dir.display(),
        Source::of(
            environment.listen_transcripts_dir.is_some(),
            file.listen.transcripts_dir.is_some(),
            LISTEN_TRANSCRIPTS_DIR_ENV,
        ),
    ));
    Ok(rows)
}

fn speaker(file: &FileConfig, environment: &Environment) -> Result<Vec<Row>, ConfigError> {
    let config = load::speaker(file, environment)?;
    Ok(vec![
        row(
            "speaker.model",
            config.model.display(),
            Source::of(
                environment.speaker_model.is_some(),
                file.speaker.model.is_some(),
                SPEAKER_MODEL_ENV,
            ),
        ),
        row(
            "speaker.segmentation_model",
            config.segmentation_model.display(),
            Source::of(
                environment.segmentation_model.is_some(),
                file.speaker.segmentation_model.is_some(),
                SEGMENTATION_MODEL_ENV,
            ),
        ),
        row(
            "speaker.speakers_dir",
            config.speakers_dir.display(),
            Source::of(
                environment.speakers_dir.is_some(),
                file.speaker.speakers_dir.is_some(),
                SPEAKERS_DIR_ENV,
            ),
        ),
    ])
}

fn memory(file: &FileConfig, environment: &Environment) -> Result<Vec<Row>, ConfigError> {
    let config = load::memory(file, environment)?;
    Ok(vec![
        row(
            "memory.enabled",
            config.enabled,
            Source::of(
                environment.memory_enabled.is_some(),
                file.memory.enabled.is_some(),
                MEMORY_ENABLED_ENV,
            ),
        ),
        row(
            "memory.extract_model",
            &config.extract_model,
            Source::of(
                environment.memory_extract_model.is_some(),
                file.memory.extract_model.is_some(),
                MEMORY_EXTRACT_MODEL_ENV,
            ),
        ),
        row(
            "memory.max_facts_per_session",
            config.max_facts_per_session,
            Source::of(
                environment.memory_max_facts.is_some(),
                file.memory.max_facts_per_session.is_some(),
                MEMORY_MAX_FACTS_ENV,
            ),
        ),
        row(
            "memory.dir",
            config.dir.display(),
            Source::of(
                environment.memory_dir.is_some(),
                file.memory.dir.is_some(),
                MEMORY_DIR_ENV,
            ),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::super::file::{
        FileCapture, FileChat, FileHotkey, FileListen, FileMemory, FilePhone, FileSpeaker,
        FileTimeouts, FileTranscription, FileTts, FileWake,
    };
    use super::*;

    fn home_with(config: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("temp home");
        let path = home.path().join(DEFAULT_CONFIG_RELATIVE_PATH);
        fs::create_dir_all(path.parent().expect("config dir")).expect("create dir");
        fs::write(path, config).expect("write config");
        home
    }

    fn shown(home: &Path) -> Report {
        report(&Environment::in_home(home.to_owned())).expect("resolves")
    }

    fn rows(report: &Report) -> Vec<&Row> {
        report
            .rows
            .iter()
            .filter_map(|row| row.as_ref().ok())
            .collect()
    }

    fn failed(report: &Report) -> Vec<&'static str> {
        report
            .rows
            .iter()
            .filter_map(|row| row.as_ref().err())
            .map(|failed| failed.section)
            .collect()
    }

    fn find<'a>(rows: &'a [&Row], key: &str) -> &'a Row {
        rows.iter()
            .find(|row| row.key == key)
            .unwrap_or_else(|| panic!("no row {key}"))
    }

    #[test]
    fn an_empty_home_reads_every_row_as_a_default() {
        let home = tempfile::tempdir().expect("temp home");
        let report = shown(home.path());

        assert!(failed(&report).is_empty(), "{:?}", failed(&report));
        let rows = rows(&report);
        assert!(rows.len() > 10, "{}", rows.len());
        for row in &rows {
            let expected = if row.key.starts_with("listen.") && !row.key.ends_with("_dir") {
                Source::Inherited
            } else {
                Source::Default
            };
            assert_eq!(row.source, expected, "{}", row.key);
        }
        assert_eq!(find(&rows, "transcription.backend").value, "parakeet");
        assert_eq!(find(&rows, "dictation_timeouts.recording").value, "300s");
        assert_eq!(find(&rows, "wake.enabled").value, "false");
        assert_eq!(find(&rows, "dictation_backtrack").value, "false");
    }

    #[test]
    fn a_file_value_is_marked_and_shown_with_the_home_expanded() {
        let home = home_with("transcription:\n  backend: parakeet\n  model: ~/custom\n");
        let report = shown(home.path());
        let rows = rows(&report);

        let backend = find(&rows, "transcription.backend");
        assert_eq!(
            (backend.value.as_str(), backend.source),
            ("parakeet", Source::File)
        );
        let model = find(&rows, "transcription.model");
        assert_eq!(Path::new(&model.value), home.path().join("custom"));
        assert_eq!(model.source, Source::File);
        assert_eq!(find(&rows, "transcription.device").source, Source::Default);
    }

    #[test]
    fn llm_cleanup_reports_settings_and_invalid_enablement() {
        let home = home_with(
            "dictation_cleanup: true\ndictation_cleanup_llm:\n  endpoint: http://127.0.0.1:8080/v1/chat/completions\n  model: local-model\n",
        );
        let report = shown(home.path());
        let rows = rows(&report);
        let endpoint = find(&rows, "dictation_cleanup_llm.endpoint");
        assert_eq!(endpoint.value, "http://127.0.0.1:8080/v1/chat/completions");
        assert_eq!(endpoint.source, Source::File);
        assert_eq!(
            find(&rows, "dictation_cleanup_llm.model").value,
            "local-model"
        );
        let mut environment = Environment::in_home(home.path().to_owned());
        environment.dictation_cleanup = Some("false".to_owned());
        let report = super::report(&environment).unwrap();
        assert!(failed(&report).contains(&"dictation_cleanup_llm"));
    }

    #[test]
    fn an_environment_value_beats_the_file_and_names_its_variable() {
        let home = home_with("tts:\n  rate: 0.5\n");
        let mut environment = Environment::in_home(home.path().to_owned());
        environment.tts_rate = Some("1.4".to_owned());
        let report = report(&environment).expect("resolves");
        let rows = rows(&report);
        let rate = find(&rows, "tts.rate");

        assert_eq!(rate.value, "1.4");
        assert_eq!(rate.source, Source::Environment(TTS_RATE_ENV));
        assert_eq!(rate.source.to_string(), "env ARIA_TTS_RATE");
    }

    #[test]
    fn a_disabled_section_shows_only_that_it_is_off() {
        let off = home_with("wake:\n  enabled: false\n  threshold: 0.9\n");
        let report = shown(off.path());
        let wake: Vec<_> = rows(&report)
            .into_iter()
            .filter(|row| row.key.starts_with("wake."))
            .map(|row| row.key)
            .collect();
        assert_eq!(wake, ["wake.enabled"]);

        let on = home_with("wake:\n  enabled: true\n  threshold: 0.9\n");
        let report = shown(on.path());
        let rows = rows(&report);
        assert_eq!(find(&rows, "wake.threshold").value, "0.9");
        assert_eq!(find(&rows, "wake.model").source, Source::Default);
    }

    #[test]
    fn a_broken_section_fails_alone() {
        let home = home_with("listen:\n  backend: whisper\n");
        let report = shown(home.path());

        assert_eq!(failed(&report), ["listen"]);
        assert!(rows(&report).len() > 10);
    }

    #[test]
    fn a_failed_section_is_reported_once() {
        let home = home_with(
            "capture:\n  backend: tape\ntranscription:\n  backend: parakeet\nwake:\n  enabled: true\n",
        );
        let report = shown(home.path());

        assert_eq!(failed(&report), ["capture"]);
        assert!(!rows(&report).iter().any(|row| row.key.starts_with("wake.")));
    }

    /// A field added to either struct fails to compile here, so whoever
    /// adds it decides whether it gets a row.
    #[test]
    fn every_field_is_accounted_for() {
        let Environment {
            home: _,
            capture_backend: _,
            capture_file: _,
            capture_pace: _,
            transcription_backend: _,
            transcription_model: _,
            transcription_device: _,
            transcription_interpreter: _,
            listen_backend: _,
            listen_model: _,
            listen_device: _,
            listen_transcripts_dir: _,
            speaker_model: _,
            segmentation_model: _,
            speakers_dir: _,
            memory_enabled: _,
            memory_extract_model: _,
            memory_max_facts: _,
            memory_dir: _,
            tts_backend: _,
            tts_model: _,
            tts_voices: _,
            tts_tokenizer: _,
            tts_voice: _,
            tts_rate: _,
            tts_fallback: _,
            chat_agent: _,
            chat_timeout: _,
            wake_enabled: _,
            wake_backend: _,
            wake_model: _,
            wake_threshold: _,
            hotkey_enabled: _,
            dictation_cleanup: _,
            dictation_backtrack: _,
            recording_timeout: _,
            delivery_timeout: _,
            overall_timeout: _,
            unattended: _,
            // The Codex CLI's own setting, not one of Aria's.
            codex_home: _,
        } = Environment::in_home(PathBuf::new());
        let FileConfig {
            capture:
                FileCapture {
                    backend: _,
                    path: _,
                    pace: _,
                },
            transcription:
                FileTranscription {
                    backend: _,
                    model: _,
                    device: _,
                    interpreter: _,
                },
            listen:
                FileListen {
                    backend: _,
                    model: _,
                    device: _,
                    transcripts_dir: _,
                },
            speaker:
                FileSpeaker {
                    model: _,
                    segmentation_model: _,
                    speakers_dir: _,
                },
            wake:
                FileWake {
                    enabled: _,
                    backend: _,
                    model: _,
                    threshold: _,
                },
            hotkey: FileHotkey { enabled: _ },
            dictation_cleanup: _,
            dictation_backtrack: _,
            dictation_cleanup_llm: _,
            phone: FilePhone {
                enabled: _,
                port: _,
            },
            dictation_timeouts:
                FileTimeouts {
                    recording: _,
                    delivery: _,
                    overall: _,
                },
            tts:
                FileTts {
                    backend: _,
                    model: _,
                    voices: _,
                    tokenizer: _,
                    voice: _,
                    rate: _,
                    fallback: _,
                },
            chat:
                FileChat {
                    agent: _,
                    workdir: _,
                    project_roots: _,
                    timeout: _,
                },
            memory:
                FileMemory {
                    enabled: _,
                    extract_model: _,
                    max_facts_per_session: _,
                    dir: _,
                },
        } = FileConfig::default();
    }

    /// Each row names its own variable, so a swapped constant fails here.
    #[test]
    fn every_environment_variable_surfaces_on_its_own_row() {
        let home = tempfile::tempdir().expect("temp home");
        let mut environment = Environment::in_home(home.path().to_owned());
        let set = |value: &str| Some(value.to_owned());
        environment.capture_backend = set("microphone");
        environment.transcription_backend = set("parakeet");
        environment.transcription_model = set("~/m");
        environment.transcription_device = set("gpu");
        environment.recording_timeout = set("10");
        environment.delivery_timeout = set("2");
        environment.overall_timeout = set("20");
        environment.wake_enabled = set("true");
        environment.wake_backend = set("openwakeword");
        environment.wake_model = set("~/w");
        environment.wake_threshold = set("0.7");
        environment.hotkey_enabled = set("false");
        environment.dictation_cleanup = set("true");
        environment.dictation_backtrack = set("true");
        environment.tts_backend = set("kokoro");
        environment.tts_model = set("~/k");
        environment.tts_voices = set("~/v");
        environment.tts_voice = set("af_heart");
        environment.tts_rate = set("1.2");
        environment.tts_fallback = set("system");
        environment.chat_agent = set("codex");
        environment.chat_timeout = set("30");
        environment.listen_backend = set("nemotron");
        environment.listen_model = set("~/l");
        environment.listen_transcripts_dir = set("~/t");
        environment.speaker_model = set("~/s");
        environment.segmentation_model = set("~/g");
        environment.speakers_dir = set("~/d");
        environment.memory_enabled = set("true");
        environment.memory_extract_model = set("opus");
        environment.memory_max_facts = set("3");
        environment.memory_dir = set("~/mem");
        let report = report(&environment).expect("resolves");

        let expected = [
            ("capture.backend", CAPTURE_BACKEND_ENV),
            ("transcription.backend", BACKEND_ENV),
            ("transcription.model", MODEL_ENV),
            ("transcription.device", DEVICE_ENV),
            ("dictation_timeouts.recording", RECORDING_TIMEOUT_ENV),
            ("dictation_timeouts.delivery", DELIVERY_TIMEOUT_ENV),
            ("dictation_timeouts.overall", OVERALL_TIMEOUT_ENV),
            ("dictation_cleanup", DICTATION_CLEANUP_ENV),
            ("dictation_backtrack", DICTATION_BACKTRACK_ENV),
            ("wake.enabled", WAKE_ENABLED_ENV),
            ("wake.backend", WAKE_BACKEND_ENV),
            ("wake.model", WAKE_MODEL_ENV),
            ("wake.threshold", WAKE_THRESHOLD_ENV),
            ("hotkey.enabled", HOTKEY_ENABLED_ENV),
            ("tts.backend", TTS_BACKEND_ENV),
            ("tts.model", TTS_MODEL_ENV),
            ("tts.voices", TTS_VOICES_ENV),
            ("tts.voice", TTS_VOICE_ENV),
            ("tts.rate", TTS_RATE_ENV),
            ("tts.fallback", TTS_FALLBACK_ENV),
            ("chat.agent", CHAT_AGENT_ENV),
            ("chat.timeout", CHAT_TIMEOUT_ENV),
            ("listen.backend", LISTEN_BACKEND_ENV),
            ("listen.model", LISTEN_MODEL_ENV),
            ("listen.transcripts_dir", LISTEN_TRANSCRIPTS_DIR_ENV),
            ("speaker.model", SPEAKER_MODEL_ENV),
            ("speaker.segmentation_model", SEGMENTATION_MODEL_ENV),
            ("speaker.speakers_dir", SPEAKERS_DIR_ENV),
            ("memory.enabled", MEMORY_ENABLED_ENV),
            ("memory.extract_model", MEMORY_EXTRACT_MODEL_ENV),
            ("memory.max_facts_per_session", MEMORY_MAX_FACTS_ENV),
            ("memory.dir", MEMORY_DIR_ENV),
        ];
        let from_environment: Vec<_> = rows(&report)
            .iter()
            .filter_map(|row| match row.source {
                Source::Environment(variable) => Some((row.key, variable)),
                _ => None,
            })
            .collect();
        assert_eq!(from_environment, expected);
    }

    /// The keys only some engines have: the MLX interpreter, the parakeet
    /// device under listen, and the qwen tokenizer.
    #[test]
    fn engine_specific_keys_surface_for_their_engine() {
        let home = home_with(
            "transcription:\n  backend: parakeet-mlx\n  interpreter: ~/python\n\
             listen:\n  backend: parakeet\n  model: ~/l\n  device: gpu\n\
             tts:\n  backend: qwen\n  model: ~/q\n  tokenizer: ~/tok\n",
        );
        let report = shown(home.path());
        let rows = rows(&report);

        for key in [
            "transcription.interpreter",
            "listen.device",
            "tts.tokenizer",
        ] {
            assert_eq!(find(&rows, key).source, Source::File, "{key}");
        }
        assert_eq!(find(&rows, "transcription.model").source, Source::Default);
    }

    #[test]
    fn a_listen_section_of_its_own_stops_inheriting() {
        let home = home_with("listen:\n  backend: moonshine\n");
        let report = shown(home.path());
        let rows = rows(&report);

        assert_eq!(find(&rows, "listen.backend").source, Source::File);
        assert_eq!(find(&rows, "listen.model").source, Source::Default);
    }

    #[test]
    fn a_wav_file_capture_shows_its_path_and_pace() {
        let home =
            home_with("capture:\n  backend: wav-file\n  path: ~/press.wav\n  pace: realtime\n");
        let report = shown(home.path());
        let rows = rows(&report);

        assert_eq!(find(&rows, "capture.backend").value, "wav-file");
        assert_eq!(
            Path::new(&find(&rows, "capture.path").value),
            home.path().join("press.wav")
        );
        let pace = find(&rows, "capture.pace");
        assert_eq!(
            (pace.value.as_str(), pace.source),
            ("realtime", Source::File)
        );
    }

    /// Every key the file can carry has a row that says so, which is what
    /// keeps this module honest when a field is added.
    #[test]
    fn every_file_key_surfaces_as_a_file_row() {
        let home = home_with(
            "capture:\n  backend: microphone\n\
             transcription:\n  backend: parakeet\n  model: ~/m\n  device: gpu\n\
             listen:\n  backend: nemotron\n  model: ~/l\n  transcripts_dir: ~/t\n\
             speaker:\n  model: ~/s\n  segmentation_model: ~/g\n  speakers_dir: ~/d\n\
             wake:\n  enabled: true\n  backend: openwakeword\n  model: ~/w\n  threshold: 0.7\n\
             hotkey:\n  enabled: false\n\
             phone:\n  enabled: true\n  port: 9999\n\
             dictation_timeouts:\n  recording: 10\n  delivery: 2\n  overall: 20\n\
             dictation_cleanup: true\n\
             dictation_backtrack: true\n\
             dictation_cleanup_llm:\n  endpoint: http://127.0.0.1:8080/v1/chat/completions\n  model: local-model\n\
             tts:\n  backend: kokoro\n  model: ~/k\n  voices: ~/v\n  voice: af_heart\n  rate: 1.2\n  fallback: system\n\
             chat:\n  agent: codex\n  workdir: ~/c\n  project_roots: [~/p]\n  timeout: 30\n\
             memory:\n  enabled: true\n  extract_model: opus\n  max_facts_per_session: 3\n  dir: ~/mem\n",
        );
        let report = shown(home.path());
        let rows = rows(&report);

        let expected = [
            "capture.backend",
            "transcription.backend",
            "transcription.model",
            "transcription.device",
            "dictation_timeouts.recording",
            "dictation_timeouts.delivery",
            "dictation_timeouts.overall",
            "dictation_cleanup",
            "dictation_backtrack",
            "dictation_cleanup_llm.endpoint",
            "dictation_cleanup_llm.model",
            "wake.enabled",
            "wake.backend",
            "wake.model",
            "wake.threshold",
            "hotkey.enabled",
            "phone.enabled",
            "phone.port",
            "tts.backend",
            "tts.model",
            "tts.voices",
            "tts.voice",
            "tts.rate",
            "tts.fallback",
            "chat.agent",
            "chat.workdir",
            "chat.project_roots",
            "chat.timeout",
            "listen.backend",
            "listen.model",
            "listen.transcripts_dir",
            "speaker.model",
            "speaker.segmentation_model",
            "speaker.speakers_dir",
            "memory.enabled",
            "memory.extract_model",
            "memory.max_facts_per_session",
            "memory.dir",
        ];
        let from_file: Vec<_> = rows
            .iter()
            .filter(|row| row.source == Source::File)
            .map(|row| row.key)
            .collect();
        assert_eq!(from_file, expected);
    }
}
