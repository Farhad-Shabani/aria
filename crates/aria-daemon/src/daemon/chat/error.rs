//! How an ask or a prompt run fails, settings included.

use aria_assistant::chat::{AssistantError, InstructionsError, ProjectsError, SessionStoreError};
use aria_platform::delivery::DeliveryError;
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
    Session(#[from] SessionStoreError),

    #[error("failed to read shared settings")]
    Settings(#[source] aria_settings::StoreError),

    #[error("no stored prompt has that ID")]
    UnknownPrompt,

    #[error("that prompt is turned off")]
    PromptOff,

    #[error(transparent)]
    Instructions(#[from] InstructionsError),
}

impl From<&ChatError> for RpcErrorCode {
    fn from(error: &ChatError) -> Self {
        match error {
            ChatError::Session(_) | ChatError::Settings(_) => Self::Internal,
            ChatError::Assistant(AssistantError::Missing { .. }) => Self::AssistantUnavailable,
            ChatError::Assistant(AssistantError::NoWorkdir(_))
            | ChatError::UnknownPrompt
            | ChatError::PromptOff => Self::InvalidRequest,
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
                | DeliveryError::InsertionUnsupported
                | DeliveryError::StreamingUnsupported,
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
