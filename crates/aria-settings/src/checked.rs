//! The engine choices of a checked settings file. The check builds them, so a
//! config loader reads them and checks nothing again.

use std::ops::Deref;
use std::path::PathBuf;
use std::time::Duration;

use aria_kit::synthesis::{SpeakingRate, Voice};

use crate::config::{backend, tts_backend, wake_backend};
use crate::{LlmCleanupTarget, LocalProvider, RunsOn, SettingsError, SettingsFile};

/// A settings file that passed every check, with the engine choices the
/// check built.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedSettings {
    file: SettingsFile,
    choices: Choices,
}

impl CheckedSettings {
    pub fn new(file: SettingsFile) -> Result<Self, SettingsError> {
        let choices = file.check()?;
        Ok(Self { file, choices })
    }

    pub fn choices(&self) -> &Choices {
        &self.choices
    }
}

impl Deref for CheckedSettings {
    type Target = SettingsFile;

    fn deref(&self) -> &SettingsFile {
        &self.file
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Choices {
    /// The local engine, kept while the server recognizes.
    pub stt: SttChoice,
    pub stt_runs_on: RunsOn,
    /// `None` while system transcription runs the stt engine.
    pub system_transcription: Option<LocalStt>,
    pub tts: TtsEngine,
    pub voices: Voices,
    pub rate: SpeakingRate,
    pub timeouts: DictationTimeouts,
    pub local_provider: Option<LocalProvider>,
    pub cleanup: Option<LlmCleanupTarget>,
    pub capture: CaptureChoice,
    pub wake: WakeEngine,
    pub assistant_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttChoice {
    Local(LocalStt),
    ParakeetMlx { interpreter: PathBuf },
}

impl Choices {
    /// Whether a dictation can type during recording. The server's own engine
    /// decides for itself, so it isn't held back here.
    pub fn stt_streams(&self) -> bool {
        match (self.stt_runs_on, &self.stt) {
            (RunsOn::Server, _) => true,
            (RunsOn::Local, SttChoice::Local(engine)) => engine.streams(),
            (RunsOn::Local, SttChoice::ParakeetMlx { .. }) => false,
        }
    }
}

/// An engine that runs in process. Its model and device come from the
/// section that named it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalStt {
    Whisper,
    WhisperCandle,
    Parakeet,
    Moonshine,
    Nemotron,
}

impl LocalStt {
    pub fn named(name: &str) -> Option<Self> {
        match name {
            backend::WHISPER => Some(Self::Whisper),
            backend::WHISPER_CANDLE => Some(Self::WhisperCandle),
            backend::PARAKEET => Some(Self::Parakeet),
            backend::MOONSHINE => Some(Self::Moonshine),
            backend::NEMOTRON => Some(Self::Nemotron),
            _ => None,
        }
    }

    pub(crate) fn streams(self) -> bool {
        backend::STREAMING
            .iter()
            .any(|name| Self::named(name) == Some(self))
    }
}

/// An engine that speaks, which owns its saved voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtsEngine {
    Kokoro,
    Qwen,
    System,
}

impl TtsEngine {
    pub(crate) fn named(name: &str) -> Option<Self> {
        match name {
            tts_backend::KOKORO => Some(Self::Kokoro),
            tts_backend::QWEN => Some(Self::Qwen),
            tts_backend::SYSTEM => Some(Self::System),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Voices {
    pub system: Option<Voice>,
    pub kokoro: Option<Voice>,
    pub qwen: Option<Voice>,
}

/// The timeouts with their defaults filled in. Only the check builds one,
/// so a recording always ends before the dictation's overall timeout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DictationTimeouts {
    pub(crate) recording: Duration,
    pub(crate) overall: Duration,
    pub(crate) delivery: Duration,
}

impl DictationTimeouts {
    pub fn recording(&self) -> Duration {
        self.recording
    }

    pub fn overall(&self) -> Duration {
        self.overall
    }

    pub fn delivery(&self) -> Duration {
        self.delivery
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureChoice {
    Microphone,
    WavFile { path: PathBuf },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeEngine {
    OpenWakeWord,
    LiveKit,
}

impl WakeEngine {
    pub(crate) fn named(name: &str) -> Option<Self> {
        match name {
            wake_backend::OPEN_WAKE_WORD => Some(Self::OpenWakeWord),
            wake_backend::LIVEKIT => Some(Self::LiveKit),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::OpenWakeWord => wake_backend::OPEN_WAKE_WORD,
            Self::LiveKit => wake_backend::LIVEKIT,
        }
    }
}
