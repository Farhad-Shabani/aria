use crate::daemon::inference::{
    Event, Info, MAX_AUDIO_SAMPLES, MAX_ENGINE_NAME_BYTES, MAX_TEXT_BYTES, Start, Strategy,
    Transcript, identity,
};
use crate::ipc::wire;
use crate::rpc::{RpcErrorCode, RpcFailure};
use crate::transcription::{TranscriptSegment, TranscriptUpdate};

use super::ProtocolError;

impl From<Strategy> for i32 {
    fn from(strategy: Strategy) -> Self {
        match strategy {
            Strategy::Whole => wire::RecognitionStrategy::Whole.into(),
            Strategy::Streaming => wire::RecognitionStrategy::Streaming.into(),
        }
    }
}

impl TryFrom<i32> for Strategy {
    type Error = ProtocolError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match wire::RecognitionStrategy::try_from(value) {
            Ok(wire::RecognitionStrategy::Whole) => Ok(Self::Whole),
            Ok(wire::RecognitionStrategy::Streaming) => Ok(Self::Streaming),
            _ => Err(ProtocolError::InvalidEnum {
                field: "recognition strategy",
                value,
            }),
        }
    }
}

impl TryFrom<wire::InferenceStart> for Start {
    type Error = ProtocolError;

    fn try_from(value: wire::InferenceStart) -> Result<Self, Self::Error> {
        if !valid_engine(&value.engine) {
            return Err(ProtocolError::InvalidInference);
        }
        Ok(Self {
            engine: value.engine,
            language: value
                .language
                .parse()
                .map_err(|_| ProtocolError::InvalidInference)?,
            strategy: value.strategy.try_into()?,
        })
    }
}

impl From<Start> for wire::InferenceStart {
    fn from(value: Start) -> Self {
        Self {
            engine: value.engine,
            language: value.language.as_str().to_owned(),
            strategy: value.strategy.into(),
        }
    }
}

impl From<Info> for wire::InferenceInfoResponse {
    fn from(value: Info) -> Self {
        let mut strategies = vec![Strategy::Whole.into()];
        if value.streaming {
            strategies.push(Strategy::Streaming.into());
        }
        Self {
            engine: value.engine,
            strategies,
        }
    }
}

impl TryFrom<wire::InferenceInfoResponse> for Info {
    type Error = ProtocolError;

    fn try_from(value: wire::InferenceInfoResponse) -> Result<Self, Self::Error> {
        if !valid_engine(&value.engine) || value.strategies.len() > 2 {
            return Err(ProtocolError::InvalidInference);
        }
        let strategies = value
            .strategies
            .into_iter()
            .map(Strategy::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        if !strategies.contains(&Strategy::Whole) {
            return Err(ProtocolError::InvalidInference);
        }
        Ok(Self {
            engine: value.engine,
            streaming: strategies.contains(&Strategy::Streaming),
        })
    }
}

impl From<Transcript> for wire::InferenceComplete {
    fn from(value: Transcript) -> Self {
        Self {
            text: value.text,
            engine: value.engine,
        }
    }
}

impl TryFrom<wire::InferenceComplete> for Transcript {
    type Error = ProtocolError;

    fn try_from(value: wire::InferenceComplete) -> Result<Self, Self::Error> {
        if value.text.len() > MAX_TEXT_BYTES || !valid_engine(&value.engine) {
            return Err(ProtocolError::InvalidInference);
        }
        Ok(Self {
            text: value.text,
            engine: value.engine,
        })
    }
}

impl From<Event> for wire::InferenceResponse {
    fn from(value: Event) -> Self {
        use wire::inference_response::Body;
        let body = match value {
            Event::Complete(transcript) => Body::Complete(transcript.into()),
            Event::Update {
                update,
                retained_from,
                read_to,
                completed,
            } => {
                let body = match update {
                    TranscriptUpdate::Provisional(text) => {
                        wire::inference_update::Body::Provisional(text)
                    }
                    TranscriptUpdate::Finalized(segment) => {
                        wire::inference_update::Body::Finalized(wire::InferenceSegment {
                            span: segment.span().map(|span| wire::InferenceSpan {
                                start: span.start,
                                end: span.end,
                            }),
                            text: segment.into_text(),
                        })
                    }
                };
                Body::Update(wire::InferenceUpdate {
                    retained_from,
                    read_to: Some(read_to),
                    body: Some(body),
                    completed: completed.map(Into::into),
                })
            }
        };
        Self { body: Some(body) }
    }
}

impl TryFrom<wire::InferenceResponse> for Event {
    type Error = ProtocolError;

    fn try_from(value: wire::InferenceResponse) -> Result<Self, Self::Error> {
        match value.body.ok_or(ProtocolError::InvalidInference)? {
            wire::inference_response::Body::Complete(value) => {
                Ok(Self::Complete(value.try_into()?))
            }
            wire::inference_response::Body::Update(value) => {
                let read_to = value.read_to.ok_or(ProtocolError::InvalidInference)?;
                if read_to > MAX_AUDIO_SAMPLES || value.retained_from > read_to {
                    return Err(ProtocolError::InvalidInference);
                }
                let update = match value.body.ok_or(ProtocolError::InvalidInference)? {
                    wire::inference_update::Body::Provisional(text) => {
                        if text.len() > MAX_TEXT_BYTES {
                            return Err(ProtocolError::InvalidInference);
                        }
                        TranscriptUpdate::Provisional(text)
                    }
                    wire::inference_update::Body::Finalized(segment) => {
                        if segment.text.len() > MAX_TEXT_BYTES
                            || segment
                                .span
                                .as_ref()
                                .is_some_and(|span| span.start > span.end || span.end > read_to)
                        {
                            return Err(ProtocolError::InvalidInference);
                        }
                        let span = segment.span.map(|span| span.start..span.end);
                        TranscriptUpdate::Finalized(
                            TranscriptSegment::new(segment.text, span)
                                .map_err(|_| ProtocolError::InvalidInference)?,
                        )
                    }
                };
                Ok(Self::Update {
                    update,
                    retained_from: value.retained_from,
                    read_to,
                    completed: value.completed.map(TryInto::try_into).transpose()?,
                })
            }
        }
    }
}

fn valid_engine(engine: &str) -> bool {
    !engine.is_empty() && engine.len() <= MAX_ENGINE_NAME_BYTES
}

impl From<identity::Error> for RpcFailure {
    fn from(error: identity::Error) -> Self {
        use identity::Error;
        let (code, message) = match error {
            Error::RevokeUnconfirmed(_) => (
                RpcErrorCode::DurabilityUnconfirmed,
                "client denied for this process; revocation durability is unconfirmed",
            ),
            Error::DurabilityUnconfirmed(_) => (
                RpcErrorCode::DurabilityUnconfirmed,
                "trust was written but durability is unconfirmed",
            ),
            Error::InvalidInvitation | Error::InvalidEndpoint => (
                RpcErrorCode::InvalidRequest,
                "invalid remote invitation or endpoint",
            ),
            Error::Unauthenticated | Error::Unpaired => (
                RpcErrorCode::Unauthenticated,
                "remote identity is not authorized",
            ),
            Error::Full => (
                RpcErrorCode::PermissionDenied,
                "client enrollment limit reached",
            ),
            Error::Busy => (RpcErrorCode::Busy, "inference is busy"),
            Error::Stopped => (RpcErrorCode::ShuttingDown, "inference host is stopping"),
            Error::Document(_)
            | Error::InvalidIdentity
            | Error::Certificate(_)
            | Error::Invite(_)
            | Error::Tls(_) => (RpcErrorCode::Internal, "remote identity operation failed"),
        };
        Self::new(code, message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inference_rejects_unknown_strategy_and_sensitive_invalid_language() {
        for strategy in [0, 3, -1] {
            assert!(Strategy::try_from(strategy).is_err());
        }
        let error = Start::try_from(wire::InferenceStart {
            engine: "whisper".into(),
            language: "private audio content".into(),
            strategy: Strategy::Whole.into(),
        })
        .err()
        .unwrap();
        assert_eq!(error.to_string(), "invalid inference message");
    }

    #[test]
    fn inference_round_trip_preserves_strategy_and_language() {
        for strategy in [Strategy::Whole, Strategy::Streaming] {
            let start = Start {
                engine: "whisper".into(),
                language: "en".parse().unwrap(),
                strategy,
            };
            let decoded = Start::try_from(wire::InferenceStart::from(start)).unwrap();
            assert_eq!(decoded.strategy, strategy);
            assert_eq!(decoded.language.as_str(), "en");
        }
    }
    #[test]
    fn inference_updates_preserve_timing_retention_and_completion() {
        let event = Event::Update {
            update: TranscriptUpdate::Finalized(
                TranscriptSegment::new("words", Some(12..24)).unwrap(),
            ),
            retained_from: 8,
            read_to: 32,
            completed: Some(Transcript {
                text: "words".into(),
                engine: "whisper".into(),
            }),
        };
        let Event::Update {
            update,
            retained_from,
            read_to,
            completed,
        } = Event::try_from(wire::InferenceResponse::from(event)).unwrap()
        else {
            panic!("an update stays an update");
        };
        let TranscriptUpdate::Finalized(segment) = update else {
            panic!("a finalized segment stays finalized");
        };
        assert_eq!(segment.as_str(), "words");
        assert_eq!(segment.span(), Some(12..24));
        assert_eq!(retained_from, 8);
        assert_eq!(read_to, 32);
        assert_eq!(completed.unwrap().text, "words");
    }

    #[test]
    fn inference_update_progress_fails_closed_with_an_old_writer() {
        use prost::Message;

        #[derive(Clone, PartialEq, Message)]
        struct OldUpdate {
            #[prost(uint64, tag = "1")]
            retained_from: u64,
            #[prost(oneof = "wire::inference_update::Body", tags = "2, 3")]
            body: Option<wire::inference_update::Body>,
            #[prost(message, optional, tag = "4")]
            completed: Option<wire::InferenceComplete>,
        }

        let old = OldUpdate {
            retained_from: 0,
            body: Some(wire::inference_update::Body::Provisional("words".into())),
            completed: None,
        };
        let encoded = old.encode_to_vec();
        let mut update = wire::InferenceUpdate::decode(encoded.as_slice()).unwrap();
        assert!(update.read_to.is_none());
        assert!(
            Event::try_from(wire::InferenceResponse {
                body: Some(wire::inference_response::Body::Update(update.clone())),
            })
            .is_err()
        );

        update.read_to = Some(1600);
        let encoded = update.encode_to_vec();
        assert_eq!(OldUpdate::decode(encoded.as_slice()).unwrap(), old);
        assert!(
            Event::try_from(wire::InferenceResponse {
                body: Some(wire::inference_response::Body::Update(update)),
            })
            .is_ok()
        );
    }

    #[test]
    fn inference_updates_reject_impossible_read_positions() {
        for (retained_from, read_to, span_end) in
            [(11, 10, 10), (0, MAX_AUDIO_SAMPLES + 1, 10), (0, 10, 11)]
        {
            let update = wire::InferenceUpdate {
                retained_from,
                read_to: Some(read_to),
                body: Some(wire::inference_update::Body::Finalized(
                    wire::InferenceSegment {
                        text: "words".into(),
                        span: Some(wire::InferenceSpan {
                            start: 0,
                            end: span_end,
                        }),
                    },
                )),
                completed: None,
            };
            assert!(
                Event::try_from(wire::InferenceResponse {
                    body: Some(wire::inference_response::Body::Update(update)),
                })
                .is_err()
            );
        }
    }

    #[test]
    fn inference_updates_reject_missing_bodies_and_invalid_spans() {
        assert!(Event::try_from(wire::InferenceResponse { body: None }).is_err());
        for span in [(8, 4), (0, MAX_AUDIO_SAMPLES + 1)] {
            let response = wire::InferenceResponse {
                body: Some(wire::inference_response::Body::Update(
                    wire::InferenceUpdate {
                        retained_from: 0,
                        read_to: Some(MAX_AUDIO_SAMPLES),
                        body: Some(wire::inference_update::Body::Finalized(
                            wire::InferenceSegment {
                                text: "private words".into(),
                                span: Some(wire::InferenceSpan {
                                    start: span.0,
                                    end: span.1,
                                }),
                            },
                        )),
                        completed: None,
                    },
                )),
            };
            let error = Event::try_from(response).err().unwrap();
            assert_eq!(error.to_string(), "invalid inference message");
        }
    }
}
