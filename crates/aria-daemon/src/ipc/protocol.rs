//! Translation between generated wire messages and domain types.

use super::wire;
use crate::daemon::relay::{Invite, RelayStage};
use aria_assistant::chat::{
    AskCall, AssistantModel, AssistantStatus, ChatChoice, ChatReply, ChatStage, ChatStageKind,
    ChatTarget, DeliverCall, Instructions, PendingAsk, PromptCall, PromptChoice, PromptInput, Step,
    Ticket,
};
use aria_assistant::prompts::{NewSource, PromptContents, PromptDraft, PromptSource};
use aria_kit::audio::{AutoStop, AutoStopError, SilenceDuration, SilenceDurationError};
use aria_kit::synthesis::{
    SpeakingRate, SpeakingRateError, SpeechText, SpeechTextError, Voice, VoiceError,
};
use aria_platform::delivery::{OutputMode, Placement, Rect};
use aria_platform::hotkey::{AriaKey, Combo};
use aria_settings::config::AssistantId;
use aria_settings::rpc::{RpcErrorCode, RpcFailure};
use aria_settings::{
    ApplicationId, ApplicationOverride, ApplicationOverrides, ChatKey, ChatName, ChatNameError,
    Chord, ChordError, DeliveryMode, DictationOverrides, Modifiers, OverrideError, Position,
    Prompt, PromptError, PromptId, PromptKind, Prompts, Settings, SettingsFault, SettingsPatch,
    SettingsSnapshot, SettingsStatus, Shortcut, StreamingSupport, StreamingUnavailable,
    StreamingUnavailableOutput, UpdateResult,
};
use aria_voice::dictation::{
    CaptionSnapshot, DeliveryOutcome, DictationEvent, DictationOutcome, DictationRequest,
    DictationResult, DictationRoute, DictationStage, GhostSnapshot, HeardSpeaker, MicLevel,
    RecordingId, StopMode,
};
use aria_voice::enrollment::{
    EnrolledSpeaker, EnrollmentRequest, EnrollmentResult, EnrollmentStage,
};
use aria_voice::history::{ChatPage, ChatQuery, ClearHistoryResult, PromptResult};
use aria_voice::speaker::{Role, SpeakerName, SpeakerNameError};
use aria_voice::speech::{SpeechOutcome, SpeechRequest, SpeechStage};
use aria_voice::stt::LanguageCodeError;
use aria_voice::system_transcription::{
    SystemTranscriptionSnapshot, SystemTranscriptionStage, SystemTranscriptionStarted,
    SystemTranscriptionStopped,
};
use prost::Message;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tonic::Status;

mod inference;

// Handlers and the client speak domain types; every wire message and status
// is assembled through the conversions here. A handler builds a wire message
// by hand only when it is empty or carries one scalar the call returned.

/// `grpc-status-details-bin` carries a `google.rpc.Status` by convention.
const RPC_ERROR_TYPE_URL: &str = "type.googleapis.com/aria.daemon.v1.RpcError";

/// The message goes into the trailer three times: `grpc-message`, the status
/// details and the `RpcError`. A client caps that header list, so past the cap
/// the verdict is dropped, and far past it the connection closes.
const MAX_FAILURE_MESSAGE_BYTES: usize = 1024;

/// The status code is the lossy projection; the exact Aria code rides in the
/// status details as a `google.rpc.Status` holding one packed `RpcError`.
pub(crate) fn failure_status(mut failure: RpcFailure) -> Status {
    bound_message(&mut failure.message);
    let code = grpc_code(failure.code);
    let error = wire::RpcError::from(failure.clone());
    let details = tonic_types::pb::Status {
        code: i32::from(code),
        message: failure.message.clone(),
        details: vec![prost_types::Any {
            type_url: RPC_ERROR_TYPE_URL.to_owned(),
            value: error.encode_to_vec(),
        }],
    };
    Status::with_details(code, failure.message, details.encode_to_vec().into())
}

fn wire_time(since_epoch: Duration) -> prost_types::Timestamp {
    (UNIX_EPOCH + since_epoch).into()
}

/// How long after the Unix epoch a wire time is. A time before it is refused.
fn since_epoch(
    time: Option<prost_types::Timestamp>,
    field: &'static str,
) -> Result<Duration, ProtocolError> {
    let time = time.ok_or(ProtocolError::MissingBody(field))?;
    SystemTime::try_from(time)
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .ok_or(ProtocolError::OutOfRange(field))
}

fn wire_duration(duration: Duration) -> prost_types::Duration {
    // Only a span past `i64::MAX` seconds fails, and nothing sent is that long.
    prost_types::Duration::try_from(duration).unwrap_or(prost_types::Duration {
        seconds: i64::MAX,
        nanos: 999_999_999,
    })
}

/// A wire duration, refusing a negative one.
fn duration(
    duration: prost_types::Duration,
    field: &'static str,
) -> Result<Duration, ProtocolError> {
    Duration::try_from(duration).map_err(|_| ProtocolError::OutOfRange(field))
}

fn millis(duration: Duration, field: &'static str) -> Result<u64, ProtocolError> {
    u64::try_from(duration.as_millis()).map_err(|_| ProtocolError::OutOfRange(field))
}

fn bound_message(message: &mut String) {
    if message.len() <= MAX_FAILURE_MESSAGE_BYTES {
        return;
    }
    let ellipsis = "…";
    let mut end = MAX_FAILURE_MESSAGE_BYTES - ellipsis.len();
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message.push_str(ellipsis);
}

impl From<RpcFailure> for wire::RpcError {
    fn from(failure: RpcFailure) -> Self {
        Self {
            code: error_code_to_wire(failure.code),
            message: failure.message,
            settings_position: failure.settings_position.map(wire::SettingsPosition::from),
        }
    }
}

impl TryFrom<wire::RpcError> for RpcFailure {
    type Error = ProtocolError;

    fn try_from(error: wire::RpcError) -> Result<Self, Self::Error> {
        Ok(Self::new(error_code_from_wire(error.code)?, error.message)
            .with_settings_position(error.settings_position.map(Position::from)))
    }
}

fn grpc_code(code: RpcErrorCode) -> tonic::Code {
    match code {
        RpcErrorCode::InvalidRequest => tonic::Code::InvalidArgument,
        RpcErrorCode::Busy | RpcErrorCode::PayloadTooLarge => tonic::Code::ResourceExhausted,
        RpcErrorCode::Cancelled => tonic::Code::Cancelled,
        RpcErrorCode::Unauthenticated => tonic::Code::Unauthenticated,
        RpcErrorCode::Unsupported => tonic::Code::Unimplemented,
        RpcErrorCode::ShuttingDown | RpcErrorCode::HistoryUnavailable => tonic::Code::Unavailable,
        RpcErrorCode::OverallTimeout
        | RpcErrorCode::RecordingTimeout
        | RpcErrorCode::DeliveryTimeout
        | RpcErrorCode::SpeechTimeout
        | RpcErrorCode::AssistantTimeout => tonic::Code::DeadlineExceeded,
        RpcErrorCode::HistoryAudioUnavailable
        | RpcErrorCode::HistoryProjectUnavailable
        | RpcErrorCode::HistoryEntryEmpty
        | RpcErrorCode::HistoryTranscriptUnreadable
        | RpcErrorCode::NoSpeech
        | RpcErrorCode::SpeakerNoSpeech
        | RpcErrorCode::SpeakerModelMissing
        | RpcErrorCode::SpeakerInconsistentSamples
        | RpcErrorCode::SystemTranscriptionUnsupportedOs
        | RpcErrorCode::AssistantUnavailable
        | RpcErrorCode::AssistantSignedOut
        | RpcErrorCode::PromptUnreadable
        | RpcErrorCode::PromptsFull
        | RpcErrorCode::ApplicationExcluded
        | RpcErrorCode::SettingsInvalid => tonic::Code::FailedPrecondition,
        RpcErrorCode::SpeakerAlreadyEnrolled | RpcErrorCode::ChatNameTaken => {
            tonic::Code::AlreadyExists
        }
        RpcErrorCode::HistoryMissing
        | RpcErrorCode::PromptMissing
        | RpcErrorCode::SpeakerNotEnrolled
        | RpcErrorCode::VoiceUnavailable => tonic::Code::NotFound,
        RpcErrorCode::SystemTranscriptionPermissionDenied | RpcErrorCode::PermissionDenied => {
            tonic::Code::PermissionDenied
        }
        RpcErrorCode::DeliveryTargetChanged | RpcErrorCode::DeliveryInterrupted => {
            tonic::Code::Aborted
        }
        RpcErrorCode::Internal
        | RpcErrorCode::CaptureFailed
        | RpcErrorCode::TranscriptionFailed
        | RpcErrorCode::DeliveryFailed
        | RpcErrorCode::SynthesisFailed
        | RpcErrorCode::PlaybackFailed
        | RpcErrorCode::AssistantFailed
        | RpcErrorCode::SystemTranscriptionCaptureFailed
        | RpcErrorCode::SpeakerProfileWriteFailed
        | RpcErrorCode::PromptWriteFailed
        | RpcErrorCode::DurabilityUnconfirmed => tonic::Code::Internal,
        RpcErrorCode::Unknown(_) => tonic::Code::Unknown,
    }
}

/// What a status without an Aria failure tells a consumer: the transport
/// gave out, or the daemon does not know the method. Everything else is
/// `Other`, so no consumer learns a gRPC code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportFailure {
    Unavailable,
    Unimplemented,
    DeadlineExceeded,
    Cancelled,
    Other,
}

/// The inverse of `grpc_code` for the categories a consumer may act on.
pub(crate) fn transport_failure(code: tonic::Code) -> TransportFailure {
    match code {
        tonic::Code::Unavailable => TransportFailure::Unavailable,
        tonic::Code::Unimplemented => TransportFailure::Unimplemented,
        tonic::Code::DeadlineExceeded => TransportFailure::DeadlineExceeded,
        tonic::Code::Cancelled => TransportFailure::Cancelled,
        tonic::Code::Ok
        | tonic::Code::Unknown
        | tonic::Code::InvalidArgument
        | tonic::Code::NotFound
        | tonic::Code::AlreadyExists
        | tonic::Code::PermissionDenied
        | tonic::Code::ResourceExhausted
        | tonic::Code::FailedPrecondition
        | tonic::Code::Aborted
        | tonic::Code::OutOfRange
        | tonic::Code::Internal
        | tonic::Code::DataLoss
        | tonic::Code::Unauthenticated => TransportFailure::Other,
    }
}

/// The Aria failure a status carries, when its details hold one.
pub(crate) fn rpc_failure(status: &Status) -> Option<RpcFailure> {
    let details = status.details();
    if details.is_empty() {
        return None;
    }
    let details = tonic_types::pb::Status::decode(details).ok()?;
    let packed = details
        .details
        .iter()
        .find(|any| any.type_url == RPC_ERROR_TYPE_URL)?;
    let error = wire::RpcError::decode(packed.value.as_slice()).ok()?;
    error.try_into().ok()
}

impl From<DictationOutcome> for wire::StartDictationResponse {
    fn from(outcome: DictationOutcome) -> Self {
        let body = match outcome {
            DictationOutcome::Completed(result) => {
                wire::start_dictation_response::Body::Result(wire::DictationResult {
                    text: result.text,
                    own_voice: result.own_voice,
                })
            }
            DictationOutcome::StopAccepted => {
                wire::start_dictation_response::Body::StopAccepted(wire::DictationStopAccepted {})
            }
        };
        Self { body: Some(body) }
    }
}

impl From<DictationStage> for wire::StartDictationResponse {
    fn from(stage: DictationStage) -> Self {
        Self {
            body: Some(wire::start_dictation_response::Body::Progress(stage.into())),
        }
    }
}

impl From<UpdateResult> for wire::UpdateSettingsResponse {
    fn from(update: UpdateResult) -> Self {
        let (result, restart_required) = match update {
            UpdateResult::Updated {
                snapshot,
                restart_required,
            } => (
                wire::update_settings_response::Result::Snapshot((&snapshot).into()),
                restart_required,
            ),
            UpdateResult::Conflict(current) => (
                wire::update_settings_response::Result::Conflict(wire::SettingsConflict {
                    current: Some((&current).into()),
                }),
                Vec::new(),
            ),
        };
        Self {
            result: Some(result),
            restart_required,
        }
    }
}

impl TryFrom<wire::UpdateSettingsResponse> for UpdateResult {
    type Error = ProtocolError;

    fn try_from(response: wire::UpdateSettingsResponse) -> Result<Self, Self::Error> {
        match response.result {
            Some(wire::update_settings_response::Result::Snapshot(snapshot)) => Ok(Self::Updated {
                snapshot: snapshot.try_into()?,
                restart_required: response.restart_required,
            }),
            Some(wire::update_settings_response::Result::Conflict(conflict)) => {
                let current = conflict
                    .current
                    .ok_or(ProtocolError::MissingBody("settings conflict snapshot"))?;
                Ok(Self::Conflict(current.try_into()?))
            }
            None => Err(ProtocolError::MissingBody("settings update response")),
        }
    }
}

impl From<Position> for wire::SettingsPosition {
    fn from(position: Position) -> Self {
        Self {
            line: u32::try_from(position.line).unwrap_or(u32::MAX),
            column: u32::try_from(position.column).unwrap_or(u32::MAX),
        }
    }
}

impl From<wire::SettingsPosition> for Position {
    fn from(position: wire::SettingsPosition) -> Self {
        Self {
            line: usize::try_from(position.line).unwrap_or(usize::MAX),
            column: usize::try_from(position.column).unwrap_or(usize::MAX),
        }
    }
}

impl From<&SettingsStatus> for wire::GetSettingsStatusResponse {
    fn from(status: &SettingsStatus) -> Self {
        Self {
            status: Some(status.into()),
        }
    }
}

impl TryFrom<wire::GetSettingsStatusResponse> for SettingsStatus {
    type Error = ProtocolError;

    fn try_from(response: wire::GetSettingsStatusResponse) -> Result<Self, Self::Error> {
        response
            .status
            .map(Self::from)
            .ok_or(ProtocolError::MissingBody("settings status"))
    }
}

impl From<ClearHistoryResult> for wire::ClearHistoryResponse {
    fn from(result: ClearHistoryResult) -> Self {
        Self {
            deleted_entry_count: result.deleted_entries,
            audio_cleanup_pending: result.cleanup_pending,
        }
    }
}

impl From<Option<aria_voice::history::HistorySection>> for wire::ClearHistoryRequest {
    fn from(section: Option<aria_voice::history::HistorySection>) -> Self {
        Self {
            section: section.map(|section| wire::HistorySection::from(section) as i32),
        }
    }
}

impl From<wire::ClearHistoryResponse> for ClearHistoryResult {
    fn from(response: wire::ClearHistoryResponse) -> Self {
        Self {
            deleted_entries: response.deleted_entry_count,
            cleanup_pending: response.audio_cleanup_pending,
        }
    }
}

impl From<wire::DictationResult> for DictationResult {
    fn from(result: wire::DictationResult) -> Self {
        Self {
            text: result.text,
            own_voice: result.own_voice,
        }
    }
}

impl From<RelayStage> for wire::RelayEvent {
    fn from(outcome: RelayStage) -> Self {
        let (stage, text) = match outcome {
            RelayStage::Transcribing => (wire::RelayStage::Transcribing, String::new()),
            RelayStage::Completed(text) => (wire::RelayStage::Completed, text),
            RelayStage::NoSpeech => (wire::RelayStage::NoSpeech, String::new()),
        };
        Self {
            stage: stage as i32,
            text,
        }
    }
}

impl TryFrom<Invite> for wire::IssueInviteResponse {
    type Error = ProtocolError;

    fn try_from(invite: Invite) -> Result<Self, Self::Error> {
        Ok(Self {
            instance_name: invite.instance,
            code: invite.code.raw().to_owned(),
            fingerprint: invite.fingerprint.to_vec(),
            expires_in: Some(wire_duration(invite.expires_in)),
            pairing_count: invite.pairing_count,
        })
    }
}

impl From<&SettingsStatus> for wire::SettingsStatus {
    fn from(status: &SettingsStatus) -> Self {
        Self {
            fault: status.fault.as_ref().map(|fault| wire::SettingsFault {
                message: fault.message.clone(),
                position: fault.position.map(Into::into),
            }),
            restart_required: status.restart_required.clone(),
            reverted: status.reverted.clone(),
            revision: status.revision,
        }
    }
}

impl From<wire::SettingsStatus> for SettingsStatus {
    fn from(status: wire::SettingsStatus) -> Self {
        Self {
            fault: status.fault.map(|fault| SettingsFault {
                message: fault.message,
                position: fault.position.map(Into::into),
            }),
            restart_required: status.restart_required,
            reverted: status.reverted,
            revision: status.revision,
        }
    }
}

impl From<&SettingsStatus> for wire::Event {
    fn from(status: &SettingsStatus) -> Self {
        Self {
            payload: Some(wire::event::Payload::Settings(status.into())),
        }
    }
}

impl From<Combo> for wire::Event {
    fn from(combo: Combo) -> Self {
        Self {
            payload: Some(wire::event::Payload::Hotkey(wire::HotkeyEvent {
                combo: wire::ShortcutCombo::from(combo).into(),
            })),
        }
    }
}

impl From<Combo> for wire::ShortcutCombo {
    fn from(combo: Combo) -> Self {
        match combo {
            Combo::Shift => Self::Shift,
            Combo::Command => Self::Command,
        }
    }
}

impl WireEnum for wire::ShortcutCombo {
    type Domain = Combo;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::Shift => Some(Combo::Shift),
            Self::Command => Some(Combo::Command),
            Self::Unspecified => None,
        }
    }
}

impl From<DictationStage> for wire::Event {
    fn from(stage: DictationStage) -> Self {
        Self {
            payload: Some(wire::event::Payload::Dictation(stage.into())),
        }
    }
}

impl From<DictationEvent> for wire::Event {
    fn from(event: DictationEvent) -> Self {
        match event {
            DictationEvent::Stage(stage) => stage.into(),
            DictationEvent::Caption(caption) => Self {
                payload: Some(wire::event::Payload::Caption(caption.into())),
            },
            DictationEvent::Ghost(ghost) => Self {
                payload: Some(wire::event::Payload::Ghost(ghost.into())),
            },
            DictationEvent::Level(level) => Self {
                payload: Some(wire::event::Payload::Level(level.into())),
            },
        }
    }
}

impl From<CaptionSnapshot> for wire::CaptionSnapshot {
    fn from(caption: CaptionSnapshot) -> Self {
        Self {
            recording_id: caption.recording.0,
            finalized: caption.finalized.to_string(),
            provisional: caption.provisional.to_string(),
            ended: caption.ended,
        }
    }
}

impl From<GhostSnapshot> for wire::GhostSnapshot {
    fn from(ghost: GhostSnapshot) -> Self {
        Self {
            recording_id: ghost.recording.0,
            text: ghost.text.to_string(),
            placement: ghost.placement.map(Into::into),
        }
    }
}

impl From<Placement> for wire::Placement {
    fn from(placement: Placement) -> Self {
        let (font_name, font_size) = placement
            .font
            .map_or((String::new(), 0.0), |font| (font.name, font.size));
        Self {
            caret: Some(placement.caret.into()),
            field: placement.field.map(Into::into),
            font_name,
            font_size,
        }
    }
}

impl From<Rect> for wire::Rect {
    fn from(rect: Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

impl From<MicLevel> for wire::MicLevel {
    fn from(level: MicLevel) -> Self {
        Self {
            recording_id: level.recording.0,
            level: level.level,
        }
    }
}

impl From<SpeechStage> for wire::Event {
    fn from(stage: SpeechStage) -> Self {
        Self {
            payload: Some(wire::event::Payload::Speech(stage.into())),
        }
    }
}

impl From<ChatStage> for wire::Event {
    fn from(stage: ChatStage) -> Self {
        Self {
            payload: Some(wire::event::Payload::Chat(stage.into())),
        }
    }
}

impl From<ChatStage> for wire::ChatEvent {
    fn from(stage: ChatStage) -> Self {
        let ticket = stage.ticket.0;
        let assistant_id = wire::AssistantId::from(stage.assistant_id) as i32;
        match stage.kind {
            ChatStageKind::Working(step) => {
                let skill = match &step {
                    Step::Skill(name) => name.clone(),
                    _ => String::new(),
                };
                Self {
                    stage: wire::ChatStage::Working as i32,
                    error_code: 0,
                    ticket,
                    text: String::new(),
                    error_message: String::new(),
                    assistant_id,
                    step: step_to_wire(step),
                    skill,
                    copied: false,
                    history_entry_id: None,
                }
            }
            ChatStageKind::Replied(ChatReply {
                text,
                delivery_error,
                copied,
                history_entry_id,
            }) => Self {
                stage: wire::ChatStage::Replied as i32,
                error_code: delivery_error.map_or(0, error_code_to_wire),
                ticket,
                text,
                error_message: String::new(),
                assistant_id,
                step: 0,
                skill: String::new(),
                copied,
                history_entry_id,
            },
            ChatStageKind::Failed { code, message } => Self {
                stage: wire::ChatStage::Failed as i32,
                error_code: error_code_to_wire(code),
                ticket,
                text: String::new(),
                error_message: message,
                assistant_id,
                step: 0,
                skill: String::new(),
                copied: false,
                history_entry_id: None,
            },
        }
    }
}

impl TryFrom<&wire::ChatEvent> for ChatStage {
    type Error = ProtocolError;

    fn try_from(event: &wire::ChatEvent) -> Result<Self, Self::Error> {
        let kind = match wire::ChatStage::try_from(event.stage) {
            Ok(wire::ChatStage::Working) => {
                ChatStageKind::Working(step_from_wire(event.step, &event.skill)?)
            }
            Ok(wire::ChatStage::Replied) => ChatStageKind::Replied(ChatReply {
                text: event.text.clone(),
                delivery_error: match event.error_code {
                    0 => None,
                    code => Some(error_code_from_wire(code)?),
                },
                copied: event.copied,
                history_entry_id: event.history_entry_id.map(history_id).transpose()?,
            }),
            Ok(wire::ChatStage::Failed) => ChatStageKind::Failed {
                code: error_code_from_wire(event.error_code)?,
                message: event.error_message.clone(),
            },
            Ok(wire::ChatStage::Unspecified) | Err(..) => {
                return Err(ProtocolError::InvalidEnum {
                    field: "stage",
                    value: event.stage,
                });
            }
        };
        Ok(Self {
            ticket: Ticket(event.ticket),
            assistant_id: decode_enum::<wire::AssistantId>(event.assistant_id, "assistant_id")?,
            kind,
        })
    }
}

impl From<AssistantId> for wire::AssistantId {
    fn from(assistant_id: AssistantId) -> Self {
        match assistant_id {
            AssistantId::ClaudeCode => Self::ClaudeCode,
            AssistantId::Codex => Self::Codex,
            AssistantId::LocalModel => Self::LocalModel,
        }
    }
}

fn step_to_wire(step: Step) -> i32 {
    let code = match step {
        Step::Thinking => wire::ChatStep::Thinking,
        Step::Reading => wire::ChatStep::Reading,
        Step::Searching => wire::ChatStep::Searching,
        Step::Running => wire::ChatStep::Running,
        Step::Skill(..) => wire::ChatStep::Skill,
        Step::Unknown(code) => return code,
    };
    code as i32
}

/// A code this build does not know is kept, as `RpcErrorCode` keeps
/// its own, so a newer daemon's step still shows as one. Unspecified
/// is a daemon too old to send steps at all.
fn step_from_wire(code: i32, skill: &str) -> Result<Step, ProtocolError> {
    Ok(match wire::ChatStep::try_from(code) {
        Ok(wire::ChatStep::Thinking) => Step::Thinking,
        Ok(wire::ChatStep::Reading) => Step::Reading,
        Ok(wire::ChatStep::Searching) => Step::Searching,
        Ok(wire::ChatStep::Running) => Step::Running,
        Ok(wire::ChatStep::Skill) => Step::skill(skill),
        Ok(wire::ChatStep::Unspecified) => {
            return Err(ProtocolError::InvalidEnum {
                field: "step",
                value: code,
            });
        }
        Err(..) => Step::Unknown(code),
    })
}

impl WireEnum for wire::AssistantId {
    type Domain = AssistantId;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::ClaudeCode => Some(AssistantId::ClaudeCode),
            Self::Codex => Some(AssistantId::Codex),
            Self::LocalModel => Some(AssistantId::LocalModel),
            Self::Unspecified => None,
        }
    }
}

impl From<AskCall> for wire::AskRequest {
    fn from(call: AskCall) -> Self {
        use wire::ask_request::Chat;
        let path = |workdir: std::path::PathBuf| workdir.to_string_lossy().into_owned();
        let chat = match call.chat {
            ChatChoice::Folder {
                workdir,
                new_session,
            } => Chat::Folder(wire::FolderChat {
                workdir: path(workdir),
                new_session,
            }),
            ChatChoice::Named {
                name,
                workdir,
                new_session,
            } => Chat::Named(wire::NamedChat {
                name: name.as_str().to_owned(),
                workdir: workdir.map(path).unwrap_or_default(),
                new_session,
            }),
            ChatChoice::Start { workdir } => Chat::Start(wire::StartChat {
                workdir: path(workdir),
            }),
        };
        Self {
            assistant_id: wire::AssistantId::from(call.assistant_id) as i32,
            model_id: call.model_id.unwrap_or_default(),
            chat: Some(chat),
            text: call.text,
            own_voice: call.own_voice,
            output_mode: call
                .output_mode
                .map(|mode| wire::OutputMode::from(mode) as i32),
            tap_cancels: call.tap_cancels,
        }
    }
}

impl TryFrom<wire::AskRequest> for AskCall {
    type Error = ProtocolError;

    fn try_from(request: wire::AskRequest) -> Result<Self, Self::Error> {
        if request.text.trim().is_empty() {
            return Err(ProtocolError::EmptyPrompt);
        }
        let output_mode = request
            .output_mode
            .map(|value| decode_enum::<wire::OutputMode>(value, "output mode"))
            .transpose()?;
        let assistant_id = decode_enum::<wire::AssistantId>(request.assistant_id, "assistant_id")?;
        if !aria_settings::is_valid_model_id(&request.model_id) {
            return Err(ProtocolError::InvalidModelId);
        }
        let chat = chat_choice(request.chat.ok_or(ProtocolError::MissingBody("chat"))?)?;
        Ok(Self {
            text: request.text,
            chat,
            assistant_id,
            model_id: Some(request.model_id).filter(|model_id| !model_id.is_empty()),
            output_mode,
            own_voice: request.own_voice,
            tap_cancels: request.tap_cancels,
        })
    }
}

/// Empty is proto3's absent. A relative path would name a folder beside the
/// daemon, not the caller, and a folder's own chat goes by its path,
/// which only an absolute one keeps apart from chat names.
fn ask_workdir(workdir: String) -> Result<Option<std::path::PathBuf>, ProtocolError> {
    if workdir.is_empty() {
        return Ok(None);
    }
    let workdir = std::path::PathBuf::from(workdir);
    if !workdir.is_absolute() {
        return Err(ProtocolError::RelativeWorkdir);
    }
    Ok(Some(workdir))
}

fn required_ask_workdir(workdir: String) -> Result<std::path::PathBuf, ProtocolError> {
    ask_workdir(workdir)?.ok_or(ProtocolError::MissingBody("assistant working directory"))
}

fn chat_choice(chat: wire::ask_request::Chat) -> Result<ChatChoice, ProtocolError> {
    use wire::ask_request::Chat;
    Ok(match chat {
        Chat::Folder(folder) => ChatChoice::Folder {
            workdir: required_ask_workdir(folder.workdir)?,
            new_session: folder.new_session,
        },
        Chat::Named(named) => ChatChoice::Named {
            name: ChatName::new(&named.name)?,
            workdir: ask_workdir(named.workdir)?,
            new_session: named.new_session,
        },
        Chat::Start(start) => ChatChoice::Start {
            workdir: required_ask_workdir(start.workdir)?,
        },
    })
}

impl From<PromptCall> for wire::RunPromptRequest {
    fn from(call: PromptCall) -> Self {
        Self {
            input: Some(match call.input {
                PromptInput::Text(text) => wire::run_prompt_request::Input::Text(text),
                PromptInput::Entry(id) => wire::run_prompt_request::Input::HistoryEntryId(id),
            }),
            prompt: Some(match call.prompt {
                PromptChoice::Stored(id) => {
                    wire::run_prompt_request::Prompt::PromptId(id.as_str().to_owned())
                }
                PromptChoice::OneOff(instructions) => {
                    wire::run_prompt_request::Prompt::Instructions(instructions.as_str().to_owned())
                }
            }),
            application_id: call
                .application
                .map(|application| application.as_str().to_owned())
                .unwrap_or_default(),
            tap_cancels: call.tap_cancels,
            output_mode: call
                .output_mode
                .map(|mode| wire::OutputMode::from(mode) as i32),
        }
    }
}

impl TryFrom<wire::RunPromptRequest> for PromptCall {
    type Error = ProtocolError;

    fn try_from(request: wire::RunPromptRequest) -> Result<Self, Self::Error> {
        let input = match request.input {
            Some(wire::run_prompt_request::Input::Text(text)) if !text.trim().is_empty() => {
                PromptInput::Text(text)
            }
            Some(wire::run_prompt_request::Input::HistoryEntryId(id)) => {
                PromptInput::Entry(history_id(id)?)
            }
            Some(wire::run_prompt_request::Input::Text(_)) | None => {
                return Err(ProtocolError::EmptyPrompt);
            }
        };
        let application = Some(request.application_id)
            .filter(|id| !id.is_empty())
            .map(ApplicationId::new)
            .transpose()?;
        let output_mode = request
            .output_mode
            .map(|value| decode_enum::<wire::OutputMode>(value, "output mode"))
            .transpose()?;
        // A named app only says where to type, so it needs an output mode.
        if application.is_some() && output_mode.is_none() {
            return Err(ProtocolError::MissingBody("output mode for the named app"));
        }
        let prompt = match request.prompt {
            Some(wire::run_prompt_request::Prompt::PromptId(id)) => {
                PromptChoice::Stored(PromptId::new(id)?)
            }
            Some(wire::run_prompt_request::Prompt::Instructions(text)) => {
                PromptChoice::OneOff(Instructions::new(text)?)
            }
            None => return Err(ProtocolError::MissingBody("prompt")),
        };
        Ok(Self {
            input,
            prompt,
            application,
            tap_cancels: request.tap_cancels,
            output_mode,
        })
    }
}

impl TryFrom<wire::SavePromptRequest> for PromptDraft {
    type Error = ProtocolError;

    fn try_from(request: wire::SavePromptRequest) -> Result<Self, Self::Error> {
        if request
            .name
            .as_ref()
            .is_some_and(|name| name.trim().is_empty())
        {
            return Err(PromptError::Name.into());
        }
        let file = request.file.map(prompt_file).transpose()?;
        let id = request.id.map(PromptId::new).transpose()?;
        let kind = decode_enum::<wire::PromptKind>(request.kind, "prompt kind")?;
        let instructions = request.instructions.map(Instructions::new).transpose()?;
        let name = request.name;
        if let Some(id) = id {
            return Ok(Self::Edit {
                id,
                name,
                kind: Some(kind),
                file,
                instructions,
            });
        }
        let source = match file {
            Some(file) => NewSource::Chosen {
                file,
                name,
                instructions,
            },
            None => NewSource::Written {
                name: name.ok_or(ProtocolError::MissingBody(
                    "name, or a file to name it after",
                ))?,
                instructions: instructions.ok_or(ProtocolError::MissingBody("instructions"))?,
            },
        };
        Ok(Self::New { kind, source })
    }
}

/// A relative path would name a file beside the daemon, not the caller.
fn prompt_file(file: String) -> Result<std::path::PathBuf, PromptError> {
    let path = std::path::PathBuf::from(&file);
    if path.is_absolute() && !file.contains('\0') {
        Ok(path)
    } else {
        Err(PromptError::File)
    }
}

impl TryFrom<wire::GetPromptRequest> for PromptSource {
    type Error = ProtocolError;

    fn try_from(request: wire::GetPromptRequest) -> Result<Self, Self::Error> {
        match request.source {
            Some(wire::get_prompt_request::Source::PromptId(id)) => {
                Ok(Self::Stored(PromptId::new(id)?))
            }
            Some(wire::get_prompt_request::Source::File(file)) => {
                Ok(Self::File(prompt_file(file)?))
            }
            None => Err(ProtocolError::MissingBody("prompt source")),
        }
    }
}

impl From<PromptContents> for wire::GetPromptResponse {
    fn from(contents: PromptContents) -> Self {
        let contents = match contents {
            PromptContents::Instructions(text) => {
                wire::get_prompt_response::Contents::Instructions(text)
            }
            PromptContents::Missing => wire::get_prompt_response::Contents::Missing(true),
        };
        Self {
            contents: Some(contents),
        }
    }
}

impl TryFrom<wire::RemovePromptRequest> for PromptId {
    type Error = ProtocolError;

    fn try_from(request: wire::RemovePromptRequest) -> Result<Self, Self::Error> {
        Ok(Self::new(request.id)?)
    }
}

impl From<&Prompt> for wire::RemovePromptResponse {
    fn from(prompt: &Prompt) -> Self {
        Self {
            file: prompt.file().to_string_lossy().into_owned(),
        }
    }
}

impl From<&Prompt> for wire::SavePromptResponse {
    fn from(prompt: &Prompt) -> Self {
        Self {
            id: prompt.id().as_str().to_owned(),
            file: prompt.file().to_string_lossy().into_owned(),
        }
    }
}

impl TryFrom<wire::DeliverRequest> for DeliverCall {
    type Error = ProtocolError;

    fn try_from(request: wire::DeliverRequest) -> Result<Self, Self::Error> {
        if request.text.trim().is_empty() {
            return Err(ProtocolError::EmptyPrompt);
        }
        if request.text.len() > aria_platform::delivery::MAX_TEXT_BYTES {
            return Err(ProtocolError::OutOfRange("text"));
        }
        Ok(Self {
            text: request.text,
            application: Some(request.application_id)
                .filter(|id| !id.is_empty())
                .map(ApplicationId::new)
                .transpose()?,
            output_mode: decode_enum::<wire::OutputMode>(request.output_mode, "output mode")?,
        })
    }
}

impl From<aria_platform::delivery::DeliveryOutcome> for wire::DeliverResponse {
    fn from(outcome: aria_platform::delivery::DeliveryOutcome) -> Self {
        let outcome = match outcome {
            aria_platform::delivery::DeliveryOutcome::Inserted => wire::DeliveryOutcome::Inserted,
            aria_platform::delivery::DeliveryOutcome::Copied => wire::DeliveryOutcome::Copied,
        };
        Self {
            outcome: outcome as i32,
        }
    }
}

impl From<Ticket> for wire::AskResponse {
    fn from(ticket: Ticket) -> Self {
        (ticket, None).into()
    }
}

impl From<(Ticket, Option<ChatKey>)> for wire::AskResponse {
    fn from((ticket, chat): (Ticket, Option<ChatKey>)) -> Self {
        Self {
            ticket: ticket.0,
            chat: chat.map(Into::into),
        }
    }
}

impl TryFrom<wire::AskResponse> for (Ticket, Option<ChatKey>) {
    type Error = ProtocolError;

    fn try_from(response: wire::AskResponse) -> Result<Self, Self::Error> {
        let chat = response.chat.map(TryInto::try_into).transpose()?;
        Ok((Ticket(response.ticket), chat))
    }
}

impl From<wire::AskResponse> for Ticket {
    fn from(response: wire::AskResponse) -> Self {
        Self(response.ticket)
    }
}

impl TryFrom<wire::ListPendingAsksRequest> for ChatKey {
    type Error = ProtocolError;

    fn try_from(request: wire::ListPendingAsksRequest) -> Result<Self, Self::Error> {
        required_chat(request.chat)
    }
}

impl TryFrom<wire::CancelChatRequest> for ChatKey {
    type Error = ProtocolError;

    fn try_from(request: wire::CancelChatRequest) -> Result<Self, Self::Error> {
        required_chat(request.chat)
    }
}

impl From<Vec<PendingAsk>> for wire::ListPendingAsksResponse {
    fn from(asks: Vec<PendingAsk>) -> Self {
        Self {
            asks: asks
                .into_iter()
                .map(|ask| wire::PendingAsk {
                    ticket: ask.ticket.0,
                    text: ask.text,
                    running: ask.running,
                })
                .collect(),
        }
    }
}

impl From<Ticket> for wire::CancelAskRequest {
    fn from(ticket: Ticket) -> Self {
        Self { ticket: ticket.0 }
    }
}

impl From<wire::CancelAskRequest> for Ticket {
    fn from(request: wire::CancelAskRequest) -> Self {
        Self(request.ticket)
    }
}

impl From<EnrollmentStage> for wire::Event {
    fn from(stage: EnrollmentStage) -> Self {
        Self {
            payload: Some(wire::event::Payload::Speaker(stage.into())),
        }
    }
}

impl From<EnrollmentStage> for wire::SpeakerEvent {
    fn from(stage: EnrollmentStage) -> Self {
        let counted = |code: wire::SpeakerStage, index: u32, samples: u32| Self {
            stage: code as i32,
            index,
            samples,
            error_code: 0,
        };
        match stage {
            EnrollmentStage::Recording { index, of } => {
                counted(wire::SpeakerStage::Recording, index, of)
            }
            EnrollmentStage::SampleTaken { index, of } => {
                counted(wire::SpeakerStage::SampleTaken, index, of)
            }
            EnrollmentStage::Enrolled => counted(wire::SpeakerStage::Enrolled, 0, 0),
            EnrollmentStage::Cancelled => counted(wire::SpeakerStage::Cancelled, 0, 0),
            EnrollmentStage::Failed(code) => Self {
                stage: wire::SpeakerStage::Failed as i32,
                index: 0,
                samples: 0,
                error_code: error_code_to_wire(code),
            },
        }
    }
}

impl TryFrom<&wire::SpeakerEvent> for EnrollmentStage {
    type Error = ProtocolError;

    fn try_from(event: &wire::SpeakerEvent) -> Result<Self, Self::Error> {
        match wire::SpeakerStage::try_from(event.stage) {
            Ok(wire::SpeakerStage::Recording) => Ok(Self::Recording {
                index: event.index,
                of: event.samples,
            }),
            Ok(wire::SpeakerStage::SampleTaken) => Ok(Self::SampleTaken {
                index: event.index,
                of: event.samples,
            }),
            Ok(wire::SpeakerStage::Enrolled) => Ok(Self::Enrolled),
            Ok(wire::SpeakerStage::Cancelled) => Ok(Self::Cancelled),
            Ok(wire::SpeakerStage::Failed) => {
                error_code_from_wire(event.error_code).map(Self::Failed)
            }
            Ok(wire::SpeakerStage::Unspecified) | Err(..) => Err(ProtocolError::InvalidEnum {
                field: "stage",
                value: event.stage,
            }),
        }
    }
}

impl From<Role> for wire::SpeakerRole {
    fn from(role: Role) -> Self {
        match role {
            Role::Owner => Self::Owner,
            Role::Guest => Self::Guest,
        }
    }
}

impl From<EnrollmentResult> for wire::EnrollSpeakerResponse {
    fn from(result: EnrollmentResult) -> Self {
        Self {
            role: wire::SpeakerRole::from(result.role) as i32,
            consistency: result.consistency,
        }
    }
}

/// The result of enrolling `name`; the response carries the rest.
pub(crate) fn enrollment_result(
    name: SpeakerName,
    response: wire::EnrollSpeakerResponse,
) -> Result<EnrollmentResult, ProtocolError> {
    Ok(EnrollmentResult {
        name,
        role: role(response.role)?,
        consistency: response.consistency,
    })
}

impl From<Vec<EnrolledSpeaker>> for wire::ListSpeakersResponse {
    fn from(speakers: Vec<EnrolledSpeaker>) -> Self {
        Self {
            speakers: speakers.iter().map(wire::EnrolledSpeaker::from).collect(),
        }
    }
}

impl TryFrom<wire::ListSpeakersResponse> for Vec<EnrolledSpeaker> {
    type Error = ProtocolError;

    fn try_from(response: wire::ListSpeakersResponse) -> Result<Self, Self::Error> {
        response
            .speakers
            .iter()
            .map(EnrolledSpeaker::try_from)
            .collect()
    }
}

impl From<&EnrolledSpeaker> for wire::EnrolledSpeaker {
    fn from(speaker: &EnrolledSpeaker) -> Self {
        Self {
            name: speaker.name.to_string(),
            role: wire::SpeakerRole::from(speaker.role) as i32,
            enroll_time: Some(wire_time(Duration::from_secs(speaker.enrolled_at))),
        }
    }
}

impl TryFrom<&wire::EnrolledSpeaker> for EnrolledSpeaker {
    type Error = ProtocolError;

    fn try_from(speaker: &wire::EnrolledSpeaker) -> Result<Self, Self::Error> {
        Ok(Self {
            name: speaker_name(&speaker.name)?,
            role: role(speaker.role)?,
            enrolled_at: since_epoch(speaker.enroll_time, "enroll_time")?.as_secs(),
        })
    }
}

impl From<EnrollmentRequest> for wire::EnrollSpeakerRequest {
    fn from(request: EnrollmentRequest) -> Self {
        Self {
            name: request.name.to_string(),
            role: request
                .role
                .map_or(0, |role| wire::SpeakerRole::from(role) as i32),
            samples: request.samples,
            replace: request.replace,
        }
    }
}

impl TryFrom<wire::EnrollSpeakerRequest> for EnrollmentRequest {
    type Error = ProtocolError;

    fn try_from(request: wire::EnrollSpeakerRequest) -> Result<Self, Self::Error> {
        Ok(Self {
            name: speaker_name(&request.name)?,
            role: decode_role(request.role)?,
            samples: request.samples,
            replace: request.replace,
        })
    }
}

/// The role a daemon answered with. Unspecified is a request, never an
/// answer.
pub(crate) fn role(role: i32) -> Result<Role, ProtocolError> {
    decode_role(role)?.ok_or(ProtocolError::InvalidEnum {
        field: "role",
        value: role,
    })
}

/// `None` for the unspecified role, which is how a client says "whichever
/// role fits".
fn decode_role(role: i32) -> Result<Option<Role>, ProtocolError> {
    match wire::SpeakerRole::try_from(role) {
        Ok(wire::SpeakerRole::Unspecified) => Ok(None),
        Ok(wire::SpeakerRole::Owner) => Ok(Some(Role::Owner)),
        Ok(wire::SpeakerRole::Guest) => Ok(Some(Role::Guest)),
        Err(_unknown) => Err(ProtocolError::InvalidEnum {
            field: "role",
            value: role,
        }),
    }
}

pub(crate) fn speaker_name(name: &str) -> Result<SpeakerName, ProtocolError> {
    SpeakerName::new(name).map_err(|source| ProtocolError::SpeakerName { source })
}

fn wire_speaker_name(name: Option<SpeakerName>) -> String {
    name.map(|name| name.as_str().to_owned())
        .unwrap_or_default()
}

fn optional_speaker_name(name: &str) -> Result<Option<SpeakerName>, ProtocolError> {
    match name {
        "" => Ok(None),
        name => speaker_name(name).map(Some),
    }
}

impl From<SystemTranscriptionStarted> for wire::StartSystemTranscriptionResponse {
    fn from(started: SystemTranscriptionStarted) -> Self {
        Self {
            already_running: started.already_running,
            transcript_path: started.transcript.display().to_string(),
        }
    }
}

impl From<wire::StartSystemTranscriptionResponse> for SystemTranscriptionStarted {
    fn from(response: wire::StartSystemTranscriptionResponse) -> Self {
        Self {
            already_running: response.already_running,
            transcript: response.transcript_path.into(),
        }
    }
}

impl From<SystemTranscriptionStopped> for wire::StopSystemTranscriptionResponse {
    fn from(stopped: SystemTranscriptionStopped) -> Self {
        Self {
            was_running: stopped.was_running,
            transcript_path: stopped.transcript.display().to_string(),
        }
    }
}

impl From<wire::StopSystemTranscriptionResponse> for SystemTranscriptionStopped {
    fn from(response: wire::StopSystemTranscriptionResponse) -> Self {
        Self {
            was_running: response.was_running,
            transcript: response.transcript_path.into(),
        }
    }
}

impl From<SystemTranscriptionSnapshot> for wire::GetSystemTranscriptionStatusResponse {
    fn from(snapshot: SystemTranscriptionSnapshot) -> Self {
        Self {
            running: snapshot.running,
            transcript_path: snapshot.transcript.display().to_string(),
            error_message: snapshot.error.unwrap_or_default(),
        }
    }
}

impl From<wire::GetSystemTranscriptionStatusResponse> for SystemTranscriptionSnapshot {
    fn from(response: wire::GetSystemTranscriptionStatusResponse) -> Self {
        Self {
            running: response.running,
            transcript: response.transcript_path.into(),
            error: Some(response.error_message).filter(|message| !message.is_empty()),
        }
    }
}

impl TryFrom<wire::StartSystemTranscriptionRequest> for Option<std::path::PathBuf> {
    type Error = ProtocolError;

    fn try_from(request: wire::StartSystemTranscriptionRequest) -> Result<Self, Self::Error> {
        request
            .transcript_path
            .map(|path| {
                let output = std::path::PathBuf::from(&path);
                if !output.is_absolute() || path.contains('\0') {
                    return Err(ProtocolError::InvalidTranscriptPath);
                }
                Ok(output)
            })
            .transpose()
    }
}

impl From<SystemTranscriptionStage> for wire::Event {
    fn from(stage: SystemTranscriptionStage) -> Self {
        Self {
            payload: Some(wire::event::Payload::SystemTranscription(stage.into())),
        }
    }
}

impl From<SystemTranscriptionStage> for wire::SystemTranscriptionEvent {
    fn from(stage: SystemTranscriptionStage) -> Self {
        match stage {
            SystemTranscriptionStage::Started => Self {
                stage: wire::SystemTranscriptionStage::Started as i32,
                text: String::new(),
                speaker_label: String::new(),
                merged_from_label: String::new(),
                offset: None,
                error_code: 0,
            },
            SystemTranscriptionStage::SegmentTranscribed {
                text,
                label,
                at_seconds,
            } => Self {
                stage: wire::SystemTranscriptionStage::SegmentTranscribed as i32,
                text: text.to_string(),
                speaker_label: label.map(|label| label.to_string()).unwrap_or_default(),
                merged_from_label: String::new(),
                offset: Some(wire_duration(Duration::from_secs(at_seconds))),
                error_code: 0,
            },
            SystemTranscriptionStage::SpeakerMerged { from, into } => Self {
                stage: wire::SystemTranscriptionStage::SpeakerMerged as i32,
                text: String::new(),
                speaker_label: into.to_string(),
                merged_from_label: from.to_string(),
                offset: None,
                error_code: 0,
            },
            SystemTranscriptionStage::Stopped => Self {
                stage: wire::SystemTranscriptionStage::Stopped as i32,
                text: String::new(),
                speaker_label: String::new(),
                merged_from_label: String::new(),
                offset: None,
                error_code: 0,
            },
            SystemTranscriptionStage::Failed(code) => Self {
                stage: wire::SystemTranscriptionStage::Failed as i32,
                text: String::new(),
                speaker_label: String::new(),
                merged_from_label: String::new(),
                offset: None,
                error_code: error_code_to_wire(code),
            },
        }
    }
}

impl TryFrom<&wire::SystemTranscriptionEvent> for SystemTranscriptionStage {
    type Error = ProtocolError;

    fn try_from(event: &wire::SystemTranscriptionEvent) -> Result<Self, Self::Error> {
        match wire::SystemTranscriptionStage::try_from(event.stage) {
            Ok(wire::SystemTranscriptionStage::Started) => Ok(Self::Started),
            Ok(wire::SystemTranscriptionStage::SegmentTranscribed) => {
                Ok(Self::SegmentTranscribed {
                    text: Arc::from(event.text.as_str()),
                    label: (!event.speaker_label.is_empty())
                        .then(|| Arc::from(event.speaker_label.as_str())),
                    at_seconds: duration(
                        event.offset.ok_or(ProtocolError::MissingBody("offset"))?,
                        "offset",
                    )?
                    .as_secs(),
                })
            }
            Ok(wire::SystemTranscriptionStage::SpeakerMerged) => Ok(Self::SpeakerMerged {
                from: Arc::from(event.merged_from_label.as_str()),
                into: Arc::from(event.speaker_label.as_str()),
            }),
            Ok(wire::SystemTranscriptionStage::Stopped) => Ok(Self::Stopped),
            Ok(wire::SystemTranscriptionStage::Failed) => {
                error_code_from_wire(event.error_code).map(Self::Failed)
            }
            Ok(wire::SystemTranscriptionStage::Unspecified) | Err(..) => {
                Err(ProtocolError::InvalidEnum {
                    field: "stage",
                    value: event.stage,
                })
            }
        }
    }
}

impl From<SpeechStage> for wire::SpeechEvent {
    fn from(stage: SpeechStage) -> Self {
        let (code, error) = match stage {
            SpeechStage::Synthesizing => (wire::SpeechStage::Synthesizing, 0),
            SpeechStage::Speaking => (wire::SpeechStage::Speaking, 0),
            SpeechStage::Spoken => (wire::SpeechStage::Completed, 0),
            SpeechStage::Stopped => (wire::SpeechStage::Interrupted, 0),
            SpeechStage::Cancelled => (wire::SpeechStage::Cancelled, 0),
            SpeechStage::Failed(code) => (wire::SpeechStage::Failed, error_code_to_wire(code)),
        };
        Self {
            stage: code as i32,
            error_code: error,
        }
    }
}

impl TryFrom<&wire::SpeechEvent> for SpeechStage {
    type Error = ProtocolError;

    fn try_from(event: &wire::SpeechEvent) -> Result<Self, Self::Error> {
        match wire::SpeechStage::try_from(event.stage) {
            Ok(wire::SpeechStage::Synthesizing) => Ok(Self::Synthesizing),
            Ok(wire::SpeechStage::Speaking) => Ok(Self::Speaking),
            Ok(wire::SpeechStage::Completed) => Ok(Self::Spoken),
            Ok(wire::SpeechStage::Interrupted) => Ok(Self::Stopped),
            Ok(wire::SpeechStage::Cancelled) => Ok(Self::Cancelled),
            // 0 is rejected rather than kept as Unknown(0): unknown codes
            // must round-trip, but UNSPECIFIED must never re-enter the wire.
            Ok(wire::SpeechStage::Failed) => match error_code_from_wire(event.error_code) {
                Ok(code) => Ok(Self::Failed(code)),
                Err(error) => Err(error),
            },
            Ok(wire::SpeechStage::Unspecified) | Err(..) => Err(ProtocolError::InvalidEnum {
                field: "stage",
                value: event.stage,
            }),
        }
    }
}

impl From<SpeechRequest> for wire::SpeakRequest {
    fn from(request: SpeechRequest) -> Self {
        Self {
            text: request.text.as_str().to_owned(),
            voice: request.voice.map(|voice| voice.as_str().to_owned()),
            rate: request.rate.map(|rate| rate.get()),
        }
    }
}

impl TryFrom<wire::SpeakRequest> for SpeechRequest {
    type Error = ProtocolError;

    fn try_from(request: wire::SpeakRequest) -> Result<Self, Self::Error> {
        let rate = request
            .rate
            .map(|rate| SpeakingRate::new(rate).map_err(ProtocolError::InvalidSpeakingRate))
            .transpose()?;
        Ok(Self {
            text: SpeechText::new(request.text)?,
            voice: request.voice.map(Voice::new).transpose()?,
            rate,
        })
    }
}

impl From<SpeechOutcome> for wire::SpeakResponse {
    fn from(outcome: SpeechOutcome) -> Self {
        Self {
            outcome: match outcome {
                SpeechOutcome::Spoken => wire::SpeechOutcome::Completed,
                SpeechOutcome::Stopped => wire::SpeechOutcome::Interrupted,
            } as i32,
        }
    }
}

impl TryFrom<wire::SpeakResponse> for SpeechOutcome {
    type Error = ProtocolError;

    fn try_from(response: wire::SpeakResponse) -> Result<Self, Self::Error> {
        match wire::SpeechOutcome::try_from(response.outcome) {
            Ok(wire::SpeechOutcome::Completed) => Ok(Self::Spoken),
            Ok(wire::SpeechOutcome::Interrupted) => Ok(Self::Stopped),
            _ => Err(ProtocolError::InvalidEnum {
                field: "speech outcome",
                value: response.outcome,
            }),
        }
    }
}

trait WireEnum: TryFrom<i32> {
    type Domain;

    fn into_domain(self) -> Option<Self::Domain>;
}

impl From<OutputMode> for wire::OutputMode {
    fn from(mode: OutputMode) -> Self {
        match mode {
            OutputMode::Insert => Self::Insert,
            OutputMode::InsertAndSubmit => Self::InsertAndSubmit,
            OutputMode::InsertOrCopy => Self::InsertOrCopy,
            OutputMode::Copy => Self::Copy,
        }
    }
}

impl WireEnum for wire::OutputMode {
    type Domain = OutputMode;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::Insert => Some(OutputMode::Insert),
            Self::InsertAndSubmit => Some(OutputMode::InsertAndSubmit),
            Self::InsertOrCopy => Some(OutputMode::InsertOrCopy),
            Self::Copy => Some(OutputMode::Copy),
            Self::Unspecified => None,
        }
    }
}

impl From<DictationRequest> for wire::StartDictationRequest {
    fn from(request: DictationRequest) -> Self {
        use wire::start_dictation_request::Route;
        let route = match request.route {
            DictationRoute::Deliver(overrides) => Route::Deliver(wire::DeliverRoute {
                output_mode: overrides
                    .output_mode
                    .map(|mode| wire::OutputMode::from(mode) as i32),
                delivery_mode: overrides
                    .delivery_mode
                    .map(|mode| wire::DeliveryMode::from(mode) as i32),
            }),
            DictationRoute::TranscriptOnly => Route::TranscriptOnly(wire::TranscriptOnlyRoute {}),
            DictationRoute::HistoryOnly => Route::HistoryOnly(wire::HistoryOnlyRoute {}),
        };
        Self {
            stop_mode: wire::StopMode::from(request.stop_mode) as i32,
            route: Some(route),
            project: project_to_wire(request.project),
        }
    }
}

impl TryFrom<wire::StartDictationRequest> for DictationRequest {
    type Error = ProtocolError;

    fn try_from(request: wire::StartDictationRequest) -> Result<Self, Self::Error> {
        use wire::start_dictation_request::Route;
        let stop_mode = decode_enum::<wire::StopMode>(request.stop_mode, "stop mode")?;
        let route = match request.route.ok_or(ProtocolError::MissingBody("route"))? {
            Route::Deliver(deliver) => DictationRoute::Deliver(DictationOverrides {
                output_mode: deliver
                    .output_mode
                    .map(|value| decode_enum::<wire::OutputMode>(value, "output mode"))
                    .transpose()?,
                delivery_mode: deliver
                    .delivery_mode
                    .map(|value| decode_enum::<wire::DeliveryMode>(value, "delivery mode"))
                    .transpose()?,
            }),
            Route::TranscriptOnly(wire::TranscriptOnlyRoute {}) => DictationRoute::TranscriptOnly,
            Route::HistoryOnly(wire::HistoryOnlyRoute {}) => DictationRoute::HistoryOnly,
        };

        Ok(Self {
            stop_mode,
            route,
            project: project_from_wire(request.project)?,
        })
    }
}

fn project_to_wire(
    context: aria_voice::vocabulary::Context,
) -> Option<wire::start_dictation_request::Project> {
    use aria_voice::vocabulary::Context;
    use wire::start_dictation_request::Project;
    match context {
        Context::Default => None,
        Context::Directory(path) => Some(Project::ProjectDirectory(path.as_str().to_owned())),
        Context::Disabled => Some(Project::ProjectVocabularyDisabled(
            wire::ProjectVocabularyDisabled {},
        )),
    }
}

fn project_from_wire(
    project: Option<wire::start_dictation_request::Project>,
) -> Result<aria_voice::vocabulary::Context, ProtocolError> {
    use aria_voice::vocabulary::Context;
    use wire::start_dictation_request::Project;
    Ok(match project {
        None => Context::Default,
        Some(Project::ProjectDirectory(path)) => {
            Context::Directory(aria_settings::Directory::new(path.into())?)
        }
        Some(Project::ProjectVocabularyDisabled(_)) => Context::Disabled,
    })
}

impl From<aria_voice::vocabulary::Status> for wire::VocabularyStatus {
    fn from(status: aria_voice::vocabulary::Status) -> Self {
        use aria_voice::vocabulary::State;
        Self {
            state: match status.state {
                State::Ready => wire::VocabularyState::Ready,
                State::Refreshing => wire::VocabularyState::Refreshing,
                State::Stale => wire::VocabularyState::Stale,
                State::Unavailable => wire::VocabularyState::Unavailable,
                State::TooLarge => wire::VocabularyState::TooLarge,
                State::Disabled => wire::VocabularyState::Disabled,
            } as i32,
            catalog_revision: status.catalog_revision,
            runtime_revision: status.runtime_revision,
            term_count: status.terms as u64,
            error_category: status.error,
        }
    }
}

impl TryFrom<wire::ListHistoryRequest> for aria_voice::history::HistoryQuery {
    type Error = ProtocolError;

    fn try_from(request: wire::ListHistoryRequest) -> Result<Self, Self::Error> {
        if request.query.len() > Self::MAX_QUERY_BYTES
            || !(1..=100).contains(&request.limit)
            || request
                .before_id
                .is_some_and(|id| id == 0 || id > i64::MAX as u64)
        {
            return Err(ProtocolError::InvalidHistory("query"));
        }
        Ok(Self {
            section: request.section.map(history_section).transpose()?,
            query: request.query,
            before_id: request.before_id,
            limit: request.limit as usize,
            collapse_chats: request.collapse_chats,
        })
    }
}

impl TryFrom<wire::GetChatRequest> for ChatQuery {
    type Error = ProtocolError;

    fn try_from(request: wire::GetChatRequest) -> Result<Self, Self::Error> {
        let limit = request.limit as usize;
        if !(1..=Self::MAX_LIMIT).contains(&limit) {
            return Err(ProtocolError::InvalidHistory("limit"));
        }
        Ok(Self {
            chat: required_chat(request.chat)?,
            before_id: request.before_id.map(history_id).transpose()?,
            limit,
        })
    }
}

impl From<ChatPage> for wire::GetChatResponse {
    fn from(page: ChatPage) -> Self {
        Self {
            entries: page.entries.into_iter().map(Into::into).collect(),
            next_before_id: page.next_before_id,
        }
    }
}

impl From<ChatQuery> for wire::GetChatRequest {
    fn from(query: ChatQuery) -> Self {
        Self {
            chat: Some(query.chat.into()),
            before_id: query.before_id,
            limit: u32::try_from(query.limit).unwrap_or(u32::MAX),
        }
    }
}

impl TryFrom<wire::GetChatResponse> for ChatPage {
    type Error = ProtocolError;

    fn try_from(page: wire::GetChatResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            entries: page
                .entries
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            next_before_id: history_cursor(page.next_before_id)?,
        })
    }
}

impl From<ChatKey> for wire::DeleteChatRequest {
    fn from(chat: ChatKey) -> Self {
        Self {
            chat: Some(chat.into()),
        }
    }
}

impl TryFrom<wire::DeleteChatRequest> for ChatKey {
    type Error = ProtocolError;

    fn try_from(request: wire::DeleteChatRequest) -> Result<Self, Self::Error> {
        required_chat(request.chat)
    }
}

/// UNSPECIFIED is never a section: an absent filter says "every section".
pub(crate) fn history_section(
    value: i32,
) -> Result<aria_voice::history::HistorySection, ProtocolError> {
    use aria_voice::history::HistorySection;
    match wire::HistorySection::try_from(value) {
        Ok(wire::HistorySection::Dictation) => Ok(HistorySection::Dictation),
        Ok(wire::HistorySection::SystemTranscription) => Ok(HistorySection::SystemTranscription),
        Ok(wire::HistorySection::Chat) => Ok(HistorySection::Chat),
        Ok(wire::HistorySection::Unspecified) | Err(_) => {
            Err(ProtocolError::InvalidHistory("section"))
        }
    }
}

impl From<aria_voice::history::HistorySection> for wire::HistorySection {
    fn from(section: aria_voice::history::HistorySection) -> Self {
        use aria_voice::history::HistorySection;
        match section {
            HistorySection::Dictation => Self::Dictation,
            HistorySection::SystemTranscription => Self::SystemTranscription,
            HistorySection::Chat => Self::Chat,
        }
    }
}

impl From<(u64, Option<&aria_voice::history::Title>)> for wire::RenameHistoryRequest {
    fn from((id, title): (u64, Option<&aria_voice::history::Title>)) -> Self {
        Self {
            id,
            title: title.map(|title| title.as_str().to_owned()),
        }
    }
}

impl TryFrom<wire::RenameHistoryRequest> for (u64, Option<aria_voice::history::Title>) {
    type Error = ProtocolError;

    fn try_from(request: wire::RenameHistoryRequest) -> Result<Self, Self::Error> {
        let title = history_title(request.title.as_deref(), "title")?;
        Ok((history_id(request.id)?, title))
    }
}

impl From<(ChatKey, &ChatName)> for wire::RenameChatRequest {
    fn from((chat, name): (ChatKey, &ChatName)) -> Self {
        Self {
            chat: Some(chat.into()),
            name: name.as_str().to_owned(),
        }
    }
}

impl TryFrom<wire::RenameChatRequest> for (ChatKey, ChatName) {
    type Error = ProtocolError;

    fn try_from(request: wire::RenameChatRequest) -> Result<Self, Self::Error> {
        Ok((required_chat(request.chat)?, ChatName::new(&request.name)?))
    }
}

impl From<ChatKey> for wire::RenameChatResponse {
    fn from(chat: ChatKey) -> Self {
        Self {
            chat: Some(chat.into()),
        }
    }
}

impl TryFrom<wire::RenameChatResponse> for ChatKey {
    type Error = ProtocolError;

    fn try_from(response: wire::RenameChatResponse) -> Result<Self, Self::Error> {
        required_chat(response.chat)
    }
}

fn history_title(
    title: Option<&str>,
    field: &'static str,
) -> Result<Option<aria_voice::history::Title>, ProtocolError> {
    title
        .map(aria_voice::history::Title::new)
        .transpose()
        .map_err(|_| ProtocolError::InvalidHistory(field))
}

impl From<ChatKey> for wire::ChatKey {
    fn from(chat: ChatKey) -> Self {
        Self {
            assistant_id: wire::AssistantId::from(chat.assistant_id) as i32,
            name: chat.name.as_str().to_owned(),
        }
    }
}

/// A request or response whose chat is required.
fn required_chat(chat: Option<wire::ChatKey>) -> Result<ChatKey, ProtocolError> {
    chat.ok_or(ProtocolError::MissingBody("chat"))?.try_into()
}

impl TryFrom<wire::ChatKey> for ChatKey {
    type Error = ProtocolError;

    fn try_from(chat: wire::ChatKey) -> Result<Self, Self::Error> {
        Ok(Self {
            assistant_id: decode_enum::<wire::AssistantId>(chat.assistant_id, "assistant_id")?,
            name: ChatName::new(&chat.name)?,
        })
    }
}

fn history_content(
    section: i32,
    details: Option<wire::history_entry::Details>,
    chat: Option<wire::ChatKey>,
    dictation: aria_voice::history::DictationContent,
) -> Result<aria_voice::history::HistoryContent, ProtocolError> {
    use aria_voice::history::{HistoryContent, HistorySection, TranscriptRead};
    use wire::history_entry::Details;
    let chat = chat.map(ChatKey::try_from).transpose()?;
    // A daemon from before sections sends none, and it kept only dictations.
    if section == wire::HistorySection::Unspecified as i32 && details.is_none() && chat.is_none() {
        return Ok(HistoryContent::Dictation(dictation));
    }
    match (history_section(section)?, details, chat) {
        (HistorySection::Dictation, None, None) => Ok(HistoryContent::Dictation(dictation)),
        (HistorySection::SystemTranscription, Some(Details::Transcript(transcript)), None) => {
            let read = match transcript.read.map(wire::HistoryTranscriptRead::try_from) {
                None => None,
                Some(Ok(wire::HistoryTranscriptRead::Whole)) => Some(TranscriptRead::Whole),
                Some(Ok(wire::HistoryTranscriptRead::Truncated)) => Some(TranscriptRead::Truncated),
                Some(Ok(wire::HistoryTranscriptRead::Unavailable)) => {
                    Some(TranscriptRead::Unavailable)
                }
                Some(Ok(wire::HistoryTranscriptRead::Unspecified) | Err(_)) => {
                    return Err(ProtocolError::InvalidHistory("transcript read"));
                }
            };
            Ok(HistoryContent::SystemTranscription {
                transcript: transcript.path.into(),
                read,
            })
        }
        (HistorySection::Chat, Some(Details::Reply(reply)), chat) => {
            Ok(HistoryContent::Ask { reply, chat })
        }
        _ => Err(ProtocolError::InvalidHistory("details")),
    }
}

fn history_details(
    content: aria_voice::history::HistoryContent,
) -> Option<wire::history_entry::Details> {
    use aria_voice::history::{HistoryContent, TranscriptRead};
    match content {
        HistoryContent::Dictation(_) => None,
        HistoryContent::Ask { reply, .. } => Some(wire::history_entry::Details::Reply(reply)),
        HistoryContent::SystemTranscription { transcript, read } => Some(
            wire::history_entry::Details::Transcript(wire::HistoryTranscript {
                path: transcript.to_string_lossy().into_owned(),
                read: read.map(|read| {
                    let read = match read {
                        TranscriptRead::Whole => wire::HistoryTranscriptRead::Whole,
                        TranscriptRead::Truncated => wire::HistoryTranscriptRead::Truncated,
                        TranscriptRead::Unavailable => wire::HistoryTranscriptRead::Unavailable,
                    };
                    read as i32
                }),
            }),
        ),
    }
}

pub(crate) fn history_id(id: u64) -> Result<u64, ProtocolError> {
    if id == 0 || id > i64::MAX as u64 {
        return Err(ProtocolError::InvalidHistory("id"));
    }
    Ok(id)
}

impl TryFrom<wire::RegenerateHistoryRequest> for (u64, aria_voice::history::RetryOptions) {
    type Error = ProtocolError;

    fn try_from(request: wire::RegenerateHistoryRequest) -> Result<Self, Self::Error> {
        let id = history_id(request.id)?;
        if request
            .engine_id
            .as_deref()
            .is_some_and(|engine| aria_settings::config::backend::local(engine).is_none())
        {
            return Err(ProtocolError::InvalidHistory("engine"));
        }
        Ok((
            id,
            aria_voice::history::RetryOptions {
                engine: request.engine_id,
                language: request.language.as_deref().map(str::parse).transpose()?,
            },
        ))
    }
}

impl TryFrom<wire::HistoryEntry> for aria_voice::history::HistoryEntry {
    type Error = ProtocolError;

    fn try_from(entry: wire::HistoryEntry) -> Result<Self, Self::Error> {
        use aria_platform::delivery::Application;
        use aria_voice::history::{
            AudioAvailability, DictationContent, HistorySource, HistoryStatus,
        };
        if entry.id == 0 || entry.id > i64::MAX as u64 {
            return Err(ProtocolError::InvalidHistory("id"));
        }
        let source = match wire::HistorySource::try_from(entry.source) {
            Ok(wire::HistorySource::Local) => HistorySource::Local,
            Ok(wire::HistorySource::Relay) => HistorySource::Relay,
            _ => return Err(ProtocolError::InvalidHistory("source")),
        };
        let status = match wire::HistoryStatus::try_from(entry.status) {
            Ok(wire::HistoryStatus::Success) => HistoryStatus::Success,
            Ok(wire::HistoryStatus::TranscriptionFailed) => HistoryStatus::TranscriptionFailed,
            Ok(wire::HistoryStatus::DeliveryFailed) => HistoryStatus::DeliveryFailed,
            Ok(wire::HistoryStatus::Recovered) => HistoryStatus::Recovered,
            _ => return Err(ProtocolError::InvalidHistory("status")),
        };
        if entry.audio_expire_time.is_some()
            && entry.audio_availability != wire::HistoryAudioAvailability::Available as i32
        {
            return Err(ProtocolError::InvalidHistory("audio expiry"));
        }
        let audio = match wire::HistoryAudioAvailability::try_from(entry.audio_availability) {
            Ok(wire::HistoryAudioAvailability::Unavailable) => AudioAvailability::Unavailable,
            Ok(wire::HistoryAudioAvailability::Expired) => AudioAvailability::Expired,
            Ok(wire::HistoryAudioAvailability::Available) => AudioAvailability::Available {
                expires_at_ms: millis(
                    since_epoch(entry.audio_expire_time, "audio_expire_time")?,
                    "audio_expire_time",
                )?,
            },
            _ => return Err(ProtocolError::InvalidHistory("audio")),
        };
        let dictation = DictationContent {
            language: entry.language,
            engine: entry.engine_id,
            own_voice_only: false,
            audio,
            retrying: entry.regenerating,
        };
        Ok(Self {
            id: entry.id,
            title: history_title(entry.title.as_deref(), "title")?,
            content: history_content(entry.section, entry.details, entry.chat, dictation)?,
            created_at_ms: millis(
                since_epoch(entry.create_time, "create_time")?,
                "create_time",
            )?,
            duration_ms: entry
                .duration
                .map(|span| millis(duration(span, "duration")?, "duration"))
                .transpose()?,
            source,
            application: entry.application.map(|app| Application {
                name: app.name,
                bundle_id: app.bundle_id,
            }),
            project: entry.project_directory.map(std::path::PathBuf::from),
            text: entry.text,
            complete: entry.transcription_complete,
            result_count: entry.result_count,
            status,
            error: entry.error.map(TryInto::try_into).transpose()?,
            warning: entry.warning,
            deletion_pending: entry.deletion_pending,
        })
    }
}

impl From<aria_voice::history::HistoryEntry> for wire::RenameHistoryResponse {
    fn from(entry: aria_voice::history::HistoryEntry) -> Self {
        Self {
            entry: Some(entry.into()),
        }
    }
}

impl TryFrom<wire::RenameHistoryResponse> for aria_voice::history::HistoryEntry {
    type Error = ProtocolError;

    fn try_from(response: wire::RenameHistoryResponse) -> Result<Self, Self::Error> {
        response
            .entry
            .ok_or(ProtocolError::MissingBody("history entry"))?
            .try_into()
    }
}

impl TryFrom<wire::ListHistoryResponse> for aria_voice::history::HistoryPage {
    type Error = ProtocolError;

    fn try_from(page: wire::ListHistoryResponse) -> Result<Self, Self::Error> {
        Ok(Self {
            entries: page
                .entries
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            next_before_id: history_cursor(page.next_before_id)?,
            warning: page.warning,
        })
    }
}

fn history_cursor(id: Option<u64>) -> Result<Option<u64>, ProtocolError> {
    if id.is_some_and(|id| id == 0 || id > i64::MAX as u64) {
        return Err(ProtocolError::InvalidHistory("cursor"));
    }
    Ok(id)
}

impl From<aria_voice::history::HistoryEntry> for wire::HistoryEntry {
    fn from(entry: aria_voice::history::HistoryEntry) -> Self {
        use aria_voice::history::{
            AudioAvailability, HistoryContent, HistorySource, HistoryStatus,
        };
        let (audio, audio_expire_time) = match entry.audio() {
            AudioAvailability::Unavailable => (wire::HistoryAudioAvailability::Unavailable, None),
            AudioAvailability::Expired => (wire::HistoryAudioAvailability::Expired, None),
            AudioAvailability::Available { expires_at_ms } => (
                wire::HistoryAudioAvailability::Available,
                Some(wire_time(Duration::from_millis(expires_at_ms))),
            ),
        };
        let section = wire::HistorySection::from(entry.content.section()) as i32;
        let chat = entry.content.chat().cloned().map(wire::ChatKey::from);
        let (details, dictation) = match entry.content {
            HistoryContent::Dictation(dictation) => (None, Some(dictation)),
            content => (history_details(content), None),
        };
        let regenerating = dictation
            .as_ref()
            .is_some_and(|dictation| dictation.retrying);
        let (language, engine_id) = dictation
            .map(|dictation| (dictation.language, dictation.engine))
            .unwrap_or_default();
        Self {
            id: entry.id,
            title: entry.title.map(|title| title.as_str().to_owned()),
            section,
            create_time: Some(wire_time(Duration::from_millis(entry.created_at_ms))),
            duration: entry
                .duration_ms
                .map(|ms| wire_duration(Duration::from_millis(ms))),
            source: match entry.source {
                HistorySource::Local => wire::HistorySource::Local,
                HistorySource::Relay => wire::HistorySource::Relay,
            } as i32,
            application: entry.application.map(|app| wire::HistoryApplication {
                name: app.name,
                bundle_id: app.bundle_id,
            }),
            project_directory: entry
                .project
                .map(|project| project.to_string_lossy().into_owned()),
            text: entry.text,
            transcription_complete: entry.complete,
            details,
            result_count: entry.result_count,
            engine_id,
            language,
            status: match entry.status {
                HistoryStatus::Success => wire::HistoryStatus::Success,
                HistoryStatus::TranscriptionFailed => wire::HistoryStatus::TranscriptionFailed,
                HistoryStatus::DeliveryFailed => wire::HistoryStatus::DeliveryFailed,
                HistoryStatus::Recovered => wire::HistoryStatus::Recovered,
            } as i32,
            error: entry.error.map(Into::into),
            warning: entry.warning,
            audio_availability: audio as i32,
            audio_expire_time,
            regenerating,
            deletion_pending: entry.deletion_pending,
            can_regenerate: audio == wire::HistoryAudioAvailability::Available
                && !regenerating
                && !entry.deletion_pending,
            chat,
        }
    }
}

/// List carries only the start of each text.
fn cut_for_list(text: &mut String) {
    let mut end = text.len().min(512);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

impl TryFrom<wire::PromptResult> for PromptResult {
    type Error = ProtocolError;

    fn try_from(result: wire::PromptResult) -> Result<Self, Self::Error> {
        Ok(Self {
            id: result.id,
            created_at_ms: millis(
                since_epoch(result.create_time, "create_time")?,
                "create_time",
            )?,
            prompt_name: result.prompt_name,
            text: result.text,
        })
    }
}

impl From<PromptResult> for wire::PromptResult {
    fn from(result: PromptResult) -> Self {
        Self {
            id: result.id,
            create_time: Some(wire_time(Duration::from_millis(result.created_at_ms))),
            prompt_name: result.prompt_name,
            text: result.text,
        }
    }
}

impl From<Vec<PromptResult>> for wire::HistoryThread {
    fn from(results: Vec<PromptResult>) -> Self {
        Self {
            results: results.into_iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<wire::HistoryThread> for Vec<PromptResult> {
    type Error = ProtocolError;

    fn try_from(thread: wire::HistoryThread) -> Result<Self, Self::Error> {
        thread.results.into_iter().map(TryInto::try_into).collect()
    }
}

impl From<aria_voice::history::HistoryPage> for wire::ListHistoryResponse {
    fn from(page: aria_voice::history::HistoryPage) -> Self {
        Self {
            entries: page
                .entries
                .into_iter()
                .map(|entry| {
                    let mut entry = wire::HistoryEntry::from(entry);
                    cut_for_list(&mut entry.text);
                    if let Some(wire::history_entry::Details::Reply(reply)) = &mut entry.details {
                        cut_for_list(reply);
                    }
                    entry
                })
                .collect(),
            next_before_id: page.next_before_id,
            warning: page.warning,
        }
    }
}

impl TryFrom<wire::VocabularyRequest> for Option<aria_settings::Directory> {
    type Error = ProtocolError;

    fn try_from(request: wire::VocabularyRequest) -> Result<Self, Self::Error> {
        request
            .directory
            .map(|path| aria_settings::Directory::new(path.into()).map_err(Into::into))
            .transpose()
    }
}

impl TryFrom<wire::VocabularyStatus> for aria_voice::vocabulary::Status {
    type Error = ProtocolError;
    fn try_from(status: wire::VocabularyStatus) -> Result<Self, Self::Error> {
        use aria_voice::vocabulary::State;
        if status.term_count > 100_000 {
            return Err(ProtocolError::InvalidVocabularyTerms(status.term_count));
        }
        let state = match wire::VocabularyState::try_from(status.state) {
            Ok(wire::VocabularyState::Ready) => State::Ready,
            Ok(wire::VocabularyState::Refreshing) => State::Refreshing,
            Ok(wire::VocabularyState::Stale) => State::Stale,
            Ok(wire::VocabularyState::Unavailable) => State::Unavailable,
            Ok(wire::VocabularyState::TooLarge) => State::TooLarge,
            Ok(wire::VocabularyState::Disabled) => State::Disabled,
            _ => {
                return Err(ProtocolError::InvalidEnum {
                    field: "vocabulary state",
                    value: status.state,
                });
            }
        };
        Ok(Self {
            state,
            catalog_revision: status.catalog_revision,
            runtime_revision: status.runtime_revision,
            terms: status.term_count as usize,
            error: status.error_category,
        })
    }
}

impl From<DeliveryMode> for wire::DeliveryMode {
    fn from(mode: DeliveryMode) -> Self {
        match mode {
            DeliveryMode::Complete => Self::Complete,
            DeliveryMode::Streaming => Self::Streaming,
        }
    }
}

impl From<StreamingUnavailable> for wire::StreamingUnavailable {
    fn from(reason: StreamingUnavailable) -> Self {
        match reason {
            StreamingUnavailable::Copy => Self::Copy,
            StreamingUnavailable::Platform => Self::Platform,
            StreamingUnavailable::Model => Self::Model,
        }
    }
}

impl WireEnum for wire::StreamingUnavailable {
    type Domain = StreamingUnavailable;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::Unspecified => None,
            Self::Copy => Some(StreamingUnavailable::Copy),
            Self::Platform => Some(StreamingUnavailable::Platform),
            Self::Model => Some(StreamingUnavailable::Model),
        }
    }
}

impl WireEnum for wire::DeliveryMode {
    type Domain = DeliveryMode;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::Complete => Some(DeliveryMode::Complete),
            Self::Streaming => Some(DeliveryMode::Streaming),
            Self::Unspecified => None,
        }
    }
}

impl From<AriaKey> for wire::AriaKey {
    fn from(key: AriaKey) -> Self {
        match key {
            AriaKey::RightOption => Self::RightOption,
            AriaKey::Fn => Self::Fn,
        }
    }
}

impl WireEnum for wire::AriaKey {
    type Domain = AriaKey;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::RightOption => Some(AriaKey::RightOption),
            Self::Fn => Some(AriaKey::Fn),
            Self::Unspecified => None,
        }
    }
}

impl From<(u64, SettingsPatch)> for wire::UpdateSettingsRequest {
    fn from((expected_revision, patch): (u64, SettingsPatch)) -> Self {
        Self {
            expected_revision,
            patch: Some(patch.into()),
        }
    }
}

impl TryFrom<wire::CheckSettingsRequest> for SettingsPatch {
    type Error = ProtocolError;

    fn try_from(check: wire::CheckSettingsRequest) -> Result<Self, Self::Error> {
        check
            .patch
            .ok_or(ProtocolError::MissingSettingsPatch)?
            .try_into()
    }
}

impl TryFrom<wire::UpdateSettingsRequest> for (u64, SettingsPatch) {
    type Error = ProtocolError;

    fn try_from(update: wire::UpdateSettingsRequest) -> Result<Self, Self::Error> {
        let patch = update.patch.ok_or(ProtocolError::MissingSettingsPatch)?;
        Ok((update.expected_revision, patch.try_into()?))
    }
}

fn decode_assistant_models(
    entries: Vec<wire::AssistantModelSetting>,
) -> Result<BTreeMap<AssistantId, String>, ProtocolError> {
    let mut models = BTreeMap::new();
    for entry in entries {
        let assistant_id = decode_enum::<wire::AssistantId>(entry.assistant_id, "assistant_id")?;
        if !aria_settings::is_valid_model_id(&entry.model_id) {
            return Err(ProtocolError::InvalidModelId);
        }
        if models.insert(assistant_id, entry.model_id).is_some() {
            return Err(ProtocolError::DuplicateAssistantModel);
        }
    }
    Ok(models)
}

fn encode_assistant_models(
    models: &BTreeMap<AssistantId, String>,
) -> Vec<wire::AssistantModelSetting> {
    models
        .iter()
        .map(|(assistant_id, model_id)| wire::AssistantModelSetting {
            assistant_id: wire::AssistantId::from(*assistant_id) as i32,
            model_id: model_id.clone(),
        })
        .collect()
}

impl TryFrom<wire::SettingsSnapshot> for SettingsSnapshot {
    type Error = ProtocolError;

    fn try_from(snapshot: wire::SettingsSnapshot) -> Result<Self, Self::Error> {
        let settings = snapshot.settings.ok_or(ProtocolError::MissingSettings)?;
        let streaming_unavailable = snapshot
            .streaming_unavailable
            .iter()
            .map(|entry| {
                Ok(StreamingUnavailableOutput {
                    output_mode: decode_enum::<wire::OutputMode>(entry.output_mode, "output mode")?,
                    reason: decode_enum::<wire::StreamingUnavailable>(
                        entry.reason,
                        "streaming unavailable reason",
                    )?,
                })
            })
            .collect::<Result<StreamingSupport, ProtocolError>>()?;
        Ok(Self {
            revision: snapshot.revision,
            streaming_unavailable,
            settings: Settings {
                output_mode: decode_enum::<wire::OutputMode>(settings.output_mode, "output mode")?,
                delivery_mode: decode_enum::<wire::DeliveryMode>(
                    settings.delivery_mode,
                    "delivery mode",
                )?,
                silence_duration: SilenceDuration::new(Duration::from_millis(
                    settings.silence_duration_ms,
                ))?,
                auto_stop: settings
                    .auto_stop_ms
                    .map(|milliseconds| AutoStop::new(Duration::from_millis(milliseconds)))
                    .transpose()?,
                mute_sound: settings.mute_sound,
                return_sends: settings.return_sends,
                cleanup_enabled: settings.cleanup_enabled,
                cleanup_fillers: settings.cleanup_fillers,
                cleanup_backtrack: settings.cleanup_backtrack,
                cleanup_llm_enabled: settings.cleanup_llm_enabled,
                own_voice_only: settings.own_voice_only,
                vocabulary_enabled: settings.vocabulary_enabled,
                vocabulary_dir: settings
                    .vocabulary_dir
                    .map(|path| aria_settings::Directory::new(path.into()))
                    .transpose()?,
                history_enabled: settings.history_enabled,
                history_keep_audio: settings.history_keep_audio,
                assistant_id: settings
                    .assistant_id
                    .map(|value| decode_enum::<wire::AssistantId>(value, "assistant_id"))
                    .transpose()?,
                assistant_models: decode_assistant_models(settings.assistant_models)?,
                memory_enabled: settings.memory_enabled,
                prompts: settings
                    .prompts
                    .ok_or(ProtocolError::MissingBody("prompts"))?
                    .try_into()?,
                application_overrides: settings
                    .application_overrides
                    .ok_or(ProtocolError::MissingBody("application overrides"))?
                    .try_into()?,
                aria_key: decode_enum::<wire::AriaKey>(settings.aria_key, "hotkey key")?,
                ask_shortcut: Option::<Shortcut>::try_from(
                    settings
                        .ask_shortcut
                        .ok_or(ProtocolError::MissingBody("ask hotkey"))?,
                )?,
                prompt_list_shortcut: Option::<Shortcut>::try_from(
                    settings
                        .prompt_list_shortcut
                        .ok_or(ProtocolError::MissingBody("prompt list hotkey"))?,
                )?,
                stt_language: settings.stt_language.parse()?,
                tts_rate: SpeakingRate::new(settings.tts_rate as f32)
                    .map_err(ProtocolError::InvalidSpeakingRate)?,
            },
        })
    }
}

impl From<Vec<(AssistantId, AssistantStatus)>> for wire::ListAssistantsResponse {
    fn from(assistants: Vec<(AssistantId, AssistantStatus)>) -> Self {
        Self {
            assistants: assistants.into_iter().map(wire::Assistant::from).collect(),
        }
    }
}

impl From<AssistantModel> for wire::AssistantModel {
    fn from(model: AssistantModel) -> Self {
        Self {
            id: model.id,
            name: model.name,
        }
    }
}

impl TryFrom<wire::AssistantModel> for AssistantModel {
    type Error = ProtocolError;

    fn try_from(model: wire::AssistantModel) -> Result<Self, Self::Error> {
        let model = Self {
            id: model.id,
            name: model.name,
        };
        if !model.is_valid() {
            return Err(ProtocolError::InvalidModelId);
        }
        Ok(model)
    }
}

impl From<Vec<AssistantModel>> for wire::ListAssistantModelsResponse {
    fn from(models: Vec<AssistantModel>) -> Self {
        Self {
            models: models.into_iter().map(wire::AssistantModel::from).collect(),
        }
    }
}

impl TryFrom<wire::ListAssistantModelsResponse> for Vec<AssistantModel> {
    type Error = ProtocolError;

    fn try_from(response: wire::ListAssistantModelsResponse) -> Result<Self, Self::Error> {
        response
            .models
            .into_iter()
            .map(AssistantModel::try_from)
            .collect()
    }
}

impl From<(AssistantId, AssistantStatus)> for wire::Assistant {
    fn from((id, status): (AssistantId, AssistantStatus)) -> Self {
        Self {
            id: wire::AssistantId::from(id) as i32,
            name: id.display_name().to_owned(),
            status: wire::AssistantStatus::from(&status) as i32,
            fix: status.fix().unwrap_or_default(),
            needs_model: id.needs_model(),
        }
    }
}

impl From<&AssistantStatus> for wire::AssistantStatus {
    fn from(status: &AssistantStatus) -> Self {
        match status {
            AssistantStatus::Ready => Self::Ready,
            AssistantStatus::NotInstalled { .. } => Self::NotInstalled,
            AssistantStatus::SignedOut { .. } => Self::SignedOut,
            AssistantStatus::NotConfigured { .. } => Self::NotConfigured,
            AssistantStatus::Unreachable { .. } => Self::Unreachable,
            AssistantStatus::ModelMissing { .. } => Self::ModelMissing,
            AssistantStatus::ModelUnset { .. } => Self::ModelUnset,
            AssistantStatus::Unchecked(_) => Self::Unchecked,
        }
    }
}

impl TryFrom<wire::ListAssistantModelsRequest> for AssistantId {
    type Error = ProtocolError;

    fn try_from(request: wire::ListAssistantModelsRequest) -> Result<Self, Self::Error> {
        decode_enum::<wire::AssistantId>(request.assistant_id, "assistant_id")
    }
}

impl From<SettingsPatch> for wire::SettingsPatch {
    fn from(patch: SettingsPatch) -> Self {
        Self {
            output_mode: patch
                .output_mode
                .map(|mode| wire::OutputMode::from(mode) as i32),
            delivery_mode: patch
                .delivery_mode
                .map(|mode| wire::DeliveryMode::from(mode) as i32),
            silence_duration_ms: patch
                .silence_duration
                .map(|duration| duration.get().as_millis() as u64),
            auto_stop_ms: patch.auto_stop.map(|auto_stop| {
                auto_stop.map_or(0, |auto_stop| auto_stop.get().as_millis() as u64)
            }),
            mute_sound: patch.mute_sound,
            return_sends: patch.return_sends,
            cleanup_enabled: patch.cleanup_enabled,
            cleanup_fillers: patch.cleanup_fillers,
            cleanup_backtrack: patch.cleanup_backtrack,
            cleanup_llm_enabled: patch.cleanup_llm_enabled,
            own_voice_only: patch.own_voice_only,
            vocabulary_enabled: patch.vocabulary_enabled,
            vocabulary_dir: patch
                .vocabulary_dir
                .map(|path| path.map_or_else(String::new, |path| path.as_str().to_owned())),
            history_enabled: patch.history_enabled,
            history_keep_audio: patch.history_keep_audio,
            assistant_id: patch
                .assistant_id
                .map(|assistant_id| wire::AssistantId::from(assistant_id) as i32),
            assistant_models: encode_assistant_models(&patch.assistant_models),
            memory_enabled: patch.memory_enabled,
            prompts: patch.prompts.as_ref().map(wire::Prompts::from),
            application_overrides: patch
                .application_overrides
                .as_ref()
                .map(wire::ApplicationOverrides::from),
            aria_key: patch.aria_key.map(|key| wire::AriaKey::from(key) as i32),
            ask_shortcut: patch.ask_shortcut.map(wire::Shortcut::from),
            prompt_list_shortcut: patch.prompt_list_shortcut.map(wire::Shortcut::from),
            stt_language: patch
                .stt_language
                .map(|language| language.as_str().to_owned()),
            tts_rate: patch.tts_rate.map(|rate| f64::from(rate.get())),
        }
    }
}

impl From<&SettingsSnapshot> for wire::SettingsSnapshot {
    fn from(snapshot: &SettingsSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            settings: Some(wire::Settings {
                output_mode: wire::OutputMode::from(snapshot.settings.output_mode) as i32,
                delivery_mode: wire::DeliveryMode::from(snapshot.settings.delivery_mode) as i32,
                silence_duration_ms: snapshot.settings.silence_duration.get().as_millis() as u64,
                auto_stop_ms: snapshot
                    .settings
                    .auto_stop
                    .map(|auto_stop| auto_stop.get().as_millis() as u64),
                mute_sound: snapshot.settings.mute_sound,
                return_sends: snapshot.settings.return_sends,
                cleanup_enabled: snapshot.settings.cleanup_enabled,
                cleanup_fillers: snapshot.settings.cleanup_fillers,
                cleanup_backtrack: snapshot.settings.cleanup_backtrack,
                cleanup_llm_enabled: snapshot.settings.cleanup_llm_enabled,
                own_voice_only: snapshot.settings.own_voice_only,
                vocabulary_enabled: snapshot.settings.vocabulary_enabled,
                vocabulary_dir: snapshot
                    .settings
                    .vocabulary_dir
                    .as_ref()
                    .map(|path| path.as_str().to_owned()),
                history_enabled: snapshot.settings.history_enabled,
                history_keep_audio: snapshot.settings.history_keep_audio,
                assistant_id: snapshot
                    .settings
                    .assistant_id
                    .map(|assistant_id| wire::AssistantId::from(assistant_id) as i32),
                assistant_models: encode_assistant_models(&snapshot.settings.assistant_models),
                memory_enabled: snapshot.settings.memory_enabled,
                prompts: Some((&snapshot.settings.prompts).into()),
                application_overrides: Some((&snapshot.settings.application_overrides).into()),
                aria_key: wire::AriaKey::from(snapshot.settings.aria_key) as i32,
                ask_shortcut: Some(snapshot.settings.ask_shortcut.into()),
                prompt_list_shortcut: Some(snapshot.settings.prompt_list_shortcut.into()),
                stt_language: snapshot.settings.stt_language.as_str().to_owned(),
                tts_rate: f64::from(snapshot.settings.tts_rate.get()),
            }),
            streaming_unavailable: snapshot
                .streaming_unavailable
                .iter()
                .map(|entry| wire::StreamingUnavailableOutput {
                    output_mode: wire::OutputMode::from(entry.output_mode) as i32,
                    reason: wire::StreamingUnavailable::from(entry.reason) as i32,
                })
                .collect(),
            assistant_name: snapshot
                .settings
                .assistant_id
                .map(AssistantId::display_name)
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

impl From<StopMode> for wire::StopMode {
    fn from(mode: StopMode) -> Self {
        match mode {
            StopMode::OnSilence => Self::OnSilence,
            StopMode::Manual => Self::Manual,
        }
    }
}

impl WireEnum for wire::StopMode {
    type Domain = StopMode;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::OnSilence => Some(StopMode::OnSilence),
            Self::Manual => Some(StopMode::Manual),
            Self::Unspecified => None,
        }
    }
}

impl TryFrom<wire::SettingsPatch> for SettingsPatch {
    type Error = ProtocolError;

    fn try_from(patch: wire::SettingsPatch) -> Result<Self, Self::Error> {
        Ok(Self {
            output_mode: patch
                .output_mode
                .map(|value| decode_enum::<wire::OutputMode>(value, "output mode"))
                .transpose()?,
            delivery_mode: patch
                .delivery_mode
                .map(|value| decode_enum::<wire::DeliveryMode>(value, "delivery mode"))
                .transpose()?,
            silence_duration: patch
                .silence_duration_ms
                .map(|milliseconds| SilenceDuration::new(Duration::from_millis(milliseconds)))
                .transpose()?,
            auto_stop: patch
                .auto_stop_ms
                .map(|milliseconds| match milliseconds {
                    0 => Ok(None),
                    _ => AutoStop::new(Duration::from_millis(milliseconds)).map(Some),
                })
                .transpose()?,
            mute_sound: patch.mute_sound,
            return_sends: patch.return_sends,
            cleanup_enabled: patch.cleanup_enabled,
            cleanup_fillers: patch.cleanup_fillers,
            cleanup_backtrack: patch.cleanup_backtrack,
            cleanup_llm_enabled: patch.cleanup_llm_enabled,
            own_voice_only: patch.own_voice_only,
            vocabulary_enabled: patch.vocabulary_enabled,
            vocabulary_dir: patch
                .vocabulary_dir
                .map(|path| {
                    if path.is_empty() {
                        Ok(None)
                    } else {
                        aria_settings::Directory::new(path.into()).map(Some)
                    }
                })
                .transpose()?,
            history_enabled: patch.history_enabled,
            history_keep_audio: patch.history_keep_audio,
            assistant_id: patch
                .assistant_id
                .map(|value| decode_enum::<wire::AssistantId>(value, "assistant_id"))
                .transpose()?,
            assistant_models: decode_assistant_models(patch.assistant_models)?,
            memory_enabled: patch.memory_enabled,
            prompts: patch.prompts.map(Prompts::try_from).transpose()?,
            application_overrides: patch
                .application_overrides
                .map(ApplicationOverrides::try_from)
                .transpose()?,
            aria_key: patch
                .aria_key
                .map(|value| decode_enum::<wire::AriaKey>(value, "hotkey key"))
                .transpose()?,
            ask_shortcut: patch
                .ask_shortcut
                .map(Option::<Shortcut>::try_from)
                .transpose()?,
            prompt_list_shortcut: patch
                .prompt_list_shortcut
                .map(Option::<Shortcut>::try_from)
                .transpose()?,
            stt_language: patch.stt_language.as_deref().map(str::parse).transpose()?,
            tts_rate: patch
                .tts_rate
                .map(|rate| {
                    SpeakingRate::new(rate as f32).map_err(ProtocolError::InvalidSpeakingRate)
                })
                .transpose()?,
        })
    }
}

impl From<&ApplicationOverrides> for wire::ApplicationOverrides {
    fn from(overrides: &ApplicationOverrides) -> Self {
        Self {
            entries: overrides
                .iter()
                .map(|(id, entry)| wire::ApplicationOverride {
                    application_id: id.as_str().to_owned(),
                    overrides: Some(entry.overrides.into()),
                    excluded: entry.excluded,
                })
                .collect(),
        }
    }
}

impl TryFrom<wire::ApplicationOverrides> for ApplicationOverrides {
    type Error = ProtocolError;

    fn try_from(overrides: wire::ApplicationOverrides) -> Result<Self, Self::Error> {
        let entries = overrides
            .entries
            .into_iter()
            .map(|entry| {
                let overrides = entry
                    .overrides
                    .ok_or(ProtocolError::MissingBody("dictation overrides"))?
                    .try_into()?;
                let application_override = ApplicationOverride {
                    excluded: entry.excluded,
                    overrides,
                };
                Ok((
                    ApplicationId::new(entry.application_id)?,
                    application_override,
                ))
            })
            .collect::<Result<Vec<_>, ProtocolError>>()?;
        Ok(Self::new(entries)?)
    }
}

impl From<DictationOverrides> for wire::DictationOverrides {
    fn from(overrides: DictationOverrides) -> Self {
        let DictationOverrides {
            output_mode,
            delivery_mode,
        } = overrides;
        Self {
            output_mode: output_mode.map(|mode| wire::OutputMode::from(mode) as i32),
            delivery_mode: delivery_mode.map(|mode| wire::DeliveryMode::from(mode) as i32),
        }
    }
}

impl TryFrom<wire::DictationOverrides> for DictationOverrides {
    type Error = ProtocolError;

    fn try_from(overrides: wire::DictationOverrides) -> Result<Self, Self::Error> {
        let wire::DictationOverrides {
            output_mode,
            delivery_mode,
        } = overrides;
        Ok(Self {
            output_mode: output_mode
                .map(|value| decode_enum::<wire::OutputMode>(value, "output mode"))
                .transpose()?,
            delivery_mode: delivery_mode
                .map(|value| decode_enum::<wire::DeliveryMode>(value, "delivery mode"))
                .transpose()?,
        })
    }
}

impl From<ChatTarget> for wire::ResolveChatTargetResponse {
    fn from(target: ChatTarget) -> Self {
        Self {
            workdir: target.workdir.to_string_lossy().into_owned(),
            assistant_id: wire::AssistantId::from(target.assistant_id) as i32,
        }
    }
}

impl TryFrom<wire::ResolveChatTargetResponse> for ChatTarget {
    type Error = ProtocolError;

    fn try_from(response: wire::ResolveChatTargetResponse) -> Result<Self, Self::Error> {
        if response.workdir.is_empty() {
            return Err(ProtocolError::MissingBody("assistant working directory"));
        }
        Ok(Self {
            workdir: response.workdir.into(),
            assistant_id: decode_enum::<wire::AssistantId>(response.assistant_id, "assistant_id")?,
        })
    }
}

impl TryFrom<wire::Chord> for Chord {
    type Error = ProtocolError;

    fn try_from(chord: wire::Chord) -> Result<Self, Self::Error> {
        let key_code = u16::try_from(chord.key_code)
            .map_err(|_too_wide| ProtocolError::ChordKeyCode(chord.key_code))?;
        Ok(Self::new(
            key_code,
            Modifiers {
                control: chord.control,
                option: chord.option,
                shift: chord.shift,
                command: chord.command,
            },
        )?)
    }
}

impl From<Chord> for wire::Chord {
    fn from(chord: Chord) -> Self {
        let modifiers = chord.modifiers();
        Self {
            key_code: u32::from(chord.key_code()),
            control: modifiers.control,
            option: modifiers.option,
            shift: modifiers.shift,
            command: modifiers.command,
        }
    }
}

impl TryFrom<wire::Shortcut> for Option<Shortcut> {
    type Error = ProtocolError;

    fn try_from(hotkey: wire::Shortcut) -> Result<Self, Self::Error> {
        use wire::shortcut::Binding;
        match hotkey.binding {
            None => Ok(None),
            Some(Binding::Chord(chord)) => Chord::try_from(chord).map(Shortcut::Chord).map(Some),
            Some(Binding::Combo(combo)) => {
                decode_enum::<wire::ShortcutCombo>(combo, "hotkey combo")
                    .map(Shortcut::Combo)
                    .map(Some)
            }
        }
    }
}

impl From<Option<Shortcut>> for wire::Shortcut {
    fn from(hotkey: Option<Shortcut>) -> Self {
        use wire::shortcut::Binding;
        Self {
            binding: hotkey.map(|hotkey| match hotkey {
                Shortcut::Chord(chord) => Binding::Chord(chord.into()),
                Shortcut::Combo(combo) => Binding::Combo(wire::ShortcutCombo::from(combo).into()),
            }),
        }
    }
}

impl TryFrom<wire::Prompts> for Prompts {
    type Error = ProtocolError;

    fn try_from(prompts: wire::Prompts) -> Result<Self, Self::Error> {
        let entries = prompts
            .entries
            .into_iter()
            .map(Prompt::try_from)
            .collect::<Result<_, _>>()?;
        Ok(Self::new(entries)?)
    }
}

impl From<&Prompts> for wire::Prompts {
    fn from(prompts: &Prompts) -> Self {
        Self {
            entries: prompts.entries().iter().map(wire::Prompt::from).collect(),
        }
    }
}

impl TryFrom<wire::Prompt> for Prompt {
    type Error = ProtocolError;

    fn try_from(prompt: wire::Prompt) -> Result<Self, Self::Error> {
        let chord = prompt.chord.map(Chord::try_from).transpose()?;
        Ok(Self::new(
            PromptId::new(prompt.id)?,
            prompt.name,
            decode_enum::<wire::PromptKind>(prompt.kind, "prompt kind")?,
            prompt.file.into(),
            prompt.enabled,
            chord,
        )?)
    }
}

impl From<&Prompt> for wire::Prompt {
    fn from(prompt: &Prompt) -> Self {
        Self {
            id: prompt.id().as_str().to_owned(),
            name: prompt.name().to_owned(),
            kind: wire::PromptKind::from(prompt.kind()) as i32,
            file: prompt.file().to_string_lossy().into_owned(),
            enabled: prompt.enabled(),
            chord: prompt.chord().map(wire::Chord::from),
        }
    }
}

impl From<PromptKind> for wire::PromptKind {
    fn from(kind: PromptKind) -> Self {
        match kind {
            PromptKind::Rewrite => Self::Rewrite,
            PromptKind::Compose => Self::Compose,
        }
    }
}

impl WireEnum for wire::PromptKind {
    type Domain = PromptKind;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::Rewrite => Some(PromptKind::Rewrite),
            Self::Compose => Some(PromptKind::Compose),
            Self::Unspecified => None,
        }
    }
}

fn decode_enum<E>(value: i32, field: &'static str) -> Result<E::Domain, ProtocolError>
where
    E: WireEnum,
{
    E::try_from(value)
        .ok()
        .and_then(WireEnum::into_domain)
        .ok_or(ProtocolError::InvalidEnum { field, value })
}

/// Unspecified is the honest code for a press that typed nothing.
fn delivery_outcome_code(delivered: Option<DeliveryOutcome>) -> wire::DeliveryOutcome {
    match delivered {
        None => wire::DeliveryOutcome::Unspecified,
        Some(DeliveryOutcome::Inserted) => wire::DeliveryOutcome::Inserted,
        Some(DeliveryOutcome::Copied) => wire::DeliveryOutcome::Copied,
        Some(DeliveryOutcome::InHistory) => wire::DeliveryOutcome::InHistory,
        Some(DeliveryOutcome::NotTyped) => wire::DeliveryOutcome::NotTyped,
    }
}

/// An unknown code reads as no outcome: a newer daemon may add one, and a
/// pill that says nothing beats one that fails the whole event.
fn delivery_outcome(code: i32) -> Option<DeliveryOutcome> {
    match wire::DeliveryOutcome::try_from(code) {
        Ok(wire::DeliveryOutcome::Inserted) => Some(DeliveryOutcome::Inserted),
        Ok(wire::DeliveryOutcome::Copied) => Some(DeliveryOutcome::Copied),
        Ok(wire::DeliveryOutcome::InHistory) => Some(DeliveryOutcome::InHistory),
        Ok(wire::DeliveryOutcome::NotTyped) => Some(DeliveryOutcome::NotTyped),
        Ok(wire::DeliveryOutcome::Unspecified) | Err(..) => None,
    }
}

impl From<DictationStage> for wire::DictationEvent {
    fn from(stage: DictationStage) -> Self {
        let mut text = String::new();
        let mut recording = 0;
        let mut outcome = wire::DeliveryOutcome::Unspecified;
        let mut words = 0;
        let mut speaker = String::new();
        let mut heard = wire::HeardSpeaker::Unspecified;
        let mut delivery_mode = wire::DeliveryMode::Unspecified;
        let (code, error) = match stage {
            DictationStage::Recording { id, delivery } => {
                recording = id.0;
                delivery_mode = wire::DeliveryMode::from(delivery);
                (wire::DictationStage::Recording, 0)
            }
            DictationStage::Transcribing => (wire::DictationStage::Transcribing, 0),
            DictationStage::Delivering => (wire::DictationStage::Delivering, 0),
            DictationStage::Delivered {
                outcome: delivered,
                words: count,
            } => {
                outcome = delivery_outcome_code(delivered);
                words = count;
                (wire::DictationStage::Delivered, 0)
            }
            DictationStage::Cancelled => (wire::DictationStage::Cancelled, 0),
            DictationStage::Failed(code) => {
                (wire::DictationStage::Failed, error_code_to_wire(code))
            }
            DictationStage::SegmentTranscribed {
                text: segment,
                speaker: heard_in,
            } => {
                text = segment.to_string();
                speaker = wire_speaker_name(heard_in);
                (wire::DictationStage::SegmentTranscribed, 0)
            }
            DictationStage::SegmentDelivered => (wire::DictationStage::SegmentDelivered, 0),
            DictationStage::SegmentQueued => (wire::DictationStage::SegmentQueued, 0),
            DictationStage::ReturnDropped => (wire::DictationStage::ReturnDropped, 0),
            DictationStage::SpeakerChanged(voice) => {
                heard = match voice {
                    HeardSpeaker::Undecided => wire::HeardSpeaker::Undecided,
                    HeardSpeaker::Typed(name) => {
                        speaker = wire_speaker_name(name);
                        wire::HeardSpeaker::Typed
                    }
                    HeardSpeaker::KeptOut => wire::HeardSpeaker::KeptOut,
                };
                (wire::DictationStage::SpeakerChanged, 0)
            }
        };
        Self {
            stage: code as i32,
            text,
            error_code: error,
            recording_id: recording,
            outcome: outcome as i32,
            word_count: words,
            speaker_name: speaker,
            heard: heard as i32,
            delivery_mode: delivery_mode as i32,
        }
    }
}

impl TryFrom<&wire::DictationEvent> for DictationStage {
    type Error = ProtocolError;

    fn try_from(event: &wire::DictationEvent) -> Result<Self, Self::Error> {
        match wire::DictationStage::try_from(event.stage) {
            Ok(wire::DictationStage::Recording) => Ok(Self::Recording {
                id: RecordingId(event.recording_id),
                delivery: decode_enum::<wire::DeliveryMode>(event.delivery_mode, "delivery mode")?,
            }),
            Ok(wire::DictationStage::Transcribing) => Ok(Self::Transcribing),
            Ok(wire::DictationStage::Delivering) => Ok(Self::Delivering),
            Ok(wire::DictationStage::SegmentTranscribed) => Ok(Self::SegmentTranscribed {
                text: Arc::from(event.text.as_str()),
                speaker: optional_speaker_name(&event.speaker_name)?,
            }),
            Ok(wire::DictationStage::SegmentDelivered) => Ok(Self::SegmentDelivered),
            Ok(wire::DictationStage::SegmentQueued) => Ok(Self::SegmentQueued),
            Ok(wire::DictationStage::ReturnDropped) => Ok(Self::ReturnDropped),
            Ok(wire::DictationStage::SpeakerChanged) => {
                match wire::HeardSpeaker::try_from(event.heard) {
                    Ok(wire::HeardSpeaker::Undecided) => {
                        Ok(Self::SpeakerChanged(HeardSpeaker::Undecided))
                    }
                    Ok(wire::HeardSpeaker::Typed) => Ok(Self::SpeakerChanged(HeardSpeaker::Typed(
                        optional_speaker_name(&event.speaker_name)?,
                    ))),
                    Ok(wire::HeardSpeaker::KeptOut) => {
                        Ok(Self::SpeakerChanged(HeardSpeaker::KeptOut))
                    }
                    Ok(wire::HeardSpeaker::Unspecified) | Err(..) => {
                        Err(ProtocolError::InvalidEnum {
                            field: "heard",
                            value: event.heard,
                        })
                    }
                }
            }
            Ok(wire::DictationStage::Delivered) => Ok(Self::Delivered {
                outcome: delivery_outcome(event.outcome),
                words: event.word_count,
            }),
            Ok(wire::DictationStage::Cancelled) => Ok(Self::Cancelled),
            // 0 is rejected rather than kept as Unknown(0): unknown codes
            // must round-trip, but UNSPECIFIED must never re-enter the wire.
            Ok(wire::DictationStage::Failed) => match error_code_from_wire(event.error_code) {
                Ok(code) => Ok(Self::Failed(code)),
                Err(error) => Err(error),
            },
            Ok(wire::DictationStage::Unspecified) | Err(..) => Err(ProtocolError::InvalidEnum {
                field: "stage",
                value: event.stage,
            }),
        }
    }
}

pub(crate) fn error_code_to_wire(code: RpcErrorCode) -> i32 {
    let encoded = match code {
        RpcErrorCode::InvalidRequest => wire::ErrorCode::InvalidRequest,
        RpcErrorCode::Busy => wire::ErrorCode::Busy,
        RpcErrorCode::Internal => wire::ErrorCode::Internal,
        RpcErrorCode::Cancelled => wire::ErrorCode::Cancelled,
        RpcErrorCode::Unauthenticated => wire::ErrorCode::Unauthenticated,
        RpcErrorCode::Unsupported => wire::ErrorCode::Unsupported,
        RpcErrorCode::ShuttingDown => wire::ErrorCode::ShuttingDown,
        RpcErrorCode::PayloadTooLarge => wire::ErrorCode::PayloadTooLarge,
        RpcErrorCode::PermissionDenied => wire::ErrorCode::PermissionDenied,
        RpcErrorCode::DurabilityUnconfirmed => wire::ErrorCode::DurabilityUnconfirmed,
        RpcErrorCode::SettingsInvalid => wire::ErrorCode::SettingsInvalid,
        RpcErrorCode::OverallTimeout => wire::ErrorCode::OverallTimeout,
        RpcErrorCode::RecordingTimeout => wire::ErrorCode::RecordingTimeout,
        RpcErrorCode::DeliveryTimeout => wire::ErrorCode::DeliveryTimeout,
        RpcErrorCode::CaptureFailed => wire::ErrorCode::CaptureFailed,
        RpcErrorCode::NoSpeech => wire::ErrorCode::NoSpeech,
        RpcErrorCode::TranscriptionFailed => wire::ErrorCode::TranscriptionFailed,
        RpcErrorCode::DeliveryFailed => wire::ErrorCode::DeliveryFailed,
        RpcErrorCode::DeliveryTargetChanged => wire::ErrorCode::DeliveryTargetChanged,
        RpcErrorCode::DeliveryInterrupted => wire::ErrorCode::DeliveryInterrupted,
        RpcErrorCode::ApplicationExcluded => wire::ErrorCode::ApplicationExcluded,
        RpcErrorCode::SynthesisFailed => wire::ErrorCode::SynthesisFailed,
        RpcErrorCode::PlaybackFailed => wire::ErrorCode::PlaybackFailed,
        RpcErrorCode::SpeechTimeout => wire::ErrorCode::SpeechTimeout,
        RpcErrorCode::VoiceUnavailable => wire::ErrorCode::VoiceUnavailable,
        RpcErrorCode::AssistantUnavailable => wire::ErrorCode::AssistantUnavailable,
        RpcErrorCode::AssistantFailed => wire::ErrorCode::AssistantFailed,
        RpcErrorCode::AssistantTimeout => wire::ErrorCode::AssistantTimeout,
        RpcErrorCode::AssistantSignedOut => wire::ErrorCode::AssistantSignedOut,
        RpcErrorCode::PromptUnreadable => wire::ErrorCode::PromptUnreadable,
        RpcErrorCode::PromptMissing => wire::ErrorCode::PromptMissing,
        RpcErrorCode::PromptsFull => wire::ErrorCode::PromptsFull,
        RpcErrorCode::PromptWriteFailed => wire::ErrorCode::PromptWriteFailed,
        RpcErrorCode::ChatNameTaken => wire::ErrorCode::ChatNameTaken,
        RpcErrorCode::SpeakerModelMissing => wire::ErrorCode::SpeakerModelMissing,
        RpcErrorCode::SpeakerAlreadyEnrolled => wire::ErrorCode::SpeakerAlreadyEnrolled,
        RpcErrorCode::SpeakerNotEnrolled => wire::ErrorCode::SpeakerNotEnrolled,
        RpcErrorCode::SpeakerInconsistentSamples => wire::ErrorCode::SpeakerInconsistentSamples,
        RpcErrorCode::SpeakerProfileWriteFailed => wire::ErrorCode::SpeakerProfileWriteFailed,
        RpcErrorCode::SpeakerNoSpeech => wire::ErrorCode::SpeakerNoSpeech,
        RpcErrorCode::SystemTranscriptionUnsupportedOs => {
            wire::ErrorCode::SystemTranscriptionUnsupportedOs
        }
        RpcErrorCode::SystemTranscriptionPermissionDenied => {
            wire::ErrorCode::SystemTranscriptionPermissionDenied
        }
        RpcErrorCode::SystemTranscriptionCaptureFailed => {
            wire::ErrorCode::SystemTranscriptionCaptureFailed
        }
        RpcErrorCode::HistoryMissing => wire::ErrorCode::HistoryMissing,
        RpcErrorCode::HistoryAudioUnavailable => wire::ErrorCode::HistoryAudioUnavailable,
        RpcErrorCode::HistoryUnavailable => wire::ErrorCode::HistoryUnavailable,
        RpcErrorCode::HistoryProjectUnavailable => wire::ErrorCode::HistoryProjectUnavailable,
        RpcErrorCode::HistoryEntryEmpty => wire::ErrorCode::HistoryEntryEmpty,
        RpcErrorCode::HistoryTranscriptUnreadable => wire::ErrorCode::HistoryTranscriptUnreadable,
        RpcErrorCode::Unknown(value) => return value,
    };
    encoded as i32
}

pub(crate) fn error_code_from_wire(code: i32) -> Result<RpcErrorCode, ProtocolError> {
    let decoded = match wire::ErrorCode::try_from(code) {
        Ok(wire::ErrorCode::InvalidRequest) => RpcErrorCode::InvalidRequest,
        Ok(wire::ErrorCode::Busy) => RpcErrorCode::Busy,
        Ok(wire::ErrorCode::Internal) => RpcErrorCode::Internal,
        Ok(wire::ErrorCode::Cancelled) => RpcErrorCode::Cancelled,
        Ok(wire::ErrorCode::Unauthenticated) => RpcErrorCode::Unauthenticated,
        Ok(wire::ErrorCode::Unsupported) => RpcErrorCode::Unsupported,
        Ok(wire::ErrorCode::ShuttingDown) => RpcErrorCode::ShuttingDown,
        Ok(wire::ErrorCode::PayloadTooLarge) => RpcErrorCode::PayloadTooLarge,
        Ok(wire::ErrorCode::PermissionDenied) => RpcErrorCode::PermissionDenied,
        Ok(wire::ErrorCode::DurabilityUnconfirmed) => RpcErrorCode::DurabilityUnconfirmed,
        Ok(wire::ErrorCode::SettingsInvalid) => RpcErrorCode::SettingsInvalid,
        Ok(wire::ErrorCode::OverallTimeout) => RpcErrorCode::OverallTimeout,
        Ok(wire::ErrorCode::RecordingTimeout) => RpcErrorCode::RecordingTimeout,
        Ok(wire::ErrorCode::DeliveryTimeout) => RpcErrorCode::DeliveryTimeout,
        Ok(wire::ErrorCode::CaptureFailed) => RpcErrorCode::CaptureFailed,
        Ok(wire::ErrorCode::NoSpeech) => RpcErrorCode::NoSpeech,
        Ok(wire::ErrorCode::TranscriptionFailed) => RpcErrorCode::TranscriptionFailed,
        Ok(wire::ErrorCode::DeliveryFailed) => RpcErrorCode::DeliveryFailed,
        Ok(wire::ErrorCode::DeliveryTargetChanged) => RpcErrorCode::DeliveryTargetChanged,
        Ok(wire::ErrorCode::DeliveryInterrupted) => RpcErrorCode::DeliveryInterrupted,
        Ok(wire::ErrorCode::ApplicationExcluded) => RpcErrorCode::ApplicationExcluded,
        Ok(wire::ErrorCode::SynthesisFailed) => RpcErrorCode::SynthesisFailed,
        Ok(wire::ErrorCode::PlaybackFailed) => RpcErrorCode::PlaybackFailed,
        Ok(wire::ErrorCode::SpeechTimeout) => RpcErrorCode::SpeechTimeout,
        Ok(wire::ErrorCode::VoiceUnavailable) => RpcErrorCode::VoiceUnavailable,
        Ok(wire::ErrorCode::AssistantUnavailable) => RpcErrorCode::AssistantUnavailable,
        Ok(wire::ErrorCode::AssistantFailed) => RpcErrorCode::AssistantFailed,
        Ok(wire::ErrorCode::AssistantTimeout) => RpcErrorCode::AssistantTimeout,
        Ok(wire::ErrorCode::AssistantSignedOut) => RpcErrorCode::AssistantSignedOut,
        Ok(wire::ErrorCode::PromptUnreadable) => RpcErrorCode::PromptUnreadable,
        Ok(wire::ErrorCode::PromptMissing) => RpcErrorCode::PromptMissing,
        Ok(wire::ErrorCode::PromptsFull) => RpcErrorCode::PromptsFull,
        Ok(wire::ErrorCode::PromptWriteFailed) => RpcErrorCode::PromptWriteFailed,
        Ok(wire::ErrorCode::ChatNameTaken) => RpcErrorCode::ChatNameTaken,
        Ok(wire::ErrorCode::SpeakerModelMissing) => RpcErrorCode::SpeakerModelMissing,
        Ok(wire::ErrorCode::SpeakerAlreadyEnrolled) => RpcErrorCode::SpeakerAlreadyEnrolled,
        Ok(wire::ErrorCode::SpeakerNotEnrolled) => RpcErrorCode::SpeakerNotEnrolled,
        Ok(wire::ErrorCode::SpeakerInconsistentSamples) => RpcErrorCode::SpeakerInconsistentSamples,
        Ok(wire::ErrorCode::SpeakerProfileWriteFailed) => RpcErrorCode::SpeakerProfileWriteFailed,
        Ok(wire::ErrorCode::SpeakerNoSpeech) => RpcErrorCode::SpeakerNoSpeech,
        Ok(wire::ErrorCode::SystemTranscriptionUnsupportedOs) => {
            RpcErrorCode::SystemTranscriptionUnsupportedOs
        }
        Ok(wire::ErrorCode::SystemTranscriptionPermissionDenied) => {
            RpcErrorCode::SystemTranscriptionPermissionDenied
        }
        Ok(wire::ErrorCode::SystemTranscriptionCaptureFailed) => {
            RpcErrorCode::SystemTranscriptionCaptureFailed
        }
        Ok(wire::ErrorCode::HistoryMissing) => RpcErrorCode::HistoryMissing,
        Ok(wire::ErrorCode::HistoryAudioUnavailable) => RpcErrorCode::HistoryAudioUnavailable,
        Ok(wire::ErrorCode::HistoryUnavailable) => RpcErrorCode::HistoryUnavailable,
        Ok(wire::ErrorCode::HistoryProjectUnavailable) => RpcErrorCode::HistoryProjectUnavailable,
        Ok(wire::ErrorCode::HistoryEntryEmpty) => RpcErrorCode::HistoryEntryEmpty,
        Ok(wire::ErrorCode::HistoryTranscriptUnreadable) => {
            RpcErrorCode::HistoryTranscriptUnreadable
        }
        Ok(wire::ErrorCode::Unspecified) => {
            return Err(ProtocolError::InvalidEnum {
                field: "error code",
                value: code,
            });
        }
        Err(_unknown) => RpcErrorCode::Unknown(code),
    };
    Ok(decoded)
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid assistant model ID")]
    InvalidModelId,
    #[error("duplicate assistant model setting")]
    DuplicateAssistantModel,
    #[error("invalid inference message")]
    InvalidInference,
    #[error("invalid history {0}")]
    InvalidHistory(&'static str),
    #[error("invalid vocabulary term count {0}")]
    InvalidVocabularyTerms(u64),
    #[error(transparent)]
    InvalidVocabularyDir(#[from] aria_settings::DirectoryError),
    #[error("invalid {field} value {value}")]
    InvalidEnum { field: &'static str, value: i32 },

    #[error("the transcript path must be absolute and contain no NUL bytes")]
    InvalidTranscriptPath,
    #[error("the assistant working directory must be an absolute path")]
    RelativeWorkdir,

    #[error("missing {0}")]
    MissingBody(&'static str),

    #[error("{0} is out of range")]
    OutOfRange(&'static str),

    #[error("the prompt must not be empty")]
    EmptyPrompt,

    #[error(transparent)]
    InvalidInstructions(#[from] aria_assistant::chat::InstructionsTextError),

    #[error("settings update is missing its patch")]
    MissingSettingsPatch,

    #[error("the settings snapshot is missing its settings")]
    MissingSettings,

    #[error("the speaker name is not usable")]
    SpeakerName {
        #[source]
        source: SpeakerNameError,
    },

    #[error(transparent)]
    InvalidLanguage(#[from] LanguageCodeError),

    #[error(transparent)]
    InvalidSilenceDuration(#[from] SilenceDurationError),

    #[error(transparent)]
    InvalidAutoStop(#[from] AutoStopError),

    #[error(transparent)]
    InvalidSpeechText(#[from] SpeechTextError),

    #[error(transparent)]
    InvalidVoice(#[from] VoiceError),

    #[error(transparent)]
    InvalidSpeakingRate(SpeakingRateError),

    #[error(transparent)]
    InvalidChord(#[from] ChordError),

    #[error(transparent)]
    InvalidApplicationOverrides(#[from] OverrideError),

    #[error("chord key code {0} is not a key code")]
    ChordKeyCode(u32),

    #[error(transparent)]
    InvalidPrompt(#[from] PromptError),

    #[error(transparent)]
    InvalidChatName(#[from] ChatNameError),
}

#[cfg(test)]
mod tests {
    use aria_voice::dictation::DictationError;

    use super::*;

    #[test]
    fn an_oversized_failure_message_is_bounded_and_keeps_its_verdict() {
        let long = "é".repeat(MAX_FAILURE_MESSAGE_BYTES);
        let status = failure_status(RpcFailure::new(RpcErrorCode::SettingsInvalid, long));
        assert!(status.message().len() <= MAX_FAILURE_MESSAGE_BYTES);
        let failure = rpc_failure(&status).expect("the verdict survives");
        assert_eq!(failure.code, RpcErrorCode::SettingsInvalid);
        assert!(failure.message.len() <= MAX_FAILURE_MESSAGE_BYTES);
        assert!(failure.message.ends_with('…'));
    }

    #[test]
    fn a_snapshot_names_the_chosen_assistant() {
        let chosen = SettingsSnapshot {
            settings: Settings {
                assistant_id: Some(AssistantId::Codex),
                ..Settings::default()
            },
            ..SettingsSnapshot::default()
        };
        assert_eq!(
            wire::SettingsSnapshot::from(&chosen).assistant_name,
            "Codex"
        );
        let unchosen = SettingsSnapshot {
            settings: Settings {
                assistant_id: None,
                ..Settings::default()
            },
            ..SettingsSnapshot::default()
        };
        assert_eq!(wire::SettingsSnapshot::from(&unchosen).assistant_name, "");
    }

    #[test]
    fn an_aria_key_round_trips_and_unspecified_is_refused() {
        for key in [AriaKey::RightOption, AriaKey::Fn] {
            let snapshot = SettingsSnapshot {
                streaming_unavailable: StreamingSupport::default(),
                revision: 1,
                settings: Settings {
                    aria_key: key,
                    ..Settings::default()
                },
            };
            let decoded =
                SettingsSnapshot::try_from(wire::SettingsSnapshot::from(&snapshot)).unwrap();
            assert_eq!(decoded.settings.aria_key, key);
            let patch = SettingsPatch::try_from(wire::SettingsPatch::from(SettingsPatch {
                aria_key: Some(key),
                ..SettingsPatch::default()
            }))
            .unwrap();
            assert_eq!(patch.aria_key, Some(key));
        }
        // An older client may still send `off`'s tag.
        const OFF_TAG: i32 = 3;
        for refused in [wire::AriaKey::Unspecified as i32, OFF_TAG] {
            let mut snapshot = wire::SettingsSnapshot::from(&SettingsSnapshot::default());
            if let Some(settings) = snapshot.settings.as_mut() {
                settings.aria_key = refused;
            }
            assert!(SettingsSnapshot::try_from(snapshot).is_err(), "{refused}");
        }
    }

    #[test]
    fn a_settings_fault_keeps_its_position_on_the_wire() {
        let failure = RpcFailure::new(RpcErrorCode::SettingsInvalid, "fix settings.toml first")
            .with_settings_position(Some(Position { line: 3, column: 7 }));
        let status = failure_status(failure.clone());
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(rpc_failure(&status), Some(failure));

        let settings = SettingsStatus {
            fault: Some(SettingsFault {
                message: "unknown field `bogus`".to_owned(),
                position: Some(Position { line: 2, column: 1 }),
            }),
            restart_required: vec!["phone.port".to_owned()],
            reverted: vec!["memory.enabled".to_owned()],
            revision: 7,
        };
        assert_eq!(
            SettingsStatus::from(wire::SettingsStatus::from(&settings)),
            settings
        );
        let update = UpdateResult::Updated {
            snapshot: SettingsSnapshot::default(),
            restart_required: vec!["tts.backend".to_owned()],
        };
        assert_eq!(
            UpdateResult::try_from(wire::UpdateSettingsResponse::from(update.clone())).unwrap(),
            update
        );
    }

    #[test]
    fn model_list_requires_a_known_assistant() {
        assert_eq!(
            AssistantId::try_from(wire::ListAssistantModelsRequest {
                assistant_id: wire::AssistantId::Codex as i32
            })
            .unwrap(),
            AssistantId::Codex
        );
        for assistant_id in [0, 99] {
            assert!(
                AssistantId::try_from(wire::ListAssistantModelsRequest { assistant_id }).is_err()
            );
        }
    }

    #[test]
    fn assistant_models_preserve_resets_and_reject_invalid_wire_values() {
        for model_id in [
            None,
            Some(String::new()),
            Some("provider/model:revision".to_owned()),
        ] {
            let patch = SettingsPatch {
                assistant_models: model_id
                    .map(|id| (AssistantId::Codex, id))
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            assert_eq!(
                SettingsPatch::try_from(wire::SettingsPatch::from(patch.clone())).unwrap(),
                patch
            );
        }
        let snapshot = SettingsSnapshot {
            streaming_unavailable: StreamingSupport::default(),
            settings: Settings {
                assistant_models: [
                    (AssistantId::ClaudeCode, "claude-example".to_owned()),
                    (AssistantId::Codex, "codex-example".to_owned()),
                ]
                .into(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            SettingsSnapshot::try_from(wire::SettingsSnapshot::from(&snapshot)).unwrap(),
            snapshot
        );
        let valid = wire::AssistantModelSetting {
            assistant_id: wire::AssistantId::Codex as i32,
            model_id: "example".to_owned(),
        };
        for entries in [
            vec![wire::AssistantModelSetting {
                model_id: "--bad".to_owned(),
                ..valid.clone()
            }],
            vec![wire::AssistantModelSetting {
                assistant_id: 0,
                ..valid.clone()
            }],
            vec![wire::AssistantModelSetting {
                assistant_id: 99,
                ..valid.clone()
            }],
            vec![valid.clone(), valid],
        ] {
            assert!(
                SettingsPatch::try_from(wire::SettingsPatch {
                    assistant_models: entries.clone(),
                    ..Default::default()
                })
                .is_err()
            );
            let mut wire = wire::SettingsSnapshot::from(&snapshot);
            wire.settings.as_mut().unwrap().assistant_models = entries;
            assert!(SettingsSnapshot::try_from(wire).is_err());
        }
    }

    #[test]
    fn chat_assistant_settings_preserve_presence_and_reject_unknown_values() {
        for assistant_id in [
            None,
            Some(AssistantId::ClaudeCode),
            Some(AssistantId::Codex),
        ] {
            let patch = SettingsPatch {
                assistant_id,
                ..Default::default()
            };
            assert_eq!(
                SettingsPatch::try_from(wire::SettingsPatch::from(patch.clone())).unwrap(),
                patch
            );
            let snapshot = SettingsSnapshot {
                streaming_unavailable: StreamingSupport::default(),
                settings: Settings {
                    assistant_id,
                    ..Default::default()
                },
                ..Default::default()
            };
            assert_eq!(
                SettingsSnapshot::try_from(wire::SettingsSnapshot::from(&snapshot)).unwrap(),
                snapshot
            );
        }
        for value in [0, 99] {
            assert!(
                SettingsPatch::try_from(wire::SettingsPatch {
                    assistant_id: Some(value),
                    ..Default::default()
                })
                .is_err()
            );
            let mut snapshot = wire::SettingsSnapshot::from(&SettingsSnapshot::default());
            snapshot.settings.as_mut().unwrap().assistant_id = Some(value);
            assert!(SettingsSnapshot::try_from(snapshot).is_err());
        }
    }

    #[test]
    fn output_modes_keep_their_wire_numbers() {
        for (mode, number) in [
            (OutputMode::Insert, 1),
            (OutputMode::Copy, 2),
            (OutputMode::InsertOrCopy, 3),
            (OutputMode::InsertAndSubmit, 4),
        ] {
            assert_eq!(wire::OutputMode::from(mode) as i32, number);
            assert_eq!(
                decode_enum::<wire::OutputMode>(number, "output mode").unwrap(),
                mode
            );
        }
        assert!(decode_enum::<wire::OutputMode>(0, "output mode").is_err());
    }

    fn error_code_label(code: i32) -> &'static str {
        error_code_from_wire(code).map_or("unspecified", |code| code.label())
    }

    #[test]
    fn a_dictation_failure_keeps_its_code_and_its_name_across_the_wire() {
        let failures = [
            DictationError::Busy,
            DictationError::RecordingAlreadyStarted,
            DictationError::Cancelled,
            DictationError::OverallTimeout(Duration::ZERO),
            DictationError::RecordingTimeout(Duration::ZERO),
            DictationError::DeliveryTimeout(Duration::ZERO),
            DictationError::DeliveryTargetChanged,
            DictationError::DeliveryInterrupted,
            DictationError::ApplicationExcluded,
            DictationError::NoSpeech,
        ];

        for failure in &failures {
            let sent = error_code_to_wire(RpcErrorCode::from(failure));

            assert_eq!(
                error_code_from_wire(sent).ok(),
                Some(failure.into()),
                "{failure}"
            );
            assert_eq!(error_code_label(sent), failure.status(), "{failure}");
        }
    }

    #[test]
    fn a_failure_round_trips_through_the_status_details() {
        let failure = RpcFailure::new(RpcErrorCode::NoSpeech, "no speech was detected");

        let status = failure_status(failure.clone());

        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
        assert_eq!(status.message(), failure.message);
        assert_eq!(rpc_failure(&status), Some(failure));
    }

    #[test]
    fn a_status_without_details_carries_no_aria_failure() {
        let status = Status::unavailable("connection refused");

        assert_eq!(rpc_failure(&status), None);
    }

    #[test]
    fn a_system_transcription_failure_crosses_as_its_message_and_no_failure_as_none() {
        let failed = SystemTranscriptionSnapshot::from(
            wire::GetSystemTranscriptionStatusResponse::from(SystemTranscriptionSnapshot {
                running: false,
                transcript: "/tmp/transcript.txt".into(),
                error: Some("screen recording is not allowed".to_owned()),
            }),
        );
        let running =
            wire::GetSystemTranscriptionStatusResponse::from(SystemTranscriptionSnapshot {
                running: true,
                transcript: "/tmp/transcript.txt".into(),
                error: None,
            });

        assert_eq!(
            failed.error.as_deref(),
            Some("screen recording is not allowed")
        );
        assert_eq!(running.error_message, "");
        assert_eq!(SystemTranscriptionSnapshot::from(running).error, None);
    }

    #[test]
    fn an_invalid_assistant_model_is_refused() {
        let invalid = wire::ListAssistantModelsResponse {
            models: vec![wire::AssistantModel {
                id: String::new(),
                name: "Nameless".to_owned(),
            }],
        };

        assert!(matches!(
            Vec::<AssistantModel>::try_from(invalid),
            Err(ProtocolError::InvalidModelId)
        ));
    }

    #[test]
    fn an_empty_settings_status_answer_is_refused() {
        assert!(matches!(
            SettingsStatus::try_from(wire::GetSettingsStatusResponse { status: None }),
            Err(ProtocolError::MissingBody("settings status"))
        ));
    }

    #[test]
    fn a_transport_category_is_the_inverse_of_its_grpc_code() {
        for (code, transport) in [
            (RpcErrorCode::ShuttingDown, TransportFailure::Unavailable),
            (RpcErrorCode::Unsupported, TransportFailure::Unimplemented),
            (
                RpcErrorCode::OverallTimeout,
                TransportFailure::DeadlineExceeded,
            ),
            (RpcErrorCode::Cancelled, TransportFailure::Cancelled),
            (RpcErrorCode::Internal, TransportFailure::Other),
            (RpcErrorCode::InvalidRequest, TransportFailure::Other),
        ] {
            assert_eq!(transport_failure(grpc_code(code)), transport, "{code}");
        }
    }

    #[test]
    fn the_zero_code_names_no_failure() {
        let unspecified = wire::ErrorCode::Unspecified as i32;

        assert!(error_code_from_wire(unspecified).is_err());
        assert_eq!(error_code_label(unspecified), "unspecified");
    }

    #[test]
    fn caption_snapshots_use_a_separate_payload_and_redact_debug_text() {
        for ended in [false, true] {
            let caption = CaptionSnapshot {
                recording: RecordingId(42),
                finalized: Arc::from("private finalized words"),
                provisional: Arc::from("private provisional words"),
                ended,
            };
            let debug = format!("{caption:?}");
            assert!(!debug.contains("private"));
            let encoded = wire::Event::from(DictationEvent::Caption(caption)).encode_to_vec();
            let decoded = wire::Event::decode(encoded.as_slice()).expect("event decodes");
            let Some(wire::event::Payload::Caption(snapshot)) = decoded.payload else {
                panic!("caption must not become a stage");
            };
            assert_eq!(snapshot.recording_id, 42);
            assert_eq!(snapshot.finalized, "private finalized words");
            assert_eq!(snapshot.provisional, "private provisional words");
            assert_eq!(snapshot.ended, ended);
        }
    }

    #[test]
    fn ghost_snapshots_carry_their_placement_and_redact_debug_text() {
        let ghost = GhostSnapshot {
            recording: RecordingId(42),
            text: Arc::from("private unconfirmed words"),
            placement: Some(Placement {
                caret: Rect {
                    x: 10.0,
                    y: 20.0,
                    width: 1.0,
                    height: 16.0,
                },
                field: None,
                font: Some(aria_platform::delivery::Font {
                    name: "Helvetica".to_owned(),
                    size: 13.0,
                }),
            }),
        };
        assert!(!format!("{ghost:?}").contains("private"));
        let encoded = wire::Event::from(DictationEvent::Ghost(ghost)).encode_to_vec();
        let decoded = wire::Event::decode(encoded.as_slice()).expect("event decodes");
        let Some(wire::event::Payload::Ghost(snapshot)) = decoded.payload else {
            panic!("a ghost keeps its own payload");
        };
        assert_eq!(snapshot.text, "private unconfirmed words");
        let placement = snapshot.placement.expect("placement survives");
        assert_eq!(placement.caret.expect("caret survives").height, 16.0);
        assert_eq!(placement.field, None);
        assert_eq!(placement.font_name, "Helvetica");
        assert_eq!(placement.font_size, 13.0);
    }

    #[test]
    fn every_dictation_stage_survives_the_round_trip() {
        let stages = [
            DictationStage::Recording {
                id: RecordingId(7),
                delivery: DeliveryMode::Complete,
            },
            DictationStage::Transcribing,
            DictationStage::Delivering,
            DictationStage::Delivered {
                outcome: None,
                words: 0,
            },
            DictationStage::Delivered {
                outcome: Some(DeliveryOutcome::Inserted),
                words: 14,
            },
            DictationStage::Delivered {
                outcome: Some(DeliveryOutcome::Copied),
                words: 1,
            },
            DictationStage::Delivered {
                outcome: Some(DeliveryOutcome::InHistory),
                words: 3,
            },
            DictationStage::Delivered {
                outcome: Some(DeliveryOutcome::NotTyped),
                words: 3,
            },
            DictationStage::Cancelled,
            DictationStage::Failed(RpcErrorCode::NoSpeech),
            DictationStage::Failed(RpcErrorCode::Unknown(9_999)),
            DictationStage::SegmentTranscribed {
                text: Arc::from("a segment"),
                speaker: None,
            },
            DictationStage::SegmentTranscribed {
                text: Arc::from("a segment"),
                speaker: Some(SpeakerName::new("ada").expect("a valid name")),
            },
            DictationStage::SegmentDelivered,
            DictationStage::SegmentQueued,
            DictationStage::SpeakerChanged(HeardSpeaker::Undecided),
            DictationStage::SpeakerChanged(HeardSpeaker::Typed(None)),
            DictationStage::SpeakerChanged(HeardSpeaker::Typed(Some(
                SpeakerName::new("ada").expect("a valid name"),
            ))),
            DictationStage::SpeakerChanged(HeardSpeaker::KeptOut),
        ];

        for stage in stages {
            let sent = wire::DictationEvent::from(stage.clone());

            assert_eq!(
                DictationStage::try_from(&sent).expect("the stage should decode"),
                stage
            );
        }
    }

    #[test]
    fn an_unspecified_stage_is_refused() {
        let event = wire::DictationEvent {
            stage: 0,
            ..Default::default()
        };

        assert!(DictationStage::try_from(&event).is_err());
        let event = wire::SpeechEvent {
            stage: 0,
            error_code: 0,
        };
        assert!(SpeechStage::try_from(&event).is_err());
    }

    #[test]
    fn every_speech_stage_survives_the_round_trip() {
        let stages = [
            SpeechStage::Synthesizing,
            SpeechStage::Speaking,
            SpeechStage::Spoken,
            SpeechStage::Stopped,
            SpeechStage::Cancelled,
            SpeechStage::Failed(RpcErrorCode::SynthesisFailed),
            SpeechStage::Failed(RpcErrorCode::Unknown(9_999)),
        ];

        for stage in stages {
            let sent = wire::SpeechEvent::from(stage);

            assert_eq!(
                SpeechStage::try_from(&sent).expect("the stage should decode"),
                stage
            );
        }
    }

    #[test]
    fn every_chat_stage_survives_the_round_trip() {
        let kinds = [
            ChatStageKind::Working(Step::Thinking),
            ChatStageKind::Working(Step::Skill("pair".to_owned())),
            ChatStageKind::Working(Step::Unknown(9)),
            ChatStageKind::Replied(ChatReply {
                text: "hi".to_owned(),
                delivery_error: None,
                copied: true,
                history_entry_id: Some(12),
            }),
            ChatStageKind::Replied(ChatReply {
                text: "hi".to_owned(),
                delivery_error: Some(RpcErrorCode::DeliveryTargetChanged),
                copied: false,
                history_entry_id: None,
            }),
            ChatStageKind::Failed {
                code: RpcErrorCode::AssistantUnavailable,
                message: "claude is missing".to_owned(),
            },
            ChatStageKind::Failed {
                code: RpcErrorCode::Unknown(9_999),
                message: String::new(),
            },
        ];

        for kind in kinds {
            let stage = ChatStage {
                ticket: Ticket(7),
                assistant_id: AssistantId::Codex,
                kind,
            };
            let sent = wire::ChatEvent::from(stage.clone());

            assert_eq!(
                ChatStage::try_from(&sent).expect("the stage should decode"),
                stage
            );
            assert!(matches!(
                ChatStage::try_from(&wire::ChatEvent {
                    assistant_id: 0,
                    ..sent
                }),
                Err(ProtocolError::InvalidEnum {
                    field: "assistant_id",
                    ..
                })
            ));
        }
        // An older daemon's THINKING carried no step at all.
        let stale = wire::ChatEvent {
            stage: wire::ChatStage::Working as i32,
            assistant_id: wire::AssistantId::Codex as i32,
            ticket: 7,
            ..Default::default()
        };
        assert!(matches!(
            ChatStage::try_from(&stale),
            Err(ProtocolError::InvalidEnum { field: "step", .. })
        ));
    }

    fn ask_call(chat: ChatChoice) -> AskCall {
        AskCall {
            text: "say hi".to_owned(),
            chat,
            assistant_id: AssistantId::Codex,
            model_id: Some("mini".to_owned()),
            output_mode: Some(OutputMode::Copy),
            own_voice: true,
            tap_cancels: true,
        }
    }

    /// An ask that starts a chat in `/tmp`, as the wire carries it.
    fn started_ask() -> wire::AskRequest {
        wire::AskRequest::from(ask_call(ChatChoice::Start {
            workdir: "/tmp".into(),
        }))
    }

    fn planning() -> ChatName {
        ChatName::new("planning").expect("a valid name")
    }

    #[test]
    fn each_chat_choice_round_trips() {
        for chat in [
            ChatChoice::Folder {
                workdir: "/tmp".into(),
                new_session: true,
            },
            ChatChoice::Named {
                name: planning(),
                workdir: Some("/tmp".into()),
                new_session: true,
            },
            ChatChoice::Named {
                name: planning(),
                workdir: None,
                new_session: false,
            },
            ChatChoice::Start {
                workdir: "/tmp".into(),
            },
        ] {
            let call = ask_call(chat);

            let sent = wire::AskRequest::from(call.clone());

            assert_eq!(AskCall::try_from(sent).expect("decodes"), call);
        }
    }

    #[test]
    fn an_ask_without_a_chat_is_refused() {
        assert!(matches!(
            AskCall::try_from(wire::AskRequest {
                chat: None,
                ..started_ask()
            }),
            Err(ProtocolError::MissingBody("chat"))
        ));
    }

    #[test]
    fn an_ask_without_the_folder_its_choice_needs_is_refused() {
        use wire::ask_request::Chat;
        let ask = |chat| wire::AskRequest {
            chat: Some(chat),
            ..started_ask()
        };

        for folderless in [
            Chat::Folder(wire::FolderChat::default()),
            Chat::Start(wire::StartChat::default()),
        ] {
            assert!(matches!(
                AskCall::try_from(ask(folderless)),
                Err(ProtocolError::MissingBody("assistant working directory"))
            ));
        }
    }

    #[test]
    fn a_chat_key_named_like_a_folder_is_refused() {
        let key = |name: &str| wire::ChatKey {
            assistant_id: wire::AssistantId::Codex as i32,
            name: name.to_owned(),
        };

        assert!(ChatKey::try_from(key("proj")).is_ok());
        for request in [
            ChatKey::try_from(wire::ListPendingAsksRequest {
                chat: Some(key("/Users/me/proj")),
            }),
            ChatKey::try_from(wire::CancelChatRequest {
                chat: Some(key("/Users/me/proj")),
            }),
            ChatKey::try_from(wire::DeleteChatRequest {
                chat: Some(key("/Users/me/proj")),
            }),
        ] {
            assert!(
                matches!(request, Err(ProtocolError::InvalidChatName(_))),
                "{request:?}"
            );
        }
    }

    #[test]
    fn an_ask_with_a_relative_folder_is_refused() {
        use wire::ask_request::Chat;
        let ask = |chat| wire::AskRequest {
            chat: Some(chat),
            ..started_ask()
        };

        for relative in [
            ask(Chat::Folder(wire::FolderChat {
                workdir: "proj".to_owned(),
                new_session: false,
            })),
            ask(Chat::Named(wire::NamedChat {
                name: "planning".to_owned(),
                workdir: "proj".to_owned(),
                new_session: false,
            })),
            ask(Chat::Start(wire::StartChat {
                workdir: "proj".to_owned(),
            })),
        ] {
            let decoded = AskCall::try_from(relative);
            assert!(
                matches!(decoded, Err(ProtocolError::RelativeWorkdir)),
                "{decoded:?}"
            );
        }
    }

    #[test]
    fn a_bad_ask_is_refused() {
        let bare = started_ask();
        for model_id in ["bad\0id", "two words", "--option", &"x".repeat(201)] {
            assert!(matches!(
                AskCall::try_from(wire::AskRequest {
                    model_id: model_id.to_owned(),
                    ..bare.clone()
                }),
                Err(ProtocolError::InvalidModelId)
            ));
        }
        assert!(matches!(
            AskCall::try_from(wire::AskRequest {
                text: "  ".to_owned(),
                ..bare.clone()
            }),
            Err(ProtocolError::EmptyPrompt)
        ));
        assert!(matches!(
            AskCall::try_from(wire::AskRequest {
                assistant_id: 0,
                ..bare.clone()
            }),
            Err(ProtocolError::InvalidEnum {
                field: "assistant_id",
                ..
            })
        ));
        assert_eq!(
            AskCall::try_from(wire::AskRequest {
                model_id: String::new(),
                ..bare
            })
            .expect("decodes")
            .model_id,
            None
        );
    }

    #[test]
    fn an_ask_with_a_blank_overlong_or_multiline_chat_name_is_refused() {
        let named = |name: &str| wire::AskRequest {
            chat: Some(wire::ask_request::Chat::Named(wire::NamedChat {
                name: name.to_owned(),
                ..Default::default()
            })),
            ..started_ask()
        };

        assert!(matches!(
            AskCall::try_from(named(" planning "))
                .expect("a padded name decodes")
                .chat,
            ChatChoice::Named { name, .. } if name.as_str() == "planning"
        ));
        for name in ["  ", "two\nlines", &"x".repeat(129), "/Users/me/proj"] {
            assert!(
                matches!(
                    AskCall::try_from(named(name)),
                    Err(ProtocolError::InvalidChatName(_))
                ),
                "{name:?}"
            );
        }
    }

    #[test]
    fn an_ask_response_round_trips_its_chat() {
        let chat = ChatKey {
            assistant_id: AssistantId::Codex,
            name: planning(),
        };
        for sent in [(Ticket(42), Some(chat)), (Ticket(43), None)] {
            let response = wire::AskResponse::from(sent.clone());
            assert_eq!(
                <(Ticket, Option<ChatKey>)>::try_from(response).expect("decodes"),
                sent
            );
        }
    }

    #[test]
    fn a_ticket_round_trips() {
        let sent = wire::AskResponse::from(Ticket(42));
        assert_eq!(Ticket::from(sent), Ticket(42));
        let cancel = wire::CancelAskRequest::from(Ticket(42));
        assert_eq!(Ticket::from(cancel), Ticket(42));
    }

    #[test]
    fn a_dictation_request_keeps_its_route_across_the_wire() {
        for route in [
            DictationRoute::Deliver(DictationOverrides {
                output_mode: Some(OutputMode::Copy),
                delivery_mode: Some(DeliveryMode::Complete),
            }),
            DictationRoute::Deliver(DictationOverrides::default()),
            DictationRoute::TranscriptOnly,
            DictationRoute::HistoryOnly,
        ] {
            let request = DictationRequest {
                stop_mode: StopMode::Manual,
                route,
                project: aria_voice::vocabulary::Context::Default,
            };

            let sent = wire::StartDictationRequest::from(request.clone());
            assert_eq!(
                DictationRequest::try_from(sent).expect("the request should decode"),
                request
            );
        }
    }

    #[test]
    fn a_request_without_a_route_is_refused() {
        let request = wire::StartDictationRequest {
            stop_mode: wire::StopMode::Manual as i32,
            ..Default::default()
        };

        assert!(matches!(
            DictationRequest::try_from(request),
            Err(ProtocolError::MissingBody("route"))
        ));
    }

    #[test]
    fn a_request_without_a_stop_mode_is_refused() {
        assert!(matches!(
            DictationRequest::try_from(wire::StartDictationRequest::default()),
            Err(ProtocolError::InvalidEnum {
                field: "stop mode",
                ..
            })
        ));
    }

    #[test]
    fn history_keep_audio_preserves_wire_presence() {
        for history_keep_audio in [None, Some(false), Some(true)] {
            let patch = SettingsPatch {
                history_keep_audio,
                ..Default::default()
            };
            assert_eq!(
                SettingsPatch::try_from(wire::SettingsPatch::from(patch.clone())).unwrap(),
                patch
            );
            let mut snapshot = wire::SettingsSnapshot::from(&SettingsSnapshot::default());
            snapshot.settings.as_mut().unwrap().history_keep_audio =
                history_keep_audio.unwrap_or(false);
            let decoded = SettingsSnapshot::try_from(snapshot).unwrap();
            assert_eq!(
                decoded.settings.history_keep_audio,
                history_keep_audio.unwrap_or(false)
            );
            assert_eq!(
                SettingsSnapshot::try_from(wire::SettingsSnapshot::from(&decoded)).unwrap(),
                decoded
            );
        }
    }

    #[test]
    fn mute_sound_preserves_snapshot_value_and_patch_presence_on_the_wire() {
        for mute_sound in [false, true] {
            let snapshot = SettingsSnapshot {
                streaming_unavailable: StreamingSupport::default(),
                revision: 4,
                settings: Settings {
                    mute_sound,
                    ..Default::default()
                },
            };
            let encoded = wire::SettingsSnapshot::from(&snapshot).encode_to_vec();
            let decoded = wire::SettingsSnapshot::decode(encoded.as_slice()).unwrap();
            assert_eq!(SettingsSnapshot::try_from(decoded).unwrap(), snapshot);
        }
        for mute_sound in [None, Some(false), Some(true)] {
            let patch = SettingsPatch {
                mute_sound,
                ..Default::default()
            };
            let encoded = wire::SettingsPatch::from(patch.clone()).encode_to_vec();
            let decoded = wire::SettingsPatch::decode(encoded.as_slice()).unwrap();
            assert_eq!(SettingsPatch::try_from(decoded).unwrap(), patch);
        }
    }

    #[test]
    fn return_sends_survives_the_wire() {
        for return_sends in [false, true] {
            let snapshot = SettingsSnapshot {
                streaming_unavailable: StreamingSupport::default(),
                revision: 4,
                settings: Settings {
                    return_sends,
                    ..Default::default()
                },
            };
            let encoded = wire::SettingsSnapshot::from(&snapshot).encode_to_vec();
            let decoded = wire::SettingsSnapshot::decode(encoded.as_slice()).unwrap();
            assert_eq!(SettingsSnapshot::try_from(decoded).unwrap(), snapshot);
        }
        for return_sends in [None, Some(false), Some(true)] {
            let patch = SettingsPatch {
                return_sends,
                ..Default::default()
            };
            let encoded = wire::SettingsPatch::from(patch.clone()).encode_to_vec();
            let decoded = wire::SettingsPatch::decode(encoded.as_slice()).unwrap();
            assert_eq!(SettingsPatch::try_from(decoded).unwrap(), patch);
        }
    }

    #[test]
    fn auto_stop_off_and_values_survive_the_wire() {
        let ten_seconds = Some(AutoStop::new(Duration::from_secs(10)).unwrap());
        for auto_stop in [None, ten_seconds] {
            let snapshot = SettingsSnapshot {
                streaming_unavailable: StreamingSupport::default(),
                revision: 4,
                settings: Settings {
                    auto_stop,
                    ..Default::default()
                },
            };
            let encoded = wire::SettingsSnapshot::from(&snapshot).encode_to_vec();
            let decoded = wire::SettingsSnapshot::decode(encoded.as_slice()).unwrap();
            assert_eq!(SettingsSnapshot::try_from(decoded).unwrap(), snapshot);
        }
        for auto_stop in [None, Some(None), Some(ten_seconds)] {
            let patch = SettingsPatch {
                auto_stop,
                ..Default::default()
            };
            let encoded = wire::SettingsPatch::from(patch.clone()).encode_to_vec();
            let decoded = wire::SettingsPatch::decode(encoded.as_slice()).unwrap();
            assert_eq!(SettingsPatch::try_from(decoded).unwrap(), patch);
        }
    }

    #[test]
    fn streaming_availability_crosses_the_wire_unchanged() {
        let snapshot = SettingsSnapshot {
            streaming_unavailable: StreamingSupport::of(
                aria_settings::testing::file("[stt]\nbackend = \"whisper-candle\"\n").choices(),
            ),
            revision: 3,
            settings: Settings::default(),
        };

        let wire = wire::SettingsSnapshot::from(&snapshot);

        let listed = &wire.streaming_unavailable;
        assert_eq!(listed.len(), OutputMode::ALL.len());
        assert!(listed.contains(&wire::StreamingUnavailableOutput {
            output_mode: wire::OutputMode::Copy as i32,
            reason: wire::StreamingUnavailable::Copy as i32,
        }));
        let decoded = SettingsSnapshot::try_from(wire)
            .unwrap()
            .streaming_unavailable;
        assert_eq!(decoded, snapshot.streaming_unavailable);
    }

    #[test]
    fn application_overrides_cross_the_wire_validated() {
        let overrides = ApplicationOverrides::new([(
            ApplicationId::new("com.apple.Terminal").unwrap(),
            ApplicationOverride {
                excluded: true,
                overrides: DictationOverrides {
                    output_mode: Some(OutputMode::InsertAndSubmit),
                    delivery_mode: None,
                },
            },
        )])
        .unwrap();
        let snapshot = SettingsSnapshot {
            streaming_unavailable: StreamingSupport::default(),
            revision: 2,
            settings: Settings {
                application_overrides: overrides.clone(),
                ..Default::default()
            },
        };
        let encoded = wire::SettingsSnapshot::from(&snapshot).encode_to_vec();
        let decoded = wire::SettingsSnapshot::decode(encoded.as_slice()).unwrap();
        assert_eq!(SettingsSnapshot::try_from(decoded).unwrap(), snapshot);

        for application_overrides in [None, Some(ApplicationOverrides::default()), Some(overrides)]
        {
            let patch = SettingsPatch {
                application_overrides,
                ..Default::default()
            };
            let encoded = wire::SettingsPatch::from(patch.clone()).encode_to_vec();
            let decoded = wire::SettingsPatch::decode(encoded.as_slice()).unwrap();
            assert_eq!(SettingsPatch::try_from(decoded).unwrap(), patch);
        }

        let spaced_id = wire::ApplicationOverrides {
            entries: vec![wire::ApplicationOverride {
                application_id: "com.apple Notes".into(),
                overrides: Some(wire::DictationOverrides::default()),
                excluded: false,
            }],
        };
        assert!(matches!(
            ApplicationOverrides::try_from(spaced_id),
            Err(ProtocolError::InvalidApplicationOverrides(
                OverrideError::IdCharacters
            ))
        ));
    }

    #[test]
    fn project_context_preserves_presence_and_rejects_invalid_paths() {
        use aria_settings::Directory;
        use aria_voice::vocabulary::Context;
        for project in [
            Context::Default,
            Context::Disabled,
            Context::Directory(Directory::new("/projects/aria".into()).unwrap()),
        ] {
            let request = DictationRequest {
                project,
                ..DictationRequest::new(StopMode::OnSilence)
            };
            assert_eq!(
                DictationRequest::try_from(wire::StartDictationRequest::from(request.clone()))
                    .unwrap(),
                request
            );
        }
        let disabled = wire::StartDictationRequest {
            project: Some(
                wire::start_dictation_request::Project::ProjectVocabularyDisabled(
                    wire::ProjectVocabularyDisabled {},
                ),
            ),
            stop_mode: wire::StopMode::OnSilence as i32,
            route: Some(wire::start_dictation_request::Route::Deliver(
                wire::DeliverRoute::default(),
            )),
        };
        assert_eq!(
            DictationRequest::try_from(disabled).unwrap().project,
            Context::Disabled
        );
        for path in ["relative", "/projects/../secret", "/project\0"] {
            let request = wire::StartDictationRequest {
                project: Some(wire::start_dictation_request::Project::ProjectDirectory(
                    path.into(),
                )),
                stop_mode: wire::StopMode::OnSilence as i32,
                route: Some(wire::start_dictation_request::Route::Deliver(
                    wire::DeliverRoute::default(),
                )),
            };
            assert!(DictationRequest::try_from(request).is_err());
        }
        let patch = SettingsPatch {
            vocabulary_enabled: Some(false),
            vocabulary_dir: Some(None),
            ..Default::default()
        };
        assert_eq!(
            <(u64, SettingsPatch)>::try_from(wire::UpdateSettingsRequest::from((3, patch.clone())))
                .unwrap(),
            (3, patch)
        );
    }

    #[test]
    fn every_system_transcription_stage_survives_the_round_trip() {
        let stages = [
            SystemTranscriptionStage::SegmentTranscribed {
                text: Arc::from("hello"),
                label: Some(Arc::from("Speaker 1")),
                at_seconds: 754,
            },
            SystemTranscriptionStage::SegmentTranscribed {
                text: Arc::from("hello"),
                label: None,
                at_seconds: 0,
            },
            SystemTranscriptionStage::SpeakerMerged {
                from: Arc::from("Speaker 3"),
                into: Arc::from("Speaker 1"),
            },
            SystemTranscriptionStage::Started,
            SystemTranscriptionStage::Stopped,
            SystemTranscriptionStage::Failed(RpcErrorCode::SystemTranscriptionPermissionDenied),
        ];

        for stage in stages {
            let sent = wire::SystemTranscriptionEvent::from(stage.clone());

            assert_eq!(
                SystemTranscriptionStage::try_from(&sent).expect("the stage should decode"),
                stage
            );
        }
    }

    #[test]
    fn every_enrollment_stage_survives_the_round_trip() {
        let stages = [
            EnrollmentStage::Recording { index: 1, of: 5 },
            EnrollmentStage::SampleTaken { index: 5, of: 5 },
            EnrollmentStage::Enrolled,
            EnrollmentStage::Cancelled,
            EnrollmentStage::Failed(RpcErrorCode::SpeakerInconsistentSamples),
        ];

        for stage in stages {
            let sent = wire::SpeakerEvent::from(stage.clone());

            assert_eq!(
                EnrollmentStage::try_from(&sent).expect("the stage should decode"),
                stage
            );
        }
    }

    #[test]
    fn an_enrollment_request_round_trips_with_and_without_a_chosen_role() {
        for role in [None, Some(Role::Owner), Some(Role::Guest)] {
            let request = EnrollmentRequest {
                name: SpeakerName::new("ada").expect("the name is usable"),
                role,
                samples: 5,
                replace: true,
            };

            let sent = wire::EnrollSpeakerRequest::from(request.clone());

            assert_eq!(
                EnrollmentRequest::try_from(sent).expect("the request should decode"),
                request
            );
        }
    }

    #[test]
    fn an_enrolled_speaker_round_trips() {
        let speaker = EnrolledSpeaker {
            name: SpeakerName::new("ada").expect("the name is usable"),
            role: Role::Guest,
            enrolled_at: 1_700_000_000,
        };

        let sent = wire::EnrolledSpeaker::from(&speaker);

        assert_eq!(
            EnrolledSpeaker::try_from(&sent).expect("the speaker should decode"),
            speaker
        );
    }

    #[test]
    fn an_unnamed_speaker_is_refused_rather_than_becoming_a_file() {
        let request = wire::EnrollSpeakerRequest {
            name: "../escape".to_owned(),
            role: 0,
            samples: 5,
            replace: false,
        };

        assert!(matches!(
            EnrollmentRequest::try_from(request),
            Err(ProtocolError::SpeakerName { .. })
        ));
    }

    #[test]
    fn a_role_the_daemon_never_sends_is_refused() {
        assert!(role(0).is_err(), "unspecified is a request, not an answer");
        assert!(role(99).is_err(), "an unknown role is not guessed at");
    }

    #[test]
    fn a_speak_request_round_trips_with_its_defaults_absent() {
        let request = SpeechRequest {
            text: SpeechText::new("hello there").expect("valid"),
            voice: Some(Voice::new("Samantha").expect("valid")),
            rate: Some(SpeakingRate::new(1.5).expect("valid")),
        };

        let decoded = SpeechRequest::try_from(wire::SpeakRequest::from(request.clone()))
            .expect("the request should decode");
        assert_eq!(decoded, request);

        let bare = wire::SpeakRequest {
            text: "hello".to_owned(),
            voice: None,
            rate: None,
        };
        let decoded = SpeechRequest::try_from(bare).expect("absent fields stay absent");
        assert_eq!(decoded.voice, None);
        assert_eq!(decoded.rate, None);
    }

    #[test]
    fn a_speak_request_with_unusable_fields_is_refused() {
        let empty = wire::SpeakRequest {
            text: "   ".to_owned(),
            voice: None,
            rate: None,
        };
        assert!(SpeechRequest::try_from(empty).is_err());

        let wild_rate = wire::SpeakRequest {
            text: "hello".to_owned(),
            voice: None,
            rate: Some(40.0),
        };
        assert!(SpeechRequest::try_from(wild_rate).is_err());
    }

    #[test]
    fn a_speech_outcome_round_trips() {
        for outcome in [SpeechOutcome::Spoken, SpeechOutcome::Stopped] {
            let decoded = SpeechOutcome::try_from(wire::SpeakResponse::from(outcome))
                .expect("the outcome should decode");
            assert_eq!(decoded, outcome);
        }
        assert!(SpeechOutcome::try_from(wire::SpeakResponse { outcome: 0 }).is_err());
    }

    #[test]
    fn an_unready_assistant_reaches_the_wire_with_its_fix() {
        let missing = wire::Assistant::from((
            AssistantId::Codex,
            AssistantStatus::NotInstalled {
                binary: "codex".into(),
            },
        ));
        assert_eq!(missing.status, wire::AssistantStatus::NotInstalled as i32);
        assert_eq!(missing.fix, "install the `codex` command");

        let unchecked = wire::Assistant::from((
            AssistantId::ClaudeCode,
            AssistantStatus::Unchecked(aria_assistant::chat::AssistantError::Timeout(
                std::time::Duration::from_secs(2),
            )),
        ));
        assert_eq!(unchecked.status, wire::AssistantStatus::Unchecked as i32);
        assert!(unchecked.fix.contains("deadline"), "{}", unchecked.fix);

        assert_eq!(error_code_to_wire(RpcErrorCode::AssistantSignedOut), 303);
        assert!(matches!(
            error_code_from_wire(303),
            Ok(RpcErrorCode::AssistantSignedOut)
        ));
        assert_eq!(error_code_to_wire(RpcErrorCode::PromptUnreadable), 304);
        assert!(matches!(
            error_code_from_wire(304),
            Ok(RpcErrorCode::PromptUnreadable)
        ));
        for (code, number) in [
            (RpcErrorCode::PromptMissing, 305),
            (RpcErrorCode::PromptsFull, 306),
            (RpcErrorCode::PromptWriteFailed, 307),
            (RpcErrorCode::ChatNameTaken, 308),
        ] {
            assert_eq!(error_code_to_wire(code), number);
            assert_eq!(error_code_from_wire(number).expect("known"), code);
        }
    }

    #[test]
    fn an_unrecognized_code_survives_the_round_trip() {
        let future = 9_999;

        let decoded = error_code_from_wire(future).expect("an unknown code still names a failure");

        assert_eq!(decoded, RpcErrorCode::Unknown(future));
        assert_eq!(error_code_to_wire(decoded), future);
        assert_eq!(error_code_label(future), "unknown");
    }

    #[test]
    fn each_combo_keeps_its_modifier_on_the_wire() {
        for (combo, expected) in [
            (Combo::Shift, wire::ShortcutCombo::Shift),
            (Combo::Command, wire::ShortcutCombo::Command),
        ] {
            assert_eq!(
                wire::Event::from(combo).payload,
                Some(wire::event::Payload::Hotkey(wire::HotkeyEvent {
                    combo: expected.into(),
                })),
                "{combo:?}"
            );
        }
    }

    #[test]
    fn prompts_round_trip_and_need_their_kind() {
        let prompts = Prompts::new(vec![
            Prompt::new(
                PromptId::new("7f3c2a90").expect("id"),
                "Polish".to_owned(),
                PromptKind::Compose,
                "/prompts/polish.md".into(),
                true,
                Some(
                    Chord::new(
                        0,
                        Modifiers {
                            control: true,
                            ..Modifiers::default()
                        },
                    )
                    .expect("chord"),
                ),
            )
            .expect("prompt"),
        ])
        .expect("prompts");

        let decoded = Prompts::try_from(wire::Prompts::from(&prompts)).expect("decodes");
        assert_eq!(decoded, prompts);

        let mut wire = wire::Prompts::from(&prompts);
        wire.entries[0].file = "relative.md".to_owned();
        assert!(matches!(
            Prompts::try_from(wire),
            Err(ProtocolError::InvalidPrompt(_))
        ));
        let mut wire = wire::Prompts::from(&prompts);
        wire.entries[0].kind = wire::PromptKind::Unspecified as i32;
        assert!(matches!(
            Prompts::try_from(wire),
            Err(ProtocolError::InvalidEnum {
                field: "prompt kind",
                ..
            })
        ));
    }

    #[test]
    fn a_saved_prompt_needs_a_name_a_kind_and_an_absolute_file() {
        let request = |name: &str, file: Option<&str>| wire::SavePromptRequest {
            id: None,
            name: Some(name.to_owned()),
            kind: wire::PromptKind::Compose as i32,
            file: file.map(str::to_owned),
            instructions: Some("Three bullets.".to_owned()),
        };

        assert!(matches!(
            PromptDraft::try_from(request(" \n", None)),
            Err(ProtocolError::InvalidPrompt(PromptError::Name))
        ));
        assert!(matches!(
            PromptDraft::try_from(request("Summary", Some("notes/summary.md"))),
            Err(ProtocolError::InvalidPrompt(PromptError::File))
        ));
        let mut unspecified = request("Summary", None);
        unspecified.kind = wire::PromptKind::Unspecified as i32;
        assert!(matches!(
            PromptDraft::try_from(unspecified),
            Err(ProtocolError::InvalidEnum {
                field: "prompt kind",
                ..
            })
        ));
        let mut blank = request("Summary", None);
        blank.instructions = Some(" ".to_owned());
        assert!(matches!(
            PromptDraft::try_from(blank),
            Err(ProtocolError::InvalidInstructions(_))
        ));

        let mut unnamed = request("unused", None);
        unnamed.name = None;
        assert!(matches!(
            PromptDraft::try_from(unnamed.clone()),
            Err(ProtocolError::MissingBody(_))
        ));
        let mut empty = request("Summary", None);
        empty.instructions = None;
        assert!(matches!(
            PromptDraft::try_from(empty),
            Err(ProtocolError::MissingBody("instructions"))
        ));

        unnamed.file = Some("/notes/summary.md".to_owned());
        unnamed.instructions = None;
        assert!(matches!(
            PromptDraft::try_from(unnamed).expect("decodes"),
            PromptDraft::New {
                kind: PromptKind::Compose,
                source: NewSource::Chosen {
                    name: None,
                    instructions: None,
                    ..
                },
            }
        ));

        let draft =
            PromptDraft::try_from(request("Summary", Some("/notes/summary.md"))).expect("decodes");
        let PromptDraft::New {
            kind,
            source: NewSource::Chosen { file, name, .. },
        } = draft
        else {
            panic!("a request with no ID is a new prompt: {draft:?}");
        };
        assert_eq!(name.as_deref(), Some("Summary"));
        assert_eq!(kind, PromptKind::Compose);
        assert_eq!(file, std::path::Path::new("/notes/summary.md"));

        let mut edit = request("Summary", None);
        edit.id = Some("polish".to_owned());
        edit.instructions = None;
        assert!(matches!(
            PromptDraft::try_from(edit).expect("decodes"),
            PromptDraft::Edit {
                kind: Some(PromptKind::Compose),
                file: None,
                instructions: None,
                ..
            }
        ));
    }

    #[test]
    fn a_prompt_is_read_by_its_id_or_an_absolute_file() {
        let request = |source| wire::GetPromptRequest {
            source: Some(source),
        };

        assert_eq!(
            PromptSource::try_from(request(wire::get_prompt_request::Source::PromptId(
                "polish".to_owned()
            )))
            .expect("decodes"),
            PromptSource::Stored(PromptId::new("polish").expect("id"))
        );
        assert_eq!(
            PromptSource::try_from(request(wire::get_prompt_request::Source::File(
                "/notes/polish.md".to_owned()
            )))
            .expect("decodes"),
            PromptSource::File("/notes/polish.md".into())
        );
        assert!(matches!(
            PromptSource::try_from(request(wire::get_prompt_request::Source::File(
                "notes/polish.md".to_owned()
            ))),
            Err(ProtocolError::InvalidPrompt(PromptError::File))
        ));
        assert!(matches!(
            PromptSource::try_from(request(wire::get_prompt_request::Source::PromptId(
                String::new()
            ))),
            Err(ProtocolError::InvalidPrompt(PromptError::Id))
        ));
        assert!(matches!(
            PromptSource::try_from(wire::GetPromptRequest { source: None }),
            Err(ProtocolError::MissingBody(_))
        ));
    }

    #[test]
    fn a_removal_needs_a_prompt_id() {
        let request = |id: &str| wire::RemovePromptRequest { id: id.to_owned() };

        assert_eq!(
            PromptId::try_from(request("polish")).expect("decodes"),
            PromptId::new("polish").expect("id")
        );
        assert!(matches!(
            PromptId::try_from(request("")),
            Err(ProtocolError::InvalidPrompt(PromptError::Id))
        ));
    }

    #[test]
    fn a_delivery_needs_text_within_its_limit_and_a_mode() {
        let call = |text: String, mode: wire::OutputMode| {
            DeliverCall::try_from(wire::DeliverRequest {
                text,
                application_id: String::new(),
                output_mode: mode as i32,
            })
        };

        assert!(call(" ".into(), wire::OutputMode::Insert).is_err());
        assert!(call("hi".into(), wire::OutputMode::Unspecified).is_err());
        let limit = aria_platform::delivery::MAX_TEXT_BYTES;
        assert!(call("x".repeat(limit), wire::OutputMode::Insert).is_ok());
        assert!(call("x".repeat(limit + 1), wire::OutputMode::Insert).is_err());
        let call = call("hi".into(), wire::OutputMode::Copy).expect("valid");
        assert_eq!(call.output_mode, OutputMode::Copy);
        assert_eq!(call.application, None);
    }

    fn run_prompt_request() -> wire::RunPromptRequest {
        wire::RunPromptRequest {
            input: Some(wire::run_prompt_request::Input::Text("hi".to_owned())),
            prompt: Some(wire::run_prompt_request::Prompt::PromptId("p".to_owned())),
            ..Default::default()
        }
    }

    #[test]
    fn a_prompt_call_needs_words_a_prompt_id_and_at_most_a_real_app() {
        let call = |text: &str, id: &str, app: &str| {
            PromptCall::try_from(wire::RunPromptRequest {
                input: Some(wire::run_prompt_request::Input::Text(text.to_owned())),
                prompt: Some(wire::run_prompt_request::Prompt::PromptId(id.to_owned())),
                application_id: app.to_owned(),
                tap_cancels: false,
                output_mode: Some(wire::OutputMode::InsertOrCopy as i32),
            })
        };
        assert!(matches!(
            call(" ", "p", ""),
            Err(ProtocolError::EmptyPrompt)
        ));
        assert!(call("hi", "../p", "").is_err());
        assert!(call("hi", "p", "not an app").is_err());
        assert_eq!(call("hi", "p", "").expect("valid").application, None);
        assert!(
            call("hi", "p", "com.apple.Notes")
                .expect("valid")
                .application
                .is_some()
        );
        let typing_nothing = |app: &str| {
            PromptCall::try_from(wire::RunPromptRequest {
                application_id: app.to_owned(),
                ..run_prompt_request()
            })
        };
        assert_eq!(typing_nothing("").expect("valid").output_mode, None);
        assert!(typing_nothing("com.apple.Notes").is_err());
    }

    #[test]
    fn a_prompt_call_runs_a_stored_prompt_or_one_off_instructions_a_run_would_take() {
        let call = |prompt| {
            PromptCall::try_from(wire::RunPromptRequest {
                input: Some(wire::run_prompt_request::Input::HistoryEntryId(7)),
                prompt,
                ..run_prompt_request()
            })
        };
        let instructions =
            |text: String| Some(wire::run_prompt_request::Prompt::Instructions(text));

        assert_eq!(
            call(instructions("List the action items.".into()))
                .expect("valid")
                .prompt,
            PromptChoice::OneOff(Instructions::new("List the action items.".into()).unwrap())
        );
        assert!(matches!(
            call(instructions(" \n".into())),
            Err(ProtocolError::InvalidInstructions(_))
        ));
        assert!(matches!(
            call(instructions("a".repeat(16 * 1024 + 1))),
            Err(ProtocolError::InvalidInstructions(_))
        ));
        assert!(matches!(
            call(None),
            Err(ProtocolError::MissingBody("prompt"))
        ));
    }

    #[test]
    fn a_prompt_result_round_trips() {
        let result = PromptResult {
            id: 3,
            created_at_ms: 1_700_000_000_000,
            prompt_name: "Summary".to_owned(),
            text: "- one\n- two".to_owned(),
        };

        assert_eq!(
            PromptResult::try_from(wire::PromptResult::from(result.clone())).unwrap(),
            result
        );
    }

    #[test]
    fn a_prompt_call_round_trips() {
        for call in [
            PromptCall {
                input: PromptInput::Entry(7),
                prompt: PromptChoice::Stored(PromptId::new("summary").unwrap()),
                application: None,
                tap_cancels: false,
                output_mode: None,
            },
            PromptCall {
                input: PromptInput::Text("we met on Friday".to_owned()),
                prompt: PromptChoice::OneOff(
                    Instructions::new("List the action items.".into()).unwrap(),
                ),
                application: Some(ApplicationId::new("com.apple.mail".to_owned()).unwrap()),
                tap_cancels: true,
                output_mode: Some(OutputMode::Copy),
            },
        ] {
            let decoded =
                PromptCall::try_from(wire::RunPromptRequest::from(call.clone())).expect("decodes");
            assert_eq!(decoded, call);
        }
    }

    #[test]
    fn a_prompt_runs_on_a_real_history_entry_or_nothing() {
        let on = |input| {
            PromptCall::try_from(wire::RunPromptRequest {
                input,
                ..run_prompt_request()
            })
        };
        assert_eq!(
            on(Some(wire::run_prompt_request::Input::HistoryEntryId(7)))
                .expect("valid")
                .input,
            PromptInput::Entry(7)
        );
        assert!(on(Some(wire::run_prompt_request::Input::HistoryEntryId(0))).is_err());
        assert!(matches!(on(None), Err(ProtocolError::EmptyPrompt)));
    }

    #[test]
    fn a_hotkey_round_trips_and_needs_a_chord_with_a_modifier() {
        use wire::shortcut::Binding;
        let chord = wire::Chord {
            key_code: 13,
            control: true,
            option: false,
            shift: true,
            command: false,
        };
        for binding in [
            Some(Binding::Chord(chord)),
            Some(Binding::Combo(wire::ShortcutCombo::Command.into())),
            None,
        ] {
            let hotkey = Option::<Shortcut>::try_from(wire::Shortcut { binding }).expect("decodes");
            assert_eq!(wire::Shortcut::from(hotkey).binding, binding);
        }
        let cleared = SettingsPatch::try_from(wire::SettingsPatch {
            ask_shortcut: Some(wire::Shortcut::default()),
            ..Default::default()
        })
        .expect("decodes");
        assert_eq!(cleared.ask_shortcut, Some(None));
        assert!(matches!(
            Option::<Shortcut>::try_from(wire::Shortcut {
                binding: Some(Binding::Combo(wire::ShortcutCombo::Unspecified.into())),
            }),
            Err(ProtocolError::InvalidEnum {
                field: "hotkey combo",
                ..
            })
        ));

        let bare = wire::Chord {
            control: false,
            shift: false,
            ..chord
        };
        assert!(Chord::try_from(bare).is_err());
        let absurd = wire::Chord {
            key_code: u32::from(u16::MAX) + 1,
            ..chord
        };
        assert!(matches!(
            Chord::try_from(absurd),
            Err(ProtocolError::ChordKeyCode(_))
        ));
    }

    #[test]
    fn system_transcription_output_requires_an_absolute_path() {
        let decode = |path: Option<&str>| {
            Option::<std::path::PathBuf>::try_from(wire::StartSystemTranscriptionRequest {
                transcript_path: path.map(str::to_owned),
            })
        };
        assert_eq!(decode(None).unwrap(), None);
        assert_eq!(
            decode(Some("/tmp/transcript.txt")).unwrap(),
            Some("/tmp/transcript.txt".into())
        );
        for path in ["", "notes/transcript.txt", "/tmp/transcript\0.txt"] {
            assert!(matches!(
                decode(Some(path)),
                Err(ProtocolError::InvalidTranscriptPath)
            ));
        }
    }

    #[test]
    fn a_chat_target_round_trips_and_an_empty_one_is_refused() {
        let target = ChatTarget {
            workdir: "/tmp".into(),
            assistant_id: AssistantId::ClaudeCode,
        };

        let decoded = ChatTarget::try_from(wire::ResolveChatTargetResponse::from(target.clone()))
            .expect("the target should decode");

        assert_eq!(decoded, target);
        assert!(
            ChatTarget::try_from(wire::ResolveChatTargetResponse {
                workdir: String::new(),
                assistant_id: wire::AssistantId::ClaudeCode as i32,
            })
            .is_err()
        );
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use aria_voice::history::{HistoryEntry, HistoryPage, HistoryQuery};
    use aria_voice::stt::Language;

    fn entry() -> HistoryEntry {
        HistoryEntry {
            id: 42,
            title: None,
            content: aria_voice::history::HistoryContent::Dictation(
                aria_voice::history::DictationContent {
                    language: "en".to_owned(),
                    engine: String::new(),
                    own_voice_only: false,
                    audio: aria_voice::history::AudioAvailability::Available {
                        expires_at_ms: 2_000,
                    },
                    retrying: false,
                },
            ),
            created_at_ms: 1_000,
            duration_ms: None,
            source: aria_voice::history::HistorySource::Local,
            application: None,
            project: None,
            text: "界".repeat(200),
            complete: false,
            result_count: 0,
            status: aria_voice::history::HistoryStatus::TranscriptionFailed,
            error: None,
            warning: None,
            deletion_pending: false,
        }
    }

    #[test]
    fn history_requests_reject_unbounded_queries_and_invalid_ids() {
        for (query, before_id, limit) in [
            ("é".repeat(129), None, 10),
            (String::new(), Some(0), 10),
            (String::new(), Some(u64::MAX), 10),
            (String::new(), None, 0),
            (String::new(), None, 101),
        ] {
            assert!(
                HistoryQuery::try_from(wire::ListHistoryRequest {
                    section: None,
                    query,
                    before_id,
                    limit,
                    collapse_chats: false,
                })
                .is_err()
            );
        }
        assert!(
            HistoryQuery::try_from(wire::ListHistoryRequest {
                section: None,
                query: "a_%".to_owned(),
                before_id: Some(2),
                limit: 100,
                collapse_chats: false,
            })
            .is_ok()
        );
        let section_filter = |section| wire::ListHistoryRequest {
            section,
            limit: 10,
            ..Default::default()
        };
        assert!(
            HistoryQuery::try_from(section_filter(Some(
                wire::HistorySection::Unspecified as i32
            )))
            .is_err()
        );
        assert!(HistoryQuery::try_from(section_filter(Some(99))).is_err());
        assert_eq!(
            HistoryQuery::try_from(section_filter(Some(
                wire::HistorySection::SystemTranscription as i32
            )))
            .unwrap()
            .section,
            Some(aria_voice::history::HistorySection::SystemTranscription)
        );
        assert_eq!(
            HistoryQuery::try_from(section_filter(None))
                .unwrap()
                .section,
            None
        );
        assert!(history_id(0).is_err());
        assert!(history_id(u64::MAX).is_err());
    }

    #[test]
    fn an_entry_keeps_its_project_folder_across_the_wire() {
        let mut entry = entry();
        entry.project = Some("/Users/me/code/aria".into());

        let wire = wire::HistoryEntry::from(entry.clone());

        assert_eq!(
            wire.project_directory.as_deref(),
            Some("/Users/me/code/aria")
        );
        assert_eq!(HistoryEntry::try_from(wire).unwrap().project, entry.project);
    }

    #[test]
    fn list_cuts_a_chat_reply_as_it_cuts_the_question() {
        let mut entry = entry();
        entry.content = aria_voice::history::HistoryContent::Ask {
            reply: "é".repeat(300),
            chat: None,
        };
        let page = aria_voice::history::HistoryPage {
            entries: vec![entry],
            next_before_id: None,
            warning: None,
        };

        let listed = wire::ListHistoryResponse::from(page);

        let Some(wire::history_entry::Details::Reply(reply)) = &listed.entries[0].details else {
            panic!("a chat lists its reply");
        };
        assert_eq!(reply.len(), 512);
    }

    #[test]
    fn an_ask_entry_keeps_its_chat_across_the_wire_and_a_bad_one_is_refused() {
        use aria_voice::history::HistoryContent;
        let mut ask = entry();
        ask.content = HistoryContent::Ask {
            reply: "Friday.".to_owned(),
            chat: Some(ChatKey {
                assistant_id: AssistantId::Codex,
                name: ChatName::new("first").expect("a valid name"),
            }),
        };

        let wire = wire::HistoryEntry::from(ask.clone());
        assert_eq!(
            HistoryEntry::try_from(wire.clone()).unwrap().content,
            ask.content
        );
        let sent = wire.chat.clone().expect("an ask sends its chat");
        for bad in [
            wire::ChatKey {
                assistant_id: wire::AssistantId::Unspecified as i32,
                ..sent.clone()
            },
            wire::ChatKey {
                name: " ".to_owned(),
                ..sent
            },
        ] {
            assert!(
                HistoryEntry::try_from(wire::HistoryEntry {
                    chat: Some(bad),
                    ..wire.clone()
                })
                .is_err()
            );
        }
        let dictation = wire::HistoryEntry {
            chat: Some(sent),
            ..wire::HistoryEntry::from(entry())
        };
        assert!(HistoryEntry::try_from(dictation).is_err());
    }

    #[test]
    fn a_chat_read_needs_its_key_and_one_to_ten_asks() {
        let chat = wire::ChatKey {
            assistant_id: wire::AssistantId::ClaudeCode as i32,
            name: "first".to_owned(),
        };
        let read = |limit, before_id| wire::GetChatRequest {
            chat: Some(chat.clone()),
            before_id,
            limit,
        };

        let query = ChatQuery::try_from(read(10, Some(7))).unwrap();
        assert_eq!((query.limit, query.before_id), (10, Some(7)));
        assert_eq!(query.chat.name.as_str(), "first");
        for bad in [
            read(0, None),
            read(11, None),
            read(1, Some(0)),
            wire::GetChatRequest {
                chat: None,
                ..read(1, None)
            },
        ] {
            assert!(ChatQuery::try_from(bad).is_err());
        }
    }

    #[test]
    fn a_clients_chat_requests_round_trip_and_a_bad_cursor_is_refused() {
        let chat = || ChatKey {
            assistant_id: AssistantId::Codex,
            name: ChatName::new("first").expect("a valid name"),
        };

        let query = ChatQuery::try_from(wire::GetChatRequest::from(ChatQuery {
            chat: chat(),
            before_id: Some(7),
            limit: 10,
        }))
        .unwrap();
        assert_eq!(
            (query.chat, query.before_id, query.limit),
            (chat(), Some(7), 10)
        );
        let trip = ChatName::new("Trip").expect("a valid name");
        let (renamed, name) =
            <(ChatKey, ChatName)>::try_from(wire::RenameChatRequest::from((chat(), &trip)))
                .unwrap();
        assert_eq!((renamed, name), (chat(), trip));
        assert_eq!(
            ChatKey::try_from(wire::RenameChatResponse::from(chat())).unwrap(),
            chat()
        );
        assert_eq!(
            ChatKey::try_from(wire::DeleteChatRequest::from(chat())).unwrap(),
            chat()
        );

        let page = |next_before_id| wire::GetChatResponse {
            entries: vec![entry().into()],
            next_before_id,
        };
        let read = ChatPage::try_from(page(Some(5))).unwrap();
        assert_eq!((read.entries.len(), read.next_before_id), (1, Some(5)));
        for cursor in [0, i64::MAX as u64 + 1] {
            assert!(ChatPage::try_from(page(Some(cursor))).is_err());
        }
    }

    #[test]
    fn a_history_list_keeps_its_choice_to_collapse_chats() {
        assert!(
            HistoryQuery::try_from(wire::ListHistoryRequest {
                limit: 10,
                collapse_chats: true,
                ..Default::default()
            })
            .unwrap()
            .collapse_chats
        );
    }

    #[test]
    fn a_mac_audio_entry_carries_its_transcript_and_needs_one() {
        use aria_voice::history::{HistoryContent, TranscriptRead};
        let mut entry = entry();
        entry.content = HistoryContent::SystemTranscription {
            transcript: "/home/me/.aria/transcripts/a.txt".into(),
            read: Some(TranscriptRead::Truncated),
        };

        let wire = wire::HistoryEntry::from(entry.clone());
        assert_eq!(
            HistoryEntry::try_from(wire.clone()).unwrap().content,
            entry.content
        );
        let without = wire::HistoryEntry {
            details: None,
            ..wire.clone()
        };
        assert!(HistoryEntry::try_from(without).is_err());

        let before_sections = wire::HistoryEntry {
            section: wire::HistorySection::Unspecified as i32,
            details: None,
            ..wire.clone()
        };
        assert!(matches!(
            HistoryEntry::try_from(before_sections).unwrap().content,
            HistoryContent::Dictation(_)
        ));
        let unspecified_with_details = wire::HistoryEntry {
            section: wire::HistorySection::Unspecified as i32,
            ..wire
        };
        assert!(HistoryEntry::try_from(unspecified_with_details).is_err());
    }

    #[test]
    fn a_rename_needs_an_id_and_a_title_that_isnt_blank() {
        type Rename = (u64, Option<aria_voice::history::Title>);
        let rename = |id, title: Option<&str>| {
            Rename::try_from(wire::RenameHistoryRequest {
                id,
                title: title.map(str::to_owned),
            })
        };

        assert!(rename(0, Some("Plan")).is_err());
        assert!(rename(7, Some("  ")).is_err());
        assert_eq!(rename(7, None).unwrap(), (7, None));
        let (_, title) = rename(7, Some(" Plan ")).unwrap();
        assert_eq!(title.unwrap().as_str(), "Plan");
    }

    #[test]
    fn a_chat_rename_needs_its_key_and_a_valid_name() {
        let first = wire::ChatKey {
            assistant_id: wire::AssistantId::ClaudeCode as i32,
            name: "first".to_owned(),
        };
        let rename = |chat: Option<wire::ChatKey>, name: &str| {
            <(ChatKey, ChatName)>::try_from(wire::RenameChatRequest {
                chat,
                name: name.to_owned(),
            })
        };

        let (chat, name) = rename(Some(first.clone()), " Trip ").unwrap();
        assert_eq!(chat.name.as_str(), "first");
        assert_eq!(name.as_str(), "Trip");
        let unspecified = wire::ChatKey {
            assistant_id: wire::AssistantId::Unspecified as i32,
            ..first.clone()
        };
        assert!(matches!(
            rename(None, "Trip"),
            Err(ProtocolError::MissingBody("chat"))
        ));
        assert!(rename(Some(unspecified), "Trip").is_err());
        for name in ["", "  ", "two\nlines", "/Users/me/proj"] {
            assert!(
                matches!(
                    rename(Some(first.clone()), name),
                    Err(ProtocolError::InvalidChatName(_))
                ),
                "{name:?}"
            );
        }
    }

    #[test]
    fn history_retry_options_are_validated_and_successful_audio_is_retryable() {
        let request = wire::RegenerateHistoryRequest {
            id: 42,
            engine_id: Some("whisper".into()),
            language: Some("auto".into()),
        };
        let (id, options) = <(u64, aria_voice::history::RetryOptions)>::try_from(request).unwrap();
        assert_eq!(id, 42);
        assert_eq!(options.engine.as_deref(), Some("whisper"));
        assert_eq!(options.language, Some(Language::Automatic));
        for (engine, language) in [(Some("unknown"), None), (Some(""), None), (None, Some(""))] {
            assert!(
                <(u64, aria_voice::history::RetryOptions)>::try_from(
                    wire::RegenerateHistoryRequest {
                        id: 42,
                        engine_id: engine.map(str::to_owned),
                        language: language.map(str::to_owned),
                    }
                )
                .is_err()
            );
        }
        let mut original = entry();
        original.status = aria_voice::history::HistoryStatus::Success;
        let encoded = wire::HistoryEntry::from(original);
        assert!(encoded.can_regenerate);
        let decoded = HistoryEntry::try_from(encoded.clone()).unwrap();
        assert_eq!(decoded.status, aria_voice::history::HistoryStatus::Success);
        assert_eq!(
            decoded.audio(),
            aria_voice::history::AudioAvailability::Available {
                expires_at_ms: 2_000
            }
        );
        for audio in [
            wire::HistoryAudioAvailability::Unavailable,
            wire::HistoryAudioAvailability::Expired,
        ] {
            let mut unexpected_expiry = encoded.clone();
            unexpected_expiry.audio_availability = audio as i32;
            assert!(HistoryEntry::try_from(unexpected_expiry).is_err());
        }
        let mut missing_expiry = encoded;
        missing_expiry.audio_expire_time = None;
        assert!(HistoryEntry::try_from(missing_expiry).is_err());
    }

    #[test]
    fn a_time_before_the_epoch_or_a_negative_span_is_refused() {
        let encoded = wire::HistoryEntry::from(entry());
        let mut before_epoch = encoded.clone();
        before_epoch.create_time = Some(prost_types::Timestamp {
            seconds: -1,
            nanos: 0,
        });
        assert!(HistoryEntry::try_from(before_epoch).is_err());
        let mut negative = encoded.clone();
        negative.duration = Some(prost_types::Duration {
            seconds: -1,
            nanos: 0,
        });
        assert!(HistoryEntry::try_from(negative).is_err());
        let mut undated = encoded;
        undated.create_time = None;
        assert!(HistoryEntry::try_from(undated).is_err());
    }

    #[test]
    fn history_pages_bound_utf8_previews_and_keep_full_text_separate() {
        let mut original = entry();
        original.project = Some("/private/project".into());
        original.dictation_mut().unwrap().own_voice_only = true;
        let full = wire::HistoryEntry::from(original.clone());
        assert_eq!(full.text.len(), 600);
        assert!(full.can_regenerate);
        let page = wire::ListHistoryResponse::from(HistoryPage {
            entries: vec![original],
            next_before_id: Some(42),
            warning: None,
        });
        assert_eq!(page.entries[0].text.len(), 510);
        assert_eq!(page.entries[0].id, 42);
        let mut busy = entry();
        busy.dictation_mut().unwrap().retrying = true;
        assert!(!wire::HistoryEntry::from(busy).can_regenerate);
    }

    #[test]
    fn history_errors_keep_their_stable_category() {
        for (code, grpc) in [
            (RpcErrorCode::HistoryMissing, tonic::Code::NotFound),
            (
                RpcErrorCode::HistoryAudioUnavailable,
                tonic::Code::FailedPrecondition,
            ),
            (RpcErrorCode::HistoryUnavailable, tonic::Code::Unavailable),
            (
                RpcErrorCode::HistoryProjectUnavailable,
                tonic::Code::FailedPrecondition,
            ),
            (
                RpcErrorCode::HistoryEntryEmpty,
                tonic::Code::FailedPrecondition,
            ),
            (
                RpcErrorCode::HistoryTranscriptUnreadable,
                tonic::Code::FailedPrecondition,
            ),
        ] {
            assert_eq!(
                error_code_from_wire(error_code_to_wire(code)).expect("known code"),
                code
            );
            assert_eq!(
                failure_status(RpcFailure::new(code, "safe message")).code(),
                grpc
            );
        }
    }
}
