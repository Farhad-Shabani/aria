//! Translation between generated wire messages and domain types.

use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use tonic::Status;

use aria_kit::synthesis::{
    SpeakingRate, SpeakingRateError, SpeechText, SpeechTextError, Voice, VoiceError,
};

use crate::audio::{SilenceDuration, SilenceDurationError};
use crate::chat::{
    AskTarget, ChatReply, ChatRequest, ChatStage, ChatStageKind, PromptCall, PromptKind, Step,
    Ticket,
};
use crate::config::ChatAgent;
use crate::delivery::{DeliveryOutcome, Placement, Rect};
use crate::dictation::{
    CaptionSnapshot, DeliveryMode, DictationEvent, DictationOutcome, DictationRequest,
    DictationStage, GhostSnapshot, MicLevel, OutputMode, RecordingId, StopMode,
};
use crate::enrollment::{EnrolledSpeaker, EnrollmentRequest, EnrollmentStage};
use crate::hotkey::HotkeyKey;
use crate::listen::ListenStage;
use crate::preferences::{
    ApplicationId, ApplicationOverride, ApplicationOverrides, Chord, ChordError,
    DictationOverrides, Modifiers, OverrideError, PreferencePatch, PreferenceSnapshot, Prompt,
    PromptError, PromptId, Prompts, UpdateResult, UserPreferences,
};
use crate::rpc::{RpcErrorCode, RpcFailure};
use crate::speaker::{Role, SpeakerName, SpeakerNameError};
use crate::speech::{SpeechOutcome, SpeechRequest, SpeechStage};
use crate::transcription::LanguageCodeError;

use super::wire;

mod inference;

// Handlers speak domain types; every wire message and status is assembled
// through the conversions here.

/// `grpc-status-details-bin` carries a `google.rpc.Status` by convention.
const RPC_ERROR_TYPE_URL: &str = "type.googleapis.com/aria.daemon.v1.RpcError";

/// The status code is the lossy projection; the exact Aria code rides in the
/// status details as a `google.rpc.Status` holding one packed `RpcError`.
impl From<RpcFailure> for Status {
    fn from(failure: RpcFailure) -> Self {
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
        Self::with_details(code, failure.message, details.encode_to_vec().into())
    }
}

impl From<RpcFailure> for wire::RpcError {
    fn from(failure: RpcFailure) -> Self {
        Self {
            code: failure.code.into(),
            message: failure.message,
        }
    }
}

impl TryFrom<wire::RpcError> for RpcFailure {
    type Error = ProtocolError;

    fn try_from(error: wire::RpcError) -> Result<Self, Self::Error> {
        Ok(Self::new(
            RpcErrorCode::try_from(error.code)?,
            error.message,
        ))
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
        | RpcErrorCode::AgentTimeout => tonic::Code::DeadlineExceeded,
        RpcErrorCode::HistoryAudioUnavailable
        | RpcErrorCode::HistoryProjectUnavailable
        | RpcErrorCode::NoSpeech
        | RpcErrorCode::SpeakerNoSpeech
        | RpcErrorCode::SpeakerModelMissing
        | RpcErrorCode::SpeakerInconsistentSamples
        | RpcErrorCode::ListenUnsupportedOs
        | RpcErrorCode::AgentUnavailable
        | RpcErrorCode::ApplicationExcluded => tonic::Code::FailedPrecondition,
        RpcErrorCode::SpeakerAlreadyEnrolled => tonic::Code::AlreadyExists,
        RpcErrorCode::HistoryMissing
        | RpcErrorCode::SpeakerNotEnrolled
        | RpcErrorCode::VoiceUnavailable => tonic::Code::NotFound,
        RpcErrorCode::ListenPermissionDenied | RpcErrorCode::PermissionDenied => {
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
        | RpcErrorCode::AgentFailed
        | RpcErrorCode::ListenCaptureFailed
        | RpcErrorCode::SpeakerProfileWriteFailed
        | RpcErrorCode::DurabilityUnconfirmed => tonic::Code::Internal,
        RpcErrorCode::Unknown(_) => tonic::Code::Unknown,
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

impl From<UpdateResult> for wire::UpdatePreferencesResponse {
    fn from(update: UpdateResult) -> Self {
        let result = match update {
            UpdateResult::Updated(snapshot) => {
                wire::update_preferences_response::Result::Snapshot((&snapshot).into())
            }
            UpdateResult::Conflict(current) => {
                wire::update_preferences_response::Result::Conflict(wire::PreferenceConflict {
                    current: Some((&current).into()),
                })
            }
        };
        Self {
            result: Some(result),
        }
    }
}

impl TryFrom<wire::UpdatePreferencesResponse> for UpdateResult {
    type Error = ProtocolError;

    fn try_from(response: wire::UpdatePreferencesResponse) -> Result<Self, Self::Error> {
        match response.result {
            Some(wire::update_preferences_response::Result::Snapshot(snapshot)) => {
                Ok(Self::Updated(snapshot.try_into()?))
            }
            Some(wire::update_preferences_response::Result::Conflict(conflict)) => {
                let current = conflict
                    .current
                    .ok_or(ProtocolError::MissingBody("preference conflict snapshot"))?;
                Ok(Self::Conflict(current.try_into()?))
            }
            None => Err(ProtocolError::MissingBody("preferences update response")),
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
            committed: caption.committed.to_string(),
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
        let agent = wire::ChatAgent::from(stage.agent) as i32;
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
                    agent,
                    step: i32::from(step),
                    skill,
                    copied: false,
                }
            }
            ChatStageKind::Replied(ChatReply {
                text,
                delivery_error,
                copied,
            }) => Self {
                stage: wire::ChatStage::Replied as i32,
                error_code: delivery_error.map_or(0, i32::from),
                ticket,
                text,
                error_message: String::new(),
                agent,
                step: 0,
                skill: String::new(),
                copied,
            },
            ChatStageKind::Failed { code, message } => Self {
                stage: wire::ChatStage::Failed as i32,
                error_code: code.into(),
                ticket,
                text: String::new(),
                error_message: message,
                agent,
                step: 0,
                skill: String::new(),
                copied: false,
            },
        }
    }
}

impl TryFrom<&wire::ChatEvent> for ChatStage {
    type Error = ProtocolError;

    fn try_from(event: &wire::ChatEvent) -> Result<Self, Self::Error> {
        let kind = match wire::ChatStage::try_from(event.stage) {
            Ok(wire::ChatStage::Working) => {
                ChatStageKind::Working(Step::from_wire(event.step, &event.skill)?)
            }
            Ok(wire::ChatStage::Replied) => ChatStageKind::Replied(ChatReply {
                text: event.text.clone(),
                delivery_error: match event.error_code {
                    0 => None,
                    code => Some(RpcErrorCode::try_from(code)?),
                },
                copied: event.copied,
            }),
            Ok(wire::ChatStage::Failed) => ChatStageKind::Failed {
                code: RpcErrorCode::try_from(event.error_code)?,
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
            agent: decode_enum::<wire::ChatAgent>(event.agent, "agent")?,
            kind,
        })
    }
}

impl From<ChatAgent> for wire::ChatAgent {
    fn from(agent: ChatAgent) -> Self {
        match agent {
            ChatAgent::ClaudeCode => Self::ClaudeCode,
            ChatAgent::Codex => Self::Codex,
        }
    }
}

impl From<Step> for i32 {
    fn from(step: Step) -> Self {
        let code = match step {
            Step::Thinking => wire::ChatStep::Thinking,
            Step::Reading => wire::ChatStep::Reading,
            Step::Searching => wire::ChatStep::Searching,
            Step::Running => wire::ChatStep::Running,
            Step::Skill(..) => wire::ChatStep::Skill,
            Step::Unknown(code) => return code,
        };
        code as Self
    }
}

impl Step {
    /// A code this build does not know is kept, as `RpcErrorCode` keeps
    /// its own, so a newer daemon's step still shows as one. Unspecified
    /// is a daemon too old to send steps at all.
    fn from_wire(code: i32, skill: &str) -> Result<Self, ProtocolError> {
        Ok(match wire::ChatStep::try_from(code) {
            Ok(wire::ChatStep::Thinking) => Self::Thinking,
            Ok(wire::ChatStep::Reading) => Self::Reading,
            Ok(wire::ChatStep::Searching) => Self::Searching,
            Ok(wire::ChatStep::Running) => Self::Running,
            Ok(wire::ChatStep::Skill) => Self::skill(skill),
            Ok(wire::ChatStep::Unspecified) => {
                return Err(ProtocolError::InvalidEnum {
                    field: "step",
                    value: code,
                });
            }
            Err(..) => Self::Unknown(code),
        })
    }
}

impl WireEnum for wire::ChatAgent {
    type Domain = ChatAgent;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::ClaudeCode => Some(ChatAgent::ClaudeCode),
            Self::Codex => Some(ChatAgent::Codex),
            Self::Unspecified => None,
        }
    }
}

impl From<ChatRequest> for wire::AskRequest {
    fn from(request: ChatRequest) -> Self {
        Self {
            text: request.text,
            workdir: request.workdir.to_string_lossy().into_owned(),
            new_conversation: request.new,
            output_mode: request
                .output_mode
                .map(|mode| wire::OutputMode::from(mode) as i32),
            agent: wire::ChatAgent::from(request.agent) as i32,
            model: request.model.unwrap_or_default(),
            session: request.session.unwrap_or_default(),
            own_voice: request.own_voice,
        }
    }
}

impl TryFrom<wire::AskRequest> for ChatRequest {
    type Error = ProtocolError;

    fn try_from(request: wire::AskRequest) -> Result<Self, Self::Error> {
        if request.text.trim().is_empty() {
            return Err(ProtocolError::EmptyPrompt);
        }
        if request.workdir.is_empty() {
            return Err(ProtocolError::MissingBody("agent working directory"));
        }
        let output_mode = request
            .output_mode
            .map(|value| decode_enum::<wire::OutputMode>(value, "output mode"))
            .transpose()?;
        let agent = decode_enum::<wire::ChatAgent>(request.agent, "agent")?;
        Ok(Self {
            text: request.text,
            workdir: request.workdir.into(),
            agent,
            model: Some(request.model).filter(|model| !model.is_empty()),
            session: Some(request.session).filter(|session| !session.is_empty()),
            new: request.new_conversation,
            output_mode,
            own_voice: request.own_voice,
        })
    }
}

impl TryFrom<wire::RunPromptRequest> for PromptCall {
    type Error = ProtocolError;

    fn try_from(request: wire::RunPromptRequest) -> Result<Self, Self::Error> {
        if request.text.trim().is_empty() {
            return Err(ProtocolError::EmptyPrompt);
        }
        let application = Some(request.application_id)
            .filter(|id| !id.is_empty())
            .map(ApplicationId::new)
            .transpose()?;
        Ok(Self {
            text: request.text,
            prompt: PromptId::new(request.prompt_id)?,
            application,
        })
    }
}

impl From<Ticket> for wire::AskResponse {
    fn from(ticket: Ticket) -> Self {
        Self { ticket: ticket.0 }
    }
}

impl From<wire::AskResponse> for Ticket {
    fn from(response: wire::AskResponse) -> Self {
        Self(response.ticket)
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
                error_code: code.into(),
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
                RpcErrorCode::try_from(event.error_code).map(Self::Failed)
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

impl From<&EnrolledSpeaker> for wire::EnrolledSpeaker {
    fn from(speaker: &EnrolledSpeaker) -> Self {
        Self {
            name: speaker.name.to_string(),
            role: wire::SpeakerRole::from(speaker.role) as i32,
            enrolled_at_seconds: speaker.enrolled_at,
        }
    }
}

impl TryFrom<&wire::EnrolledSpeaker> for EnrolledSpeaker {
    type Error = ProtocolError;

    fn try_from(speaker: &wire::EnrolledSpeaker) -> Result<Self, Self::Error> {
        Ok(Self {
            name: speaker_name(&speaker.name)?,
            role: decode_role(speaker.role)?.ok_or(ProtocolError::InvalidEnum {
                field: "role",
                value: speaker.role,
            })?,
            enrolled_at: speaker.enrolled_at_seconds,
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

impl TryFrom<wire::StartListenRequest> for Option<std::path::PathBuf> {
    type Error = ProtocolError;

    fn try_from(request: wire::StartListenRequest) -> Result<Self, Self::Error> {
        request
            .transcript_path
            .map(|path| {
                let output = std::path::PathBuf::from(&path);
                if !output.is_absolute() || path.contains('\0') {
                    return Err(ProtocolError::InvalidListenPath);
                }
                Ok(output)
            })
            .transpose()
    }
}

impl From<ListenStage> for wire::Event {
    fn from(stage: ListenStage) -> Self {
        Self {
            payload: Some(wire::event::Payload::Listen(stage.into())),
        }
    }
}

impl From<ListenStage> for wire::ListenEvent {
    fn from(stage: ListenStage) -> Self {
        match stage {
            ListenStage::SegmentTranscribed {
                text,
                label,
                at_seconds,
            } => Self {
                stage: wire::ListenStage::SegmentTranscribed as i32,
                text: text.to_string(),
                label: label.map(|label| label.to_string()).unwrap_or_default(),
                merged_from: String::new(),
                offset_seconds: at_seconds,
                error_code: 0,
            },
            ListenStage::SpeakerMerged { from, into } => Self {
                stage: wire::ListenStage::SpeakerMerged as i32,
                text: String::new(),
                label: into.to_string(),
                merged_from: from.to_string(),
                offset_seconds: 0,
                error_code: 0,
            },
            ListenStage::Stopped => Self {
                stage: wire::ListenStage::Stopped as i32,
                text: String::new(),
                label: String::new(),
                merged_from: String::new(),
                offset_seconds: 0,
                error_code: 0,
            },
            ListenStage::Failed(code) => Self {
                stage: wire::ListenStage::Failed as i32,
                text: String::new(),
                label: String::new(),
                merged_from: String::new(),
                offset_seconds: 0,
                error_code: code.into(),
            },
        }
    }
}

impl TryFrom<&wire::ListenEvent> for ListenStage {
    type Error = ProtocolError;

    fn try_from(event: &wire::ListenEvent) -> Result<Self, Self::Error> {
        match wire::ListenStage::try_from(event.stage) {
            Ok(wire::ListenStage::SegmentTranscribed) => Ok(Self::SegmentTranscribed {
                text: Arc::from(event.text.as_str()),
                label: (!event.label.is_empty()).then(|| Arc::from(event.label.as_str())),
                at_seconds: event.offset_seconds,
            }),
            Ok(wire::ListenStage::SpeakerMerged) => Ok(Self::SpeakerMerged {
                from: Arc::from(event.merged_from.as_str()),
                into: Arc::from(event.label.as_str()),
            }),
            Ok(wire::ListenStage::Stopped) => Ok(Self::Stopped),
            Ok(wire::ListenStage::Failed) => {
                RpcErrorCode::try_from(event.error_code).map(Self::Failed)
            }
            Ok(wire::ListenStage::Unspecified) | Err(..) => Err(ProtocolError::InvalidEnum {
                field: "stage",
                value: event.stage,
            }),
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
            SpeechStage::Failed(code) => (wire::SpeechStage::Failed, code.into()),
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
            Ok(wire::SpeechStage::Failed) => match RpcErrorCode::try_from(event.error_code) {
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
        Self {
            output_mode: request
                .output_mode
                .map(|mode| wire::OutputMode::from(mode) as i32),
            delivery_mode: request
                .delivery_mode
                .map(|mode| wire::DeliveryMode::from(mode) as i32),
            stop_mode: wire::StopMode::from(request.stop_mode) as i32,
            transcript_only: request.transcript_only,
            project: request.project.into(),
        }
    }
}

impl TryFrom<wire::StartDictationRequest> for DictationRequest {
    type Error = ProtocolError;

    fn try_from(request: wire::StartDictationRequest) -> Result<Self, Self::Error> {
        let output_mode = request
            .output_mode
            .map(|value| decode_enum::<wire::OutputMode>(value, "output mode"))
            .transpose()?;
        let delivery_mode = request
            .delivery_mode
            .map(|value| decode_enum::<wire::DeliveryMode>(value, "delivery mode"))
            .transpose()?;
        let stop_mode = decode_enum::<wire::StopMode>(request.stop_mode, "stop mode")?;

        Ok(Self {
            output_mode,
            delivery_mode,
            stop_mode,
            transcript_only: request.transcript_only,
            project: request.project.try_into()?,
        })
    }
}

impl From<crate::vocabulary::Context> for Option<wire::start_dictation_request::Project> {
    fn from(context: crate::vocabulary::Context) -> Self {
        use crate::vocabulary::Context;
        use wire::start_dictation_request::Project;
        match context {
            Context::Default => None,
            Context::Directory(path) => Some(Project::ProjectDirectory(path.as_str().to_owned())),
            Context::Disabled => Some(Project::ProjectVocabularyDisabled(
                wire::ProjectVocabularyDisabled {},
            )),
        }
    }
}

impl TryFrom<Option<wire::start_dictation_request::Project>> for crate::vocabulary::Context {
    type Error = ProtocolError;
    fn try_from(
        project: Option<wire::start_dictation_request::Project>,
    ) -> Result<Self, Self::Error> {
        use wire::start_dictation_request::Project;
        Ok(match project {
            None => Self::Default,
            Some(Project::ProjectDirectory(path)) => {
                Self::Directory(crate::vocabulary::Directory::new(path.into())?)
            }
            Some(Project::ProjectVocabularyDisabled(_)) => Self::Disabled,
        })
    }
}

impl From<crate::vocabulary::Status> for wire::VocabularyStatus {
    fn from(status: crate::vocabulary::Status) -> Self {
        use crate::vocabulary::State;
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

impl TryFrom<wire::ListHistoryRequest> for crate::history::HistoryQuery {
    type Error = ProtocolError;

    fn try_from(request: wire::ListHistoryRequest) -> Result<Self, Self::Error> {
        if request.query.len() > 256
            || !(1..=100).contains(&request.limit)
            || request
                .before_id
                .is_some_and(|id| id == 0 || id > i64::MAX as u64)
        {
            return Err(ProtocolError::InvalidHistory("query"));
        }
        Ok(Self {
            query: request.query,
            before_id: request.before_id,
            limit: request.limit as usize,
        })
    }
}

pub(crate) fn history_id(id: u64) -> Result<u64, ProtocolError> {
    if id == 0 || id > i64::MAX as u64 {
        return Err(ProtocolError::InvalidHistory("id"));
    }
    Ok(id)
}

impl TryFrom<wire::RegenerateHistoryRequest> for (u64, crate::history::RetryOptions) {
    type Error = ProtocolError;

    fn try_from(request: wire::RegenerateHistoryRequest) -> Result<Self, Self::Error> {
        let id = history_id(request.id)?;
        if request
            .engine
            .as_deref()
            .is_some_and(|engine| crate::config::backend::local(engine).is_none())
        {
            return Err(ProtocolError::InvalidHistory("engine"));
        }
        Ok((
            id,
            crate::history::RetryOptions {
                engine: request.engine,
                language: request.language.as_deref().map(str::parse).transpose()?,
            },
        ))
    }
}

impl TryFrom<wire::HistoryEntry> for crate::history::HistoryEntry {
    type Error = ProtocolError;

    fn try_from(entry: wire::HistoryEntry) -> Result<Self, Self::Error> {
        use crate::history::{Application, AudioAvailability, HistorySource, HistoryStatus};
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
        if entry.audio_expires_at_ms.is_some()
            && entry.audio != wire::HistoryAudioAvailability::Available as i32
        {
            return Err(ProtocolError::InvalidHistory("audio expiry"));
        }
        let audio = match wire::HistoryAudioAvailability::try_from(entry.audio) {
            Ok(wire::HistoryAudioAvailability::Unavailable) => AudioAvailability::Unavailable,
            Ok(wire::HistoryAudioAvailability::Expired) => AudioAvailability::Expired,
            Ok(wire::HistoryAudioAvailability::Available) => AudioAvailability::Available {
                expires_at_ms: entry
                    .audio_expires_at_ms
                    .ok_or(ProtocolError::InvalidHistory("audio expiry"))?,
            },
            _ => return Err(ProtocolError::InvalidHistory("audio")),
        };
        Ok(Self {
            id: entry.id,
            created_at_ms: entry.created_at_ms,
            source,
            application: entry.application.map(|app| Application {
                name: app.name,
                bundle_id: app.bundle_id,
            }),
            duration_ms: entry.duration_ms,
            text: entry.text,
            complete: entry.transcription_complete,
            status,
            error: entry.error.map(TryInto::try_into).transpose()?,
            audio,
            retrying: entry.regenerating,
            deletion_pending: entry.deletion_pending,
            language: entry.language,
            engine: entry.engine,
            own_voice_only: false,
            project: None,
            warning: entry.warning,
        })
    }
}

impl TryFrom<wire::ListHistoryResponse> for crate::history::HistoryPage {
    type Error = ProtocolError;

    fn try_from(page: wire::ListHistoryResponse) -> Result<Self, Self::Error> {
        if page
            .next_before_id
            .is_some_and(|id| id == 0 || id > i64::MAX as u64)
        {
            return Err(ProtocolError::InvalidHistory("cursor"));
        }
        Ok(Self {
            entries: page
                .entries
                .into_iter()
                .map(TryInto::try_into)
                .collect::<Result<_, _>>()?,
            next_before_id: page.next_before_id,
            warning: page.warning,
        })
    }
}

impl From<crate::history::HistoryEntry> for wire::HistoryEntry {
    fn from(entry: crate::history::HistoryEntry) -> Self {
        use crate::history::{AudioAvailability, HistorySource, HistoryStatus};
        let (audio, audio_expires_at_ms) = match entry.audio {
            AudioAvailability::Unavailable => (wire::HistoryAudioAvailability::Unavailable, None),
            AudioAvailability::Expired => (wire::HistoryAudioAvailability::Expired, None),
            AudioAvailability::Available { expires_at_ms } => (
                wire::HistoryAudioAvailability::Available,
                Some(expires_at_ms),
            ),
        };
        Self {
            can_regenerate: matches!(entry.audio, AudioAvailability::Available { .. })
                && !entry.retrying
                && !entry.deletion_pending,
            id: entry.id,
            created_at_ms: entry.created_at_ms,
            source: match entry.source {
                HistorySource::Local => wire::HistorySource::Local,
                HistorySource::Relay => wire::HistorySource::Relay,
            } as i32,
            application: entry.application.map(|app| wire::HistoryApplication {
                name: app.name,
                bundle_id: app.bundle_id,
            }),
            duration_ms: entry.duration_ms,
            text: entry.text,
            transcription_complete: entry.complete,
            status: match entry.status {
                HistoryStatus::Success => wire::HistoryStatus::Success,
                HistoryStatus::TranscriptionFailed => wire::HistoryStatus::TranscriptionFailed,
                HistoryStatus::DeliveryFailed => wire::HistoryStatus::DeliveryFailed,
                HistoryStatus::Recovered => wire::HistoryStatus::Recovered,
            } as i32,
            error: entry.error.map(Into::into),
            audio: audio as i32,
            audio_expires_at_ms,
            regenerating: entry.retrying,
            deletion_pending: entry.deletion_pending,
            language: entry.language,
            engine: entry.engine,
            warning: entry.warning,
        }
    }
}

impl From<crate::history::HistoryPage> for wire::ListHistoryResponse {
    fn from(page: crate::history::HistoryPage) -> Self {
        Self {
            entries: page
                .entries
                .into_iter()
                .map(|entry| {
                    let mut entry = wire::HistoryEntry::from(entry);
                    let mut end = entry.text.len().min(512);
                    while !entry.text.is_char_boundary(end) {
                        end -= 1;
                    }
                    entry.text.truncate(end);
                    entry
                })
                .collect(),
            next_before_id: page.next_before_id,
            warning: page.warning,
        }
    }
}

impl TryFrom<wire::VocabularyRequest> for Option<crate::vocabulary::Directory> {
    type Error = ProtocolError;

    fn try_from(request: wire::VocabularyRequest) -> Result<Self, Self::Error> {
        request
            .directory
            .map(|path| crate::vocabulary::Directory::new(path.into()).map_err(Into::into))
            .transpose()
    }
}

impl TryFrom<wire::VocabularyStatus> for crate::vocabulary::Status {
    type Error = ProtocolError;
    fn try_from(status: wire::VocabularyStatus) -> Result<Self, Self::Error> {
        use crate::vocabulary::State;
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

impl From<HotkeyKey> for wire::HotkeyKey {
    fn from(key: HotkeyKey) -> Self {
        match key {
            HotkeyKey::RightOption => Self::RightOption,
            HotkeyKey::Fn => Self::Fn,
        }
    }
}

impl WireEnum for wire::HotkeyKey {
    type Domain = HotkeyKey;

    fn into_domain(self) -> Option<Self::Domain> {
        match self {
            Self::RightOption => Some(HotkeyKey::RightOption),
            Self::Fn => Some(HotkeyKey::Fn),
            Self::Unspecified => None,
        }
    }
}

impl From<(u64, PreferencePatch)> for wire::UpdatePreferencesRequest {
    fn from((expected_revision, patch): (u64, PreferencePatch)) -> Self {
        Self {
            expected_revision,
            patch: Some(patch.into()),
        }
    }
}

impl TryFrom<wire::UpdatePreferencesRequest> for (u64, PreferencePatch) {
    type Error = ProtocolError;

    fn try_from(update: wire::UpdatePreferencesRequest) -> Result<Self, Self::Error> {
        let patch = update.patch.ok_or(ProtocolError::MissingPreferencePatch)?;
        Ok((update.expected_revision, patch.try_into()?))
    }
}

impl TryFrom<wire::PreferenceSnapshot> for PreferenceSnapshot {
    type Error = ProtocolError;

    fn try_from(snapshot: wire::PreferenceSnapshot) -> Result<Self, Self::Error> {
        let preferences = snapshot
            .preferences
            .ok_or(ProtocolError::MissingPreferences)?;
        Ok(Self {
            revision: snapshot.revision,
            preferences: UserPreferences {
                recognition_language: preferences.recognition_language.parse()?,
                output_mode: decode_enum::<wire::OutputMode>(
                    preferences.output_mode,
                    "output mode",
                )?,
                delivery_mode: decode_enum::<wire::DeliveryMode>(
                    preferences.delivery_mode,
                    "delivery mode",
                )?,
                silence_duration: SilenceDuration::new(Duration::from_millis(
                    preferences.silence_duration_ms,
                ))?,
                own_voice_only: preferences.own_voice_only,
                dictation_history: preferences.dictation_history,
                history_audio_retention: preferences.history_audio_retention,
                mute_output: preferences.mute_output,
                project_vocabulary: preferences.project_vocabulary,
                vocabulary_directory: preferences
                    .vocabulary_directory
                    .map(|path| crate::vocabulary::Directory::new(path.into()))
                    .transpose()?,
                dictation_hotkey: decode_enum::<wire::HotkeyKey>(
                    preferences.dictation_hotkey,
                    "hotkey key",
                )?,
                ask_hotkey: Option::<Chord>::try_from(
                    preferences
                        .ask_hotkey
                        .ok_or(ProtocolError::MissingBody("ask hotkey"))?,
                )?,
                prompt_list_hotkey: Option::<Chord>::try_from(
                    preferences
                        .prompt_list_hotkey
                        .ok_or(ProtocolError::MissingBody("prompt list hotkey"))?,
                )?,
                application_overrides: preferences
                    .application_overrides
                    .ok_or(ProtocolError::MissingBody("application overrides"))?
                    .try_into()?,
                prompts: preferences
                    .prompts
                    .ok_or(ProtocolError::MissingBody("prompts"))?
                    .try_into()?,
            },
        })
    }
}

impl From<PreferencePatch> for wire::PreferencePatch {
    fn from(patch: PreferencePatch) -> Self {
        Self {
            recognition_language: patch
                .recognition_language
                .map(|language| language.as_str().to_owned()),
            output_mode: patch
                .output_mode
                .map(|mode| wire::OutputMode::from(mode) as i32),
            delivery_mode: patch
                .delivery_mode
                .map(|mode| wire::DeliveryMode::from(mode) as i32),
            silence_duration_ms: patch
                .silence_duration
                .map(|duration| duration.get().as_millis() as u64),
            own_voice_only: patch.own_voice_only,
            dictation_history: patch.dictation_history,
            history_audio_retention: patch.history_audio_retention,
            mute_output: patch.mute_output,
            dictation_hotkey: patch
                .dictation_hotkey
                .map(|key| wire::HotkeyKey::from(key) as i32),
            ask_hotkey: patch.ask_hotkey.map(wire::Hotkey::from),
            prompt_list_hotkey: patch.prompt_list_hotkey.map(wire::Hotkey::from),
            prompts: patch.prompts.as_ref().map(wire::Prompts::from),
            project_vocabulary: patch.project_vocabulary,
            vocabulary_directory: patch
                .vocabulary_directory
                .map(|path| path.map_or_else(String::new, |path| path.as_str().to_owned())),
            application_overrides: patch
                .application_overrides
                .as_ref()
                .map(wire::ApplicationOverrides::from),
        }
    }
}

impl From<&PreferenceSnapshot> for wire::PreferenceSnapshot {
    fn from(snapshot: &PreferenceSnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            preferences: Some(wire::UserPreferences {
                recognition_language: snapshot
                    .preferences
                    .recognition_language
                    .as_str()
                    .to_owned(),
                output_mode: wire::OutputMode::from(snapshot.preferences.output_mode) as i32,
                delivery_mode: wire::DeliveryMode::from(snapshot.preferences.delivery_mode) as i32,
                silence_duration_ms: snapshot.preferences.silence_duration.get().as_millis() as u64,
                own_voice_only: snapshot.preferences.own_voice_only,
                dictation_history: snapshot.preferences.dictation_history,
                history_audio_retention: snapshot.preferences.history_audio_retention,
                mute_output: snapshot.preferences.mute_output,
                project_vocabulary: snapshot.preferences.project_vocabulary,
                vocabulary_directory: snapshot
                    .preferences
                    .vocabulary_directory
                    .as_ref()
                    .map(|path| path.as_str().to_owned()),
                dictation_hotkey: wire::HotkeyKey::from(snapshot.preferences.dictation_hotkey)
                    as i32,
                ask_hotkey: Some(snapshot.preferences.ask_hotkey.into()),
                prompt_list_hotkey: Some(snapshot.preferences.prompt_list_hotkey.into()),
                application_overrides: Some((&snapshot.preferences.application_overrides).into()),
                prompts: Some((&snapshot.preferences.prompts).into()),
            }),
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

impl TryFrom<wire::PreferencePatch> for PreferencePatch {
    type Error = ProtocolError;

    fn try_from(patch: wire::PreferencePatch) -> Result<Self, Self::Error> {
        Ok(Self {
            recognition_language: patch
                .recognition_language
                .as_deref()
                .map(str::parse)
                .transpose()?,
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
            own_voice_only: patch.own_voice_only,
            dictation_history: patch.dictation_history,
            history_audio_retention: patch.history_audio_retention,
            mute_output: patch.mute_output,
            dictation_hotkey: patch
                .dictation_hotkey
                .map(|value| decode_enum::<wire::HotkeyKey>(value, "hotkey key"))
                .transpose()?,
            ask_hotkey: patch
                .ask_hotkey
                .map(Option::<Chord>::try_from)
                .transpose()?,
            prompt_list_hotkey: patch
                .prompt_list_hotkey
                .map(Option::<Chord>::try_from)
                .transpose()?,
            prompts: patch.prompts.map(Prompts::try_from).transpose()?,
            project_vocabulary: patch.project_vocabulary,
            vocabulary_directory: patch
                .vocabulary_directory
                .map(|path| {
                    if path.is_empty() {
                        Ok(None)
                    } else {
                        crate::vocabulary::Directory::new(path.into()).map(Some)
                    }
                })
                .transpose()?,
            application_overrides: patch
                .application_overrides
                .map(ApplicationOverrides::try_from)
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

impl From<AskTarget> for wire::ResolveAskResponse {
    fn from(target: AskTarget) -> Self {
        Self {
            workdir: target.workdir.to_string_lossy().into_owned(),
            agent: wire::ChatAgent::from(target.agent) as i32,
        }
    }
}

impl TryFrom<wire::ResolveAskResponse> for AskTarget {
    type Error = ProtocolError;

    fn try_from(response: wire::ResolveAskResponse) -> Result<Self, Self::Error> {
        if response.workdir.is_empty() {
            return Err(ProtocolError::MissingBody("agent working directory"));
        }
        Ok(Self {
            workdir: response.workdir.into(),
            agent: decode_enum::<wire::ChatAgent>(response.agent, "agent")?,
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

impl TryFrom<wire::Hotkey> for Option<Chord> {
    type Error = ProtocolError;

    fn try_from(hotkey: wire::Hotkey) -> Result<Self, Self::Error> {
        hotkey.chord.map(Chord::try_from).transpose()
    }
}

impl From<Option<Chord>> for wire::Hotkey {
    fn from(chord: Option<Chord>) -> Self {
        Self {
            chord: chord.map(wire::Chord::from),
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
    }
}

/// An unknown code reads as no outcome: a newer daemon may add one, and a
/// pill that says nothing beats one that fails the whole event.
fn delivery_outcome(code: i32) -> Option<DeliveryOutcome> {
    match wire::DeliveryOutcome::try_from(code) {
        Ok(wire::DeliveryOutcome::Inserted) => Some(DeliveryOutcome::Inserted),
        Ok(wire::DeliveryOutcome::Copied) => Some(DeliveryOutcome::Copied),
        Ok(wire::DeliveryOutcome::Unspecified) | Err(..) => None,
    }
}

impl From<DictationStage> for wire::DictationEvent {
    fn from(stage: DictationStage) -> Self {
        let mut text = String::new();
        let mut recording = 0;
        let mut outcome = wire::DeliveryOutcome::Unspecified;
        let mut words = 0;
        let (code, error) = match stage {
            DictationStage::Recording(id) => {
                recording = id.0;
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
            DictationStage::Failed(code) => (wire::DictationStage::Failed, code.into()),
            DictationStage::SegmentTranscribed(segment) => {
                text = segment.to_string();
                (wire::DictationStage::SegmentTranscribed, 0)
            }
            DictationStage::SegmentDelivered => (wire::DictationStage::SegmentDelivered, 0),
        };
        Self {
            stage: code as i32,
            text,
            error_code: error,
            recording_id: recording,
            outcome: outcome as i32,
            word_count: words,
        }
    }
}

impl TryFrom<&wire::DictationEvent> for DictationStage {
    type Error = ProtocolError;

    fn try_from(event: &wire::DictationEvent) -> Result<Self, Self::Error> {
        match wire::DictationStage::try_from(event.stage) {
            Ok(wire::DictationStage::Recording) => {
                Ok(Self::Recording(RecordingId(event.recording_id)))
            }
            Ok(wire::DictationStage::Transcribing) => Ok(Self::Transcribing),
            Ok(wire::DictationStage::Delivering) => Ok(Self::Delivering),
            Ok(wire::DictationStage::SegmentTranscribed) => {
                Ok(Self::SegmentTranscribed(Arc::from(event.text.as_str())))
            }
            Ok(wire::DictationStage::SegmentDelivered) => Ok(Self::SegmentDelivered),
            Ok(wire::DictationStage::Delivered) => Ok(Self::Delivered {
                outcome: delivery_outcome(event.outcome),
                words: event.word_count,
            }),
            Ok(wire::DictationStage::Cancelled) => Ok(Self::Cancelled),
            // 0 is rejected rather than kept as Unknown(0): unknown codes
            // must round-trip, but UNSPECIFIED must never re-enter the wire.
            Ok(wire::DictationStage::Failed) => match RpcErrorCode::try_from(event.error_code) {
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

impl From<RpcErrorCode> for i32 {
    fn from(code: RpcErrorCode) -> Self {
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
            RpcErrorCode::AgentUnavailable => wire::ErrorCode::AgentUnavailable,
            RpcErrorCode::AgentFailed => wire::ErrorCode::AgentFailed,
            RpcErrorCode::AgentTimeout => wire::ErrorCode::AgentTimeout,
            RpcErrorCode::SpeakerModelMissing => wire::ErrorCode::SpeakerModelMissing,
            RpcErrorCode::SpeakerAlreadyEnrolled => wire::ErrorCode::SpeakerAlreadyEnrolled,
            RpcErrorCode::SpeakerNotEnrolled => wire::ErrorCode::SpeakerNotEnrolled,
            RpcErrorCode::SpeakerInconsistentSamples => wire::ErrorCode::SpeakerInconsistentSamples,
            RpcErrorCode::SpeakerProfileWriteFailed => wire::ErrorCode::SpeakerProfileWriteFailed,
            RpcErrorCode::SpeakerNoSpeech => wire::ErrorCode::SpeakerNoSpeech,
            RpcErrorCode::ListenUnsupportedOs => wire::ErrorCode::ListenUnsupportedOs,
            RpcErrorCode::ListenPermissionDenied => wire::ErrorCode::ListenPermissionDenied,
            RpcErrorCode::ListenCaptureFailed => wire::ErrorCode::ListenCaptureFailed,
            RpcErrorCode::HistoryMissing => wire::ErrorCode::HistoryMissing,
            RpcErrorCode::HistoryAudioUnavailable => wire::ErrorCode::HistoryAudioUnavailable,
            RpcErrorCode::HistoryUnavailable => wire::ErrorCode::HistoryUnavailable,
            RpcErrorCode::HistoryProjectUnavailable => wire::ErrorCode::HistoryProjectUnavailable,
            RpcErrorCode::Unknown(value) => return value,
        };
        encoded as Self
    }
}

impl TryFrom<i32> for RpcErrorCode {
    type Error = ProtocolError;

    fn try_from(code: i32) -> Result<Self, Self::Error> {
        let decoded = match wire::ErrorCode::try_from(code) {
            Ok(wire::ErrorCode::InvalidRequest) => Self::InvalidRequest,
            Ok(wire::ErrorCode::Busy) => Self::Busy,
            Ok(wire::ErrorCode::Internal) => Self::Internal,
            Ok(wire::ErrorCode::Cancelled) => Self::Cancelled,
            Ok(wire::ErrorCode::Unauthenticated) => Self::Unauthenticated,
            Ok(wire::ErrorCode::Unsupported) => Self::Unsupported,
            Ok(wire::ErrorCode::ShuttingDown) => Self::ShuttingDown,
            Ok(wire::ErrorCode::PayloadTooLarge) => Self::PayloadTooLarge,
            Ok(wire::ErrorCode::PermissionDenied) => Self::PermissionDenied,
            Ok(wire::ErrorCode::DurabilityUnconfirmed) => Self::DurabilityUnconfirmed,
            Ok(wire::ErrorCode::OverallTimeout) => Self::OverallTimeout,
            Ok(wire::ErrorCode::RecordingTimeout) => Self::RecordingTimeout,
            Ok(wire::ErrorCode::DeliveryTimeout) => Self::DeliveryTimeout,
            Ok(wire::ErrorCode::CaptureFailed) => Self::CaptureFailed,
            Ok(wire::ErrorCode::NoSpeech) => Self::NoSpeech,
            Ok(wire::ErrorCode::TranscriptionFailed) => Self::TranscriptionFailed,
            Ok(wire::ErrorCode::DeliveryFailed) => Self::DeliveryFailed,
            Ok(wire::ErrorCode::DeliveryTargetChanged) => Self::DeliveryTargetChanged,
            Ok(wire::ErrorCode::DeliveryInterrupted) => Self::DeliveryInterrupted,
            Ok(wire::ErrorCode::ApplicationExcluded) => Self::ApplicationExcluded,
            Ok(wire::ErrorCode::SynthesisFailed) => Self::SynthesisFailed,
            Ok(wire::ErrorCode::PlaybackFailed) => Self::PlaybackFailed,
            Ok(wire::ErrorCode::SpeechTimeout) => Self::SpeechTimeout,
            Ok(wire::ErrorCode::VoiceUnavailable) => Self::VoiceUnavailable,
            Ok(wire::ErrorCode::AgentUnavailable) => Self::AgentUnavailable,
            Ok(wire::ErrorCode::AgentFailed) => Self::AgentFailed,
            Ok(wire::ErrorCode::AgentTimeout) => Self::AgentTimeout,
            Ok(wire::ErrorCode::SpeakerModelMissing) => Self::SpeakerModelMissing,
            Ok(wire::ErrorCode::SpeakerAlreadyEnrolled) => Self::SpeakerAlreadyEnrolled,
            Ok(wire::ErrorCode::SpeakerNotEnrolled) => Self::SpeakerNotEnrolled,
            Ok(wire::ErrorCode::SpeakerInconsistentSamples) => Self::SpeakerInconsistentSamples,
            Ok(wire::ErrorCode::SpeakerProfileWriteFailed) => Self::SpeakerProfileWriteFailed,
            Ok(wire::ErrorCode::SpeakerNoSpeech) => Self::SpeakerNoSpeech,
            Ok(wire::ErrorCode::ListenUnsupportedOs) => Self::ListenUnsupportedOs,
            Ok(wire::ErrorCode::ListenPermissionDenied) => Self::ListenPermissionDenied,
            Ok(wire::ErrorCode::ListenCaptureFailed) => Self::ListenCaptureFailed,
            Ok(wire::ErrorCode::HistoryMissing) => Self::HistoryMissing,
            Ok(wire::ErrorCode::HistoryAudioUnavailable) => Self::HistoryAudioUnavailable,
            Ok(wire::ErrorCode::HistoryUnavailable) => Self::HistoryUnavailable,
            Ok(wire::ErrorCode::HistoryProjectUnavailable) => Self::HistoryProjectUnavailable,
            Ok(wire::ErrorCode::Unspecified) => {
                return Err(ProtocolError::InvalidEnum {
                    field: "error code",
                    value: code,
                });
            }
            Err(_unknown) => Self::Unknown(code),
        };
        Ok(decoded)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid inference message")]
    InvalidInference,
    #[error("invalid history {0}")]
    InvalidHistory(&'static str),
    #[error("invalid vocabulary term count {0}")]
    InvalidVocabularyTerms(u64),
    #[error(transparent)]
    InvalidVocabularyDirectory(#[from] crate::vocabulary::DirectoryError),
    #[error("invalid {field} value {value}")]
    InvalidEnum { field: &'static str, value: i32 },

    #[error("the transcript path must be absolute and contain no NUL bytes")]
    InvalidListenPath,

    #[error("missing {0}")]
    MissingBody(&'static str),

    #[error("the prompt must not be empty")]
    EmptyPrompt,

    #[error("preference update is missing its patch")]
    MissingPreferencePatch,

    #[error("preference snapshot is missing its preferences")]
    MissingPreferences,

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
}

#[cfg(test)]
mod tests {
    use crate::dictation::DictationError;

    use super::*;

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
        RpcErrorCode::try_from(code).map_or("unspecified", |code| code.label())
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
            let sent = i32::from(RpcErrorCode::from(failure));

            assert_eq!(
                RpcErrorCode::try_from(sent).ok(),
                Some(failure.into()),
                "{failure}"
            );
            assert_eq!(error_code_label(sent), failure.status(), "{failure}");
        }
    }

    #[test]
    fn a_failure_round_trips_through_the_status_details() {
        let failure = RpcFailure::new(RpcErrorCode::NoSpeech, "no speech was detected");

        let status = Status::from(failure.clone());

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
    fn the_zero_code_names_no_failure() {
        let unspecified = wire::ErrorCode::Unspecified as i32;

        assert!(RpcErrorCode::try_from(unspecified).is_err());
        assert_eq!(error_code_label(unspecified), "unspecified");
    }

    #[test]
    fn caption_snapshots_use_a_separate_payload_and_redact_debug_text() {
        for ended in [false, true] {
            let caption = CaptionSnapshot {
                recording: RecordingId(42),
                committed: Arc::from("private committed words"),
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
            assert_eq!(snapshot.committed, "private committed words");
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
                font: Some(crate::delivery::Font {
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
            DictationStage::Recording(RecordingId(7)),
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
            DictationStage::Cancelled,
            DictationStage::Failed(RpcErrorCode::NoSpeech),
            DictationStage::Failed(RpcErrorCode::Unknown(9_999)),
            DictationStage::SegmentTranscribed(Arc::from("a segment")),
            DictationStage::SegmentDelivered,
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
            }),
            ChatStageKind::Replied(ChatReply {
                text: "hi".to_owned(),
                delivery_error: Some(RpcErrorCode::DeliveryTargetChanged),
                copied: false,
            }),
            ChatStageKind::Failed {
                code: RpcErrorCode::AgentUnavailable,
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
                agent: ChatAgent::Codex,
                kind,
            };
            let sent = wire::ChatEvent::from(stage.clone());

            assert_eq!(
                ChatStage::try_from(&sent).expect("the stage should decode"),
                stage
            );
            assert!(matches!(
                ChatStage::try_from(&wire::ChatEvent { agent: 0, ..sent }),
                Err(ProtocolError::InvalidEnum { field: "agent", .. })
            ));
        }
        // An older daemon's THINKING carried no step at all.
        let stale = wire::ChatEvent {
            stage: wire::ChatStage::Working as i32,
            agent: wire::ChatAgent::Codex as i32,
            ticket: 7,
            ..Default::default()
        };
        assert!(matches!(
            ChatStage::try_from(&stale),
            Err(ProtocolError::InvalidEnum { field: "step", .. })
        ));
    }

    #[test]
    fn an_ask_request_round_trips_and_a_bad_one_is_refused() {
        let request = ChatRequest {
            text: "say hi".to_owned(),
            workdir: "/tmp".into(),
            agent: ChatAgent::Codex,
            model: Some("mini".to_owned()),
            session: Some("planning".to_owned()),
            new: true,
            output_mode: Some(OutputMode::Copy),
            own_voice: true,
        };

        let sent = wire::AskRequest::from(request.clone());
        assert_eq!(
            ChatRequest::try_from(sent).expect("the request should decode"),
            request
        );

        let bare = wire::AskRequest::from(ChatRequest {
            model: None,
            session: None,
            ..request
        });
        assert_eq!(
            ChatRequest::try_from(bare.clone()).expect("decodes").model,
            None
        );
        assert!(matches!(
            ChatRequest::try_from(wire::AskRequest {
                text: "  ".to_owned(),
                ..bare.clone()
            }),
            Err(ProtocolError::EmptyPrompt)
        ));
        assert!(matches!(
            ChatRequest::try_from(wire::AskRequest { agent: 0, ..bare }),
            Err(ProtocolError::InvalidEnum { field: "agent", .. })
        ));
    }

    #[test]
    fn a_ticket_round_trips() {
        let sent = wire::AskResponse::from(Ticket(42));
        assert_eq!(Ticket::from(sent), Ticket(42));
        let cancel = wire::CancelAskRequest::from(Ticket(42));
        assert_eq!(Ticket::from(cancel), Ticket(42));
    }

    #[test]
    fn a_dictation_request_keeps_transcript_only_across_the_wire() {
        let request = DictationRequest {
            output_mode: Some(OutputMode::Copy),
            delivery_mode: Some(DeliveryMode::Complete),
            stop_mode: StopMode::Manual,
            transcript_only: true,
            project: crate::vocabulary::Context::Default,
        };

        let sent = wire::StartDictationRequest::from(request.clone());
        assert_eq!(
            DictationRequest::try_from(sent).expect("the request should decode"),
            request
        );
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
    fn history_audio_retention_preserves_wire_presence() {
        for history_audio_retention in [None, Some(false), Some(true)] {
            let patch = PreferencePatch {
                history_audio_retention,
                ..Default::default()
            };
            assert_eq!(
                PreferencePatch::try_from(wire::PreferencePatch::from(patch.clone())).unwrap(),
                patch
            );
            let mut snapshot = wire::PreferenceSnapshot::from(&PreferenceSnapshot::default());
            snapshot
                .preferences
                .as_mut()
                .unwrap()
                .history_audio_retention = history_audio_retention.unwrap_or(false);
            let decoded = PreferenceSnapshot::try_from(snapshot).unwrap();
            assert_eq!(
                decoded.preferences.history_audio_retention,
                history_audio_retention.unwrap_or(false)
            );
            assert_eq!(
                PreferenceSnapshot::try_from(wire::PreferenceSnapshot::from(&decoded)).unwrap(),
                decoded
            );
        }
    }

    #[test]
    fn mute_output_preserves_snapshot_value_and_patch_presence_on_the_wire() {
        for mute_output in [false, true] {
            let snapshot = PreferenceSnapshot {
                revision: 4,
                preferences: UserPreferences {
                    mute_output,
                    ..Default::default()
                },
            };
            let encoded = wire::PreferenceSnapshot::from(&snapshot).encode_to_vec();
            let decoded = wire::PreferenceSnapshot::decode(encoded.as_slice()).unwrap();
            assert_eq!(PreferenceSnapshot::try_from(decoded).unwrap(), snapshot);
        }
        for mute_output in [None, Some(false), Some(true)] {
            let patch = PreferencePatch {
                mute_output,
                ..Default::default()
            };
            let encoded = wire::PreferencePatch::from(patch.clone()).encode_to_vec();
            let decoded = wire::PreferencePatch::decode(encoded.as_slice()).unwrap();
            assert_eq!(PreferencePatch::try_from(decoded).unwrap(), patch);
        }
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
        let snapshot = PreferenceSnapshot {
            revision: 2,
            preferences: UserPreferences {
                application_overrides: overrides.clone(),
                ..Default::default()
            },
        };
        let encoded = wire::PreferenceSnapshot::from(&snapshot).encode_to_vec();
        let decoded = wire::PreferenceSnapshot::decode(encoded.as_slice()).unwrap();
        assert_eq!(PreferenceSnapshot::try_from(decoded).unwrap(), snapshot);

        for application_overrides in [None, Some(ApplicationOverrides::default()), Some(overrides)]
        {
            let patch = PreferencePatch {
                application_overrides,
                ..Default::default()
            };
            let encoded = wire::PreferencePatch::from(patch.clone()).encode_to_vec();
            let decoded = wire::PreferencePatch::decode(encoded.as_slice()).unwrap();
            assert_eq!(PreferencePatch::try_from(decoded).unwrap(), patch);
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
        use crate::vocabulary::{Context, Directory};
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
            ..Default::default()
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
                ..Default::default()
            };
            assert!(DictationRequest::try_from(request).is_err());
        }
        let patch = PreferencePatch {
            project_vocabulary: Some(false),
            vocabulary_directory: Some(None),
            ..Default::default()
        };
        assert_eq!(
            <(u64, PreferencePatch)>::try_from(wire::UpdatePreferencesRequest::from((
                3,
                patch.clone()
            )))
            .unwrap(),
            (3, patch)
        );
    }

    #[test]
    fn every_listen_stage_survives_the_round_trip() {
        let stages = [
            ListenStage::SegmentTranscribed {
                text: Arc::from("hello"),
                label: Some(Arc::from("Speaker 1")),
                at_seconds: 754,
            },
            ListenStage::SegmentTranscribed {
                text: Arc::from("hello"),
                label: None,
                at_seconds: 0,
            },
            ListenStage::SpeakerMerged {
                from: Arc::from("Speaker 3"),
                into: Arc::from("Speaker 1"),
            },
            ListenStage::Stopped,
            ListenStage::Failed(RpcErrorCode::ListenPermissionDenied),
        ];

        for stage in stages {
            let sent = wire::ListenEvent::from(stage.clone());

            assert_eq!(
                ListenStage::try_from(&sent).expect("the stage should decode"),
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
    fn an_unrecognized_code_survives_the_round_trip() {
        let future = 9_999;

        let decoded =
            RpcErrorCode::try_from(future).expect("an unknown code still names a failure");

        assert_eq!(decoded, RpcErrorCode::Unknown(future));
        assert_eq!(i32::from(decoded), future);
        assert_eq!(error_code_label(future), "unknown");
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
                Some(Chord::ASK),
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
    fn a_prompt_call_needs_words_a_prompt_id_and_at_most_a_real_app() {
        let call = |text: &str, id: &str, app: &str| {
            PromptCall::try_from(wire::RunPromptRequest {
                text: text.to_owned(),
                prompt_id: id.to_owned(),
                application_id: app.to_owned(),
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
    }

    #[test]
    fn a_hotkey_round_trips_and_needs_a_chord_with_a_modifier() {
        let chord = wire::Chord {
            key_code: 13,
            control: true,
            option: false,
            shift: true,
            command: false,
        };
        let hotkey =
            Option::<Chord>::try_from(wire::Hotkey { chord: Some(chord) }).expect("decodes");
        assert_eq!(wire::Hotkey::from(hotkey).chord, Some(chord));
        let cleared = PreferencePatch::try_from(wire::PreferencePatch {
            ask_hotkey: Some(wire::Hotkey { chord: None }),
            ..Default::default()
        })
        .expect("decodes");
        assert_eq!(cleared.ask_hotkey, Some(None));

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
    fn listen_output_requires_an_absolute_path() {
        let decode = |path: Option<&str>| {
            Option::<std::path::PathBuf>::try_from(wire::StartListenRequest {
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
                Err(ProtocolError::InvalidListenPath)
            ));
        }
    }

    #[test]
    fn an_ask_target_round_trips_and_an_empty_one_is_refused() {
        let target = AskTarget {
            workdir: "/tmp".into(),
            agent: ChatAgent::ClaudeCode,
        };

        let decoded = AskTarget::try_from(wire::ResolveAskResponse::from(target.clone()))
            .expect("the target should decode");

        assert_eq!(decoded, target);
        assert!(
            AskTarget::try_from(wire::ResolveAskResponse {
                workdir: String::new(),
                agent: wire::ChatAgent::ClaudeCode as i32,
            })
            .is_err()
        );
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use crate::history::{HistoryEntry, HistoryPage, HistoryQuery};
    use crate::transcription::Language;

    fn entry() -> HistoryEntry {
        HistoryEntry {
            id: 42,
            created_at_ms: 1_000,
            source: crate::history::HistorySource::Local,
            application: None,
            duration_ms: None,
            text: "界".repeat(200),
            complete: false,
            status: crate::history::HistoryStatus::TranscriptionFailed,
            error: None,
            warning: None,
            audio: crate::history::AudioAvailability::Available {
                expires_at_ms: 2_000,
            },
            retrying: false,
            deletion_pending: false,
            language: "en".to_owned(),
            engine: String::new(),
            own_voice_only: false,
            project: None,
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
                    query,
                    before_id,
                    limit
                })
                .is_err()
            );
        }
        assert!(
            HistoryQuery::try_from(wire::ListHistoryRequest {
                query: "a_%".to_owned(),
                before_id: Some(2),
                limit: 100
            })
            .is_ok()
        );
        assert!(history_id(0).is_err());
        assert!(history_id(u64::MAX).is_err());
    }

    #[test]
    fn history_retry_options_are_validated_and_successful_audio_is_retryable() {
        let request = wire::RegenerateHistoryRequest {
            id: 42,
            engine: Some("whisper".into()),
            language: Some("auto".into()),
        };
        let (id, options) = <(u64, crate::history::RetryOptions)>::try_from(request).unwrap();
        assert_eq!(id, 42);
        assert_eq!(options.engine.as_deref(), Some("whisper"));
        assert_eq!(options.language, Some(Language::Automatic));
        for (engine, language) in [(Some("unknown"), None), (Some(""), None), (None, Some(""))] {
            assert!(
                <(u64, crate::history::RetryOptions)>::try_from(wire::RegenerateHistoryRequest {
                    id: 42,
                    engine: engine.map(str::to_owned),
                    language: language.map(str::to_owned),
                })
                .is_err()
            );
        }
        let mut original = entry();
        original.status = crate::history::HistoryStatus::Success;
        let encoded = wire::HistoryEntry::from(original);
        assert!(encoded.can_regenerate);
        let decoded = HistoryEntry::try_from(encoded.clone()).unwrap();
        assert_eq!(decoded.status, crate::history::HistoryStatus::Success);
        assert_eq!(
            decoded.audio,
            crate::history::AudioAvailability::Available {
                expires_at_ms: 2_000
            }
        );
        for audio in [
            wire::HistoryAudioAvailability::Unavailable,
            wire::HistoryAudioAvailability::Expired,
        ] {
            let mut unexpected_expiry = encoded.clone();
            unexpected_expiry.audio = audio as i32;
            assert!(HistoryEntry::try_from(unexpected_expiry).is_err());
        }
        let mut missing_expiry = encoded;
        missing_expiry.audio_expires_at_ms = None;
        assert!(HistoryEntry::try_from(missing_expiry).is_err());
    }

    #[test]
    fn history_pages_bound_utf8_previews_and_keep_full_text_separate() {
        let mut original = entry();
        original.project = Some("/private/project".into());
        original.own_voice_only = true;
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
        busy.retrying = true;
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
        ] {
            assert_eq!(
                RpcErrorCode::try_from(i32::from(code)).expect("known code"),
                code
            );
            assert_eq!(
                Status::from(RpcFailure::new(code, "safe message")).code(),
                grpc
            );
        }
    }
}
