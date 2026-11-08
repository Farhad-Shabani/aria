mod store;

pub use store::{MAX_ENTRY_BYTES, RetryLease, Store};

use aria_primitives::Cause;
use std::path::PathBuf;

use aria_platform::delivery::Application;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};

#[derive(Clone)]
pub struct HistoryEntry {
    pub id: u64,
    pub title: Option<Title>,
    pub content: HistoryContent,
    pub created_at_ms: u64,
    pub duration_ms: Option<u64>,
    pub source: HistorySource,
    pub application: Option<Application>,
    pub project: Option<PathBuf>,
    pub text: String,
    pub complete: bool,
    /// How many prompt results are saved under the entry.
    pub result_count: u32,
    pub status: HistoryStatus,
    pub error: Option<RpcFailure>,
    pub warning: Option<String>,
    pub deletion_pending: bool,
}

impl HistoryEntry {
    pub fn dictation(&self) -> Option<&DictationContent> {
        match &self.content {
            HistoryContent::Dictation(dictation) => Some(dictation),
            HistoryContent::Listen { .. } | HistoryContent::Ask { .. } => None,
        }
    }

    pub fn dictation_mut(&mut self) -> Option<&mut DictationContent> {
        match &mut self.content {
            HistoryContent::Dictation(dictation) => Some(dictation),
            HistoryContent::Listen { .. } | HistoryContent::Ask { .. } => None,
        }
    }

    /// Only a dictation keeps its recording.
    pub fn audio(&self) -> AudioAvailability {
        self.dictation()
            .map_or(AudioAvailability::Unavailable, |dictation| dictation.audio)
    }

    /// What a prompt run on the entry reads: an ask's reply, else its text,
    /// with a Mac Audio session's transcript read in full. A part of a
    /// transcript is refused, since the result is saved as covering the entry.
    pub fn prompt_text(self) -> Result<String, HistoryError> {
        let text = match self.content {
            HistoryContent::Ask { reply } => reply,
            HistoryContent::Listen {
                read: Some(TranscriptRead::Whole),
                ..
            }
            | HistoryContent::Dictation(_) => self.text,
            HistoryContent::Listen {
                read: None | Some(TranscriptRead::Truncated | TranscriptRead::Unavailable),
                ..
            } => return Err(HistoryError::TranscriptUnreadable),
        };
        if text.trim().is_empty() {
            return Err(HistoryError::EntryEmpty);
        }
        Ok(text)
    }
}

/// A prompt's reply, saved under the history entry it ran on. Results show
/// only with their entry: List leaves them out, and search skips them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptResult {
    pub id: u64,
    pub created_at_ms: u64,
    /// The prompt's name when it ran. Renaming the prompt later doesn't change it.
    pub prompt_name: String,
    pub text: String,
}

/// What a section's entries carry beyond the shared fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryContent {
    Dictation(DictationContent),
    /// A Mac Audio session. Its text stays in the transcript file, and the
    /// entry's text is an excerpt of it.
    Listen {
        transcript: PathBuf,
        /// How much of the file Get read into the entry's text. `None` from
        /// List and insert, where the text is the excerpt.
        read: Option<TranscriptRead>,
    },
    /// A question to the assistant, in the entry's text, and its reply.
    Ask {
        reply: String,
    },
}

impl HistoryContent {
    pub fn section(&self) -> HistorySection {
        match self {
            Self::Dictation(_) => HistorySection::Dictation,
            Self::Listen { .. } => HistorySection::Listen,
            Self::Ask { .. } => HistorySection::Ask,
        }
    }
}

/// How a dictation was transcribed, and whether its recording is kept to
/// transcribe it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DictationContent {
    pub language: String,
    pub engine: String,
    pub own_voice_only: bool,
    pub audio: AudioAvailability,
    pub retrying: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptRead {
    Whole,
    /// The file is longer than Get reads. The text is its start.
    Truncated,
    /// The file is gone or unreadable. The text is the excerpt.
    Unavailable,
}

/// One kind of saved item. Each section keeps its own budget, so one never
/// pushes another's entries out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HistorySection {
    Dictation,
    Listen,
    Ask,
}

impl HistorySection {
    pub const ALL: [Self; 3] = [Self::Dictation, Self::Listen, Self::Ask];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dictation => "dictation",
            Self::Listen => "listen",
            Self::Ask => "ask",
        }
    }
}

/// A name someone gave an entry: trimmed, not empty, at most `MAX_BYTES`, with no control
/// characters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Title(String);

impl Title {
    pub const MAX_BYTES: usize = 256;

    pub fn new(title: &str) -> Result<Self, HistoryError> {
        let title = title.trim();
        if title.is_empty() || title.len() > Self::MAX_BYTES || title.chars().any(char::is_control)
        {
            return Err(HistoryError::InvalidTitle);
        }
        Ok(Self(title.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistorySource {
    Local,
    Relay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryStatus {
    Success,
    TranscriptionFailed,
    DeliveryFailed,
    Recovered,
}

impl HistoryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::TranscriptionFailed => "transcription_failed",
            Self::DeliveryFailed => "delivery_failed",
            Self::Recovered => "recovered",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioAvailability {
    Unavailable,
    Available { expires_at_ms: u64 },
    Expired,
}

impl AudioAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available { .. } => "available",
            Self::Expired => "expired",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RetryOptions {
    pub engine: Option<String>,
    pub language: Option<crate::transcription::Language>,
}

pub struct HistoryQuery {
    /// `None` lists every section.
    pub section: Option<HistorySection>,
    pub query: String,
    pub before_id: Option<u64>,
    pub limit: usize,
}

impl HistoryQuery {
    pub const MAX_QUERY_BYTES: usize = 256;
}

pub struct HistoryPage {
    pub entries: Vec<HistoryEntry>,
    pub next_before_id: Option<u64>,
    pub warning: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ClearHistoryResult {
    pub deleted_entries: u64,
    pub cleanup_pending: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("history entry was not found")]
    Missing,
    #[error("history retry was cancelled")]
    Cancelled,
    #[error("history retry timed out")]
    Timeout,
    /// Carries the failed retry's own code when it has one, so a retry that
    /// heard no speech reports `NoSpeech`, not a generic failure.
    #[error("history transcription failed")]
    Transcription {
        code: RpcErrorCode,
        #[source]
        cause: Cause,
    },
    #[error("the recording is unavailable for retry")]
    AudioUnavailable,
    #[error("history is busy")]
    Busy,
    #[error("The original project directory is unavailable; restore it before retrying.")]
    ProjectUnavailable,
    #[error("invalid history request")]
    InvalidRequest,
    #[error("a title must be one line of 1 to {} bytes", Title::MAX_BYTES)]
    InvalidTitle,
    #[error("that history entry has no text to run a prompt on")]
    EntryEmpty,
    #[error("that Mac Audio transcript can't be read in full")]
    TranscriptUnreadable,
    #[error("history is unavailable")]
    Unavailable,
    #[error("history exceeds its storage limit")]
    TooLarge,
    #[error("history storage failed")]
    Storage(#[source] Cause),
}

impl HistoryError {
    pub fn transcription(
        code: RpcErrorCode,
        cause: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self::Transcription {
            code,
            cause: Box::new(cause),
        }
    }

    pub fn storage(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Storage(Box::new(error))
    }
}

impl From<&HistoryError> for RpcErrorCode {
    fn from(error: &HistoryError) -> Self {
        match error {
            HistoryError::Missing => Self::HistoryMissing,
            HistoryError::AudioUnavailable => Self::HistoryAudioUnavailable,
            HistoryError::Busy => Self::Busy,
            HistoryError::InvalidRequest | HistoryError::InvalidTitle => Self::InvalidRequest,
            HistoryError::EntryEmpty => Self::HistoryEntryEmpty,
            HistoryError::TranscriptUnreadable => Self::HistoryTranscriptUnreadable,
            HistoryError::ProjectUnavailable => Self::HistoryProjectUnavailable,
            HistoryError::Unavailable | HistoryError::Storage(_) => Self::HistoryUnavailable,
            HistoryError::TooLarge => Self::PayloadTooLarge,
            HistoryError::Cancelled => Self::Cancelled,
            HistoryError::Timeout => Self::OverallTimeout,
            HistoryError::Transcription { code, .. } => *code,
        }
    }
}

impl From<aria_platform::sqlite::Error> for HistoryError {
    fn from(error: aria_platform::sqlite::Error) -> Self {
        use aria_platform::sqlite::Error;
        match error {
            Error::Busy => Self::Busy,
            Error::TooLarge => Self::TooLarge,
            Error::Database(error) => error.into(),
            error @ (Error::Path
            | Error::Directory(_)
            | Error::DirectoryNotPrivate
            | Error::File(_)
            | Error::Linked
            | Error::Replaced
            | Error::Cancelled
            | Error::Io { .. }) => Self::storage(error),
        }
    }
}

impl From<rusqlite::Error> for HistoryError {
    fn from(error: rusqlite::Error) -> Self {
        match error {
            rusqlite::Error::SqliteFailure(cause, _)
                if matches!(
                    cause.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) =>
            {
                Self::Busy
            }
            rusqlite::Error::SqliteFailure(cause, _)
                if cause.code == rusqlite::ErrorCode::DiskFull =>
            {
                Self::TooLarge
            }
            error => Self::storage(error),
        }
    }
}
