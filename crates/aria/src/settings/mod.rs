//! One settings file, `~/.aria/settings.toml`, that every program edits.
//!
//! The file is the source of truth; the daemon's copy is a cache of the last
//! valid file. One write path locks, re-reads, edits only the keys named,
//! validates the whole result, and replaces the file atomically. Comments and
//! unrelated keys survive every write.
mod choices;
mod chord;
mod command;
mod document;
mod keys;
mod overrides;
mod prompts;
mod store;
mod view;
mod watch;
mod write;

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aria_kit::synthesis::{SpeakingRateError, VoiceError};

use crate::audio::{AutoStopError, SilenceDurationError};
use crate::dictation::CleanupError;
use crate::transcription::LanguageCodeError;
use crate::vocabulary::DirectoryError;

pub use choices::{Settings, Shortcut, StoreError};
pub use chord::{Chord, ChordError, Modifiers};
pub use command::{
    CommandError, Outcome, Row, banner, create, path, reset, schema, set, show, validate,
};
pub(crate) use document::{
    ApplicationOverrideEntry, AriaKeyName, Capture, CaptureBackendName, Delivery, DeliveryModeName,
    DeviceName, Dictation, Keyed, Loaded, Memory, OutputModeName, PaceName, Phone, PromptEntry,
    Revision, SettingsFile, ShortcutEntry, Speaker, Stt, Tts, TtsFallbackName, Wake, invalid,
};
pub(crate) use keys::Scope;
pub use keys::{Apply, Key};
pub use overrides::{
    ApplicationId, ApplicationOverride, ApplicationOverrides, DictationOverrides, OverrideError,
};
pub use prompts::{Prompt, PromptError, PromptId, Prompts};
pub(crate) use store::SettingsStore;
pub use store::{SettingsPatch, SettingsSnapshot, UpdateResult};
pub(crate) use watch::{SettingsCache, SettingsPoll};
pub(crate) use write::{Patch, Refusal, Written, check_patch, modify, write_schema_beside};

/// What a client sees of the file beyond its values: whether it is valid,
/// and what waits for a restart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsStatus {
    /// `None` while the file is valid.
    pub fault: Option<SettingsFault>,
    /// Dotted restart-only keys whose file value differs from the running one.
    pub restart_required: Vec<String>,
    /// Dotted keys the daemon wrote that the last reload changed back.
    pub reverted: Vec<String>,
    /// The hash of the values held, which every value change moves.
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsFault {
    pub message: String,
    pub position: Option<Position>,
}

const SETTINGS_RELATIVE_PATH: &str = ".aria/settings.toml";
const LOCK_FILE_NAME: &str = "settings.lock";
pub(crate) const MAX_SETTINGS_BYTES: u64 = 1024 * 1024;

pub(crate) fn default_path(home: &Path) -> PathBuf {
    home.join(SETTINGS_RELATIVE_PATH)
}

/// The settings a one-shot command reads: the file at `home`.
pub(crate) fn load(home: &Path) -> Result<SettingsFile, SettingsError> {
    let path = default_path(home);
    let text = document::read_text(&path)?;
    let loaded = Loaded::parse(text.as_ref().map_or("", |text| &text.raw), home)?;
    Ok(loaded.settings)
}

/// The lock sits beside the path the user names, not the symlink target, so
/// every program that names the same file takes the same lock.
fn lock_path(path: &Path) -> PathBuf {
    path.with_file_name(LOCK_FILE_NAME)
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("failed to {action} {}", path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{} exceeds the maximum size of {MAX_SETTINGS_BYTES} bytes", path.display())]
    TooLarge { path: PathBuf },

    #[error("{} is not valid UTF-8", path.display())]
    InvalidUtf8 { path: PathBuf },

    #[error("{} is not a regular file", path.display())]
    NotAFile { path: PathBuf },

    #[error("{} changed while it was being read", path.display())]
    TornRead { path: PathBuf },

    #[error("settings.toml is invalid: {0}")]
    Parse(ParseError),

    #[error("{}{key} {source}", at(*position))]
    Invalid {
        key: Key,
        #[source]
        source: InvalidValue,
        /// Where the key sits in the file, when the file holds it.
        position: Option<Position>,
    },

    #[error("there is no setting named {0}")]
    UnknownKey(String),

    #[error("another program holds {}", path.display())]
    Locked { path: PathBuf },

    #[error("{} is read-only; change it where it's managed", path.display())]
    ReadOnly { path: PathBuf },

    #[error("the settings serialize to more than {MAX_SETTINGS_BYTES} bytes")]
    SerializedTooLarge,

    #[error("{key} has no TOML form")]
    Encode {
        key: Key,
        #[source]
        source: toml_edit::ser::Error,
    },

    #[error("the startup values of the restart keys do not fit the settings")]
    Freeze(#[source] serde_json::Error),

    #[error("failed to spawn the settings poll thread")]
    Thread(#[source] io::Error),

    #[error("settings.toml sets {key} and its old name {alias}; remove {alias}")]
    KeyAndAlias { key: Key, alias: String },
}

/// A TOML fault with its position. The message names keys and expectations,
/// never the text around them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: String,
    position: Option<Position>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl SettingsError {
    /// Where in the file the fault is, when it has a place.
    pub fn position(&self) -> Option<Position> {
        match self {
            Self::Parse(error) => error.position,
            Self::Invalid { position, .. } => *position,
            Self::Io { .. }
            | Self::TooLarge { .. }
            | Self::InvalidUtf8 { .. }
            | Self::NotAFile { .. }
            | Self::TornRead { .. }
            | Self::UnknownKey(_)
            | Self::Locked { .. }
            | Self::ReadOnly { .. }
            | Self::SerializedTooLarge
            | Self::Encode { .. }
            | Self::Freeze(_)
            | Self::Thread(_)
            | Self::KeyAndAlias { .. } => None,
        }
    }

    /// A fault in what the file holds, which only an edit of the file can
    /// fix. Anything else is the machine's and may pass.
    pub(crate) fn is_file_fault(&self) -> bool {
        match self {
            Self::Parse(_)
            | Self::Invalid { .. }
            | Self::TooLarge { .. }
            | Self::InvalidUtf8 { .. }
            | Self::NotAFile { .. }
            | Self::KeyAndAlias { .. } => true,
            Self::Io { .. }
            | Self::TornRead { .. }
            | Self::UnknownKey(_)
            | Self::Locked { .. }
            | Self::ReadOnly { .. }
            | Self::SerializedTooLarge
            | Self::Encode { .. }
            | Self::Freeze(_)
            | Self::Thread(_) => false,
        }
    }

    /// The fault as a client shows it: the message without its position,
    /// which travels apart.
    pub(crate) fn fault(&self) -> SettingsFault {
        let message = match self {
            Self::Parse(error) => error.message.clone(),
            Self::Invalid { key, source, .. } => format!("{key} {source}"),
            other => crate::primitives::error_chain(other),
        };
        SettingsFault {
            message,
            position: self.position(),
        }
    }
}

impl ParseError {
    fn new(message: &str, span: Option<std::ops::Range<usize>>, text: &str) -> Self {
        Self {
            message: message.to_owned(),
            position: span.map(|span| Position::of(text, span.start)),
        }
    }

    pub fn position(&self) -> Option<Position> {
        self.position
    }
}

impl Position {
    fn of(text: &str, offset: usize) -> Self {
        let before = &text[..offset.min(text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().map_or(0, str::len) + 1;
        Self { line, column }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "line {}, column {}", self.line, self.column)
    }
}

fn at(position: Option<Position>) -> String {
    position.map_or_else(String::new, |position| format!("{position}: "))
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.position {
            Some(position) => write!(formatter, "{} at {position}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidValue {
    #[error("must not be empty")]
    Empty,

    #[error("must be an absolute path or start with ~")]
    RelativePath,

    #[error("does not accept {value:?}; supported values are {supported}")]
    Unsupported { value: String, supported: String },

    #[error("must be a finite number of seconds greater than zero; got {0}")]
    Duration(f64),

    #[error("must be at most {maximum:?}; got {actual:?}")]
    TooLong { actual: Duration, maximum: Duration },

    #[error("({recording:?}) must be shorter than dictation.timeout_secs ({overall:?})")]
    RecordingOutlastsOverall {
        recording: Duration,
        overall: Duration,
    },

    #[error("({overall:?}) must be longer than dictation.max_recording_secs ({recording:?})")]
    OverallWithinRecording {
        recording: Duration,
        overall: Duration,
    },

    #[error("must be finite, greater than zero, and at most one; got {0}")]
    Threshold(f32),

    #[error("does not apply to the {backend} backend")]
    NotApplicable { backend: String },

    #[error("is required by the {backend} backend")]
    Required { backend: String },

    #[error("must be an HTTPS origin without credentials, query or fragment")]
    ServerEndpoint,

    #[error("needs capture.backend = \"microphone\"")]
    NeedsMicrophone,

    #[error("uses unsupported home syntax")]
    HomeSyntax,

    #[error("must be a table")]
    NotATable,

    #[error(
        "model IDs must be at most 200 bytes, contain no whitespace or control characters, and not start with a dash"
    )]
    ModelId,

    #[error("sets one shortcut for two things")]
    SharedBinding,

    #[error("is not usable: {0}")]
    Cleanup(#[source] CleanupError),

    #[error("is not usable: {0}")]
    SpeakingRate(#[source] SpeakingRateError),

    #[error("is not usable: {0}")]
    Voice(#[source] VoiceError),

    #[error("is not usable: {0}")]
    Language(#[source] LanguageCodeError),

    #[error("is not usable: {0}")]
    SilenceDuration(#[source] SilenceDurationError),

    #[error("is not usable: {0}")]
    AutoStop(#[source] AutoStopError),

    #[error("is not usable: {0}")]
    Directory(#[source] DirectoryError),

    #[error("is not usable: {0}")]
    Chord(#[source] ChordError),

    #[error("is not usable: {0}")]
    Prompt(#[source] PromptError),

    #[error("is not usable: {0}")]
    Override(#[source] OverrideError),
}
