use std::path::PathBuf;

pub use crate::delivery::Application;
use crate::rpc::{RpcErrorCode, RpcFailure};

#[derive(Clone)]
pub struct HistoryEntry {
    pub id: u64,
    pub created_at_ms: u64,
    pub source: HistorySource,
    pub application: Option<Application>,
    pub duration_ms: Option<u64>,
    pub text: String,
    pub complete: bool,
    pub status: HistoryStatus,
    pub error: Option<RpcFailure>,
    pub audio: AudioAvailability,
    pub retrying: bool,
    pub deletion_pending: bool,
    pub language: String,
    pub(crate) engine: String,
    pub(crate) own_voice_only: bool,
    pub(crate) project: Option<PathBuf>,
    pub warning: Option<String>,
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
    pub query: String,
    pub before_id: Option<u64>,
    pub limit: usize,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    #[error("dictation history entry was not found")]
    Missing,
    #[error("dictation history retry was cancelled")]
    Cancelled,
    #[error("dictation history retry timed out")]
    Timeout,
    #[error("dictation history transcription failed")]
    Transcription,
    #[error("the recording is unavailable for retry")]
    AudioUnavailable,
    #[error("dictation history is busy")]
    Busy,
    #[error("The original project directory is unavailable; restore it before retrying.")]
    ProjectUnavailable,
    #[error("invalid dictation history request")]
    InvalidRequest,
    #[error("dictation history is unavailable")]
    Unavailable,
    #[error("dictation history exceeds its storage limit")]
    TooLarge,
    #[error("dictation history storage failed")]
    Storage,
}

impl From<HistoryError> for RpcFailure {
    fn from(error: HistoryError) -> Self {
        let code = match error {
            HistoryError::Missing => RpcErrorCode::HistoryMissing,
            HistoryError::AudioUnavailable => RpcErrorCode::HistoryAudioUnavailable,
            HistoryError::Busy => RpcErrorCode::Busy,
            HistoryError::InvalidRequest => RpcErrorCode::InvalidRequest,
            HistoryError::ProjectUnavailable => RpcErrorCode::HistoryProjectUnavailable,
            HistoryError::Unavailable | HistoryError::Storage => RpcErrorCode::HistoryUnavailable,
            HistoryError::TooLarge => RpcErrorCode::PayloadTooLarge,
            HistoryError::Cancelled => RpcErrorCode::Cancelled,
            HistoryError::Timeout => RpcErrorCode::OverallTimeout,
            HistoryError::Transcription => RpcErrorCode::TranscriptionFailed,
        };
        Self::new(code, error.to_string())
    }
}
