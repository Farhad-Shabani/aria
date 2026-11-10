//! How a chat or a prompt run fails, settings included.

use aria_assistant::chat::{
    AssistantError, InstructionsError, ProjectsError, RecordError, StartError,
};
use aria_platform::delivery::DeliveryError;
use aria_provider::ProviderError;
use aria_settings::rpc::RpcErrorCode;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("failed to read settings")]
    Settings(#[source] aria_settings::StoreError),

    #[error("failed to read the known projects")]
    Projects(#[source] ProjectsError),

    #[error("failed to resolve the assistant's working directory")]
    Directory(#[source] std::io::Error),

    #[error(
        "no project is in focus and assistant.workdir is unset; ask from a project once, \
         or set assistant.workdir"
    )]
    NoTarget,
}

impl From<&ResolveError> for RpcErrorCode {
    fn from(error: &ResolveError) -> Self {
        match error {
            ResolveError::Projects(_) | ResolveError::Settings(_) => Self::Internal,
            ResolveError::NoTarget | ResolveError::Directory(_) => Self::InvalidRequest,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error(transparent)]
    Assistant(#[from] AssistantError),

    #[error("failed to deliver the reply")]
    Delivery(#[source] DeliveryError),

    #[error(transparent)]
    Record(#[from] RecordError),

    #[error(transparent)]
    Start(#[from] StartError),

    #[error("failed to read shared settings")]
    Settings(#[source] aria_settings::StoreError),

    #[error("no stored prompt has that ID")]
    UnknownPrompt,

    #[error("that prompt is turned off")]
    PromptOff,

    #[error("failed to read the history entry")]
    History(#[source] aria_voice::history::HistoryError),

    #[error("the chat is still replying; try again once it's done")]
    Replying,

    #[error("no chat has that name yet; ask with a folder to start it")]
    UnknownChat,

    #[error("failed to delete the chat from history")]
    DeleteChat(#[source] aria_voice::history::HistoryError),

    #[error("another chat of that assistant already has the name")]
    NameTaken,

    #[error("failed to rename the chat in history")]
    RenameChat(#[source] aria_voice::history::HistoryError),

    #[error(transparent)]
    Instructions(#[from] InstructionsError),
}

impl From<&ChatError> for RpcErrorCode {
    fn from(error: &ChatError) -> Self {
        match error {
            ChatError::Record(_)
            | ChatError::Start(StartError::Record(_))
            | ChatError::Settings(_) => Self::Internal,
            ChatError::NameTaken | ChatError::Start(StartError::NoFreeName) => Self::ChatNameTaken,
            ChatError::Assistant(
                AssistantError::Missing { .. }
                | AssistantError::NotConfigured { .. }
                | AssistantError::ModelUnset { .. }
                | AssistantError::ModelMissing { .. }
                | AssistantError::Provider(ProviderError::Transport),
            ) => Self::AssistantUnavailable,
            ChatError::Assistant(AssistantError::Provider(
                ProviderError::Cancelled
                | ProviderError::Timeout
                | ProviderError::HttpStatus
                | ProviderError::HeadersTooLarge
                | ProviderError::ResponseTooLarge
                | ProviderError::InvalidResponse
                | ProviderError::Truncated,
            )) => Self::AssistantFailed,
            ChatError::Assistant(AssistantError::NoWorkdir(_))
            | ChatError::PromptOff
            | ChatError::UnknownChat => Self::InvalidRequest,
            ChatError::UnknownPrompt => Self::PromptMissing,
            ChatError::History(error)
            | ChatError::DeleteChat(error)
            | ChatError::RenameChat(error) => Self::from(error),
            ChatError::Replying => Self::Busy,
            ChatError::Instructions(_) => Self::PromptUnreadable,
            ChatError::Assistant(AssistantError::Timeout(_)) => Self::AssistantTimeout,
            ChatError::Assistant(AssistantError::SignedOut { .. }) => Self::AssistantSignedOut,
            ChatError::Assistant(AssistantError::Cancelled)
            | ChatError::Delivery(DeliveryError::Cancelled) => Self::Cancelled,
            ChatError::Assistant(
                AssistantError::Run(_)
                | AssistantError::RunDirectory(_)
                | AssistantError::Failed(_)
                | AssistantError::Malformed(_),
            ) => Self::AssistantFailed,
            ChatError::Delivery(DeliveryError::Timeout(_)) => Self::DeliveryTimeout,
            ChatError::Delivery(
                DeliveryError::TargetChanged
                | DeliveryError::CaretMoved
                | DeliveryError::TargetUnverified,
            ) => Self::DeliveryTargetChanged,
            ChatError::Delivery(DeliveryError::ApplicationExcluded) => Self::ApplicationExcluded,
            ChatError::Delivery(
                DeliveryError::EmptyText
                | DeliveryError::TextTooLarge { .. }
                | DeliveryError::ControlCharacter
                | DeliveryError::InvalidTimeout { .. }
                | DeliveryError::ClipboardRejected
                | DeliveryError::TargetUnavailable
                | DeliveryError::InsertionUnsupported,
            ) => Self::DeliveryFailed,
            #[cfg(target_os = "linux")]
            ChatError::Delivery(
                DeliveryError::SubmissionUnsupported
                | DeliveryError::PasteUnconfirmed
                | DeliveryError::InsertionNotPermitted,
            ) => Self::DeliveryFailed,
            #[cfg(target_os = "macos")]
            ChatError::Delivery(
                DeliveryError::SubmissionUnconfirmed
                | DeliveryError::InsertionNotPermitted
                | DeliveryError::SecureInput,
            ) => Self::DeliveryFailed,
        }
    }
}
