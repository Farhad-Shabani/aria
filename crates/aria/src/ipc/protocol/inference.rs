use std::ops::Range;

use crate::inference::{
    Completion, Event, Info, MAX_AUDIO_SAMPLES, MAX_ENGINE_NAME_BYTES, MAX_TEXT_BYTES, Start,
    Strategy,
};
use crate::ipc::wire;
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
        if !valid_engine(&value.engine_id) {
            return Err(ProtocolError::InvalidInference);
        }
        Ok(Self {
            engine: value.engine_id,
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
            engine_id: value.engine,
            language: value.language.as_str().to_owned(),
            strategy: value.strategy.into(),
        }
    }
}

impl From<Info> for wire::InferenceInfoResponse {
    fn from(value: Info) -> Self {
        let strategies = match value.strategy {
            Strategy::Whole => vec![Strategy::Whole.into()],
            Strategy::Streaming => vec![Strategy::Whole.into(), Strategy::Streaming.into()],
        };
        Self {
            engine_id: value.engine,
            strategies,
        }
    }
}

impl TryFrom<wire::InferenceInfoResponse> for Info {
    type Error = ProtocolError;

    fn try_from(value: wire::InferenceInfoResponse) -> Result<Self, Self::Error> {
        if !valid_engine(&value.engine_id) || value.strategies.len() > 2 {
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
            engine: value.engine_id,
            strategy: if strategies.contains(&Strategy::Streaming) {
                Strategy::Streaming
            } else {
                Strategy::Whole
            },
        })
    }
}

impl From<Completion> for wire::InferenceComplete {
    fn from(value: Completion) -> Self {
        Self {
            text: value.text,
            engine_id: value.engine,
        }
    }
}

impl TryFrom<wire::InferenceComplete> for Completion {
    type Error = ProtocolError;

    fn try_from(value: wire::InferenceComplete) -> Result<Self, Self::Error> {
        if value.text.len() > MAX_TEXT_BYTES || !valid_engine(&value.engine_id) {
            return Err(ProtocolError::InvalidInference);
        }
        Ok(Self {
            text: value.text,
            engine: value.engine_id,
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
                let mut provisional_source_window = None;
                let body = match update {
                    TranscriptUpdate::Provisional {
                        text,
                        source_window,
                    } => {
                        provisional_source_window = source_window.map(wire_span);
                        wire::inference_update::Body::Provisional(text)
                    }
                    TranscriptUpdate::Finalized(segment) => {
                        wire::inference_update::Body::Finalized(wire::InferenceSegment {
                            source_window: segment.source_window().map(wire_span),
                            words: segment
                                .word_spans()
                                .unwrap_or_default()
                                .iter()
                                .cloned()
                                .map(wire_span)
                                .collect(),
                            span: segment.span().map(wire_span),
                            text: segment.into_text(),
                        })
                    }
                };
                Body::Update(wire::InferenceUpdate {
                    retained_from_sample: retained_from,
                    read_to_sample: Some(read_to),
                    body: Some(body),
                    completed: completed.map(Into::into),
                    provisional_source_window,
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
                let read_to = value
                    .read_to_sample
                    .ok_or(ProtocolError::InvalidInference)?;
                if read_to > MAX_AUDIO_SAMPLES || value.retained_from_sample > read_to {
                    return Err(ProtocolError::InvalidInference);
                }
                let body = value.body.ok_or(ProtocolError::InvalidInference)?;
                let update = match (body, value.provisional_source_window) {
                    (wire::inference_update::Body::Provisional(text), window) => {
                        preview(text, window, read_to)?
                    }
                    (wire::inference_update::Body::Finalized(segment), None) => {
                        TranscriptUpdate::Finalized(finalized(segment, read_to)?)
                    }
                    (wire::inference_update::Body::Finalized(_), Some(_misplaced)) => {
                        return Err(ProtocolError::InvalidInference);
                    }
                };
                Ok(Self::Update {
                    update,
                    retained_from: value.retained_from_sample,
                    read_to,
                    completed: value.completed.map(TryInto::try_into).transpose()?,
                })
            }
        }
    }
}

fn preview(
    text: String,
    window: Option<wire::InferenceSpan>,
    read_to: u64,
) -> Result<TranscriptUpdate, ProtocolError> {
    if text.len() > MAX_TEXT_BYTES
        || window
            .as_ref()
            .is_some_and(|span| out_of_bounds(span, read_to))
    {
        return Err(ProtocolError::InvalidInference);
    }
    Ok(TranscriptUpdate::Provisional {
        text,
        source_window: window.map(|span| span.start_sample..span.end_sample),
    })
}

fn finalized(
    segment: wire::InferenceSegment,
    read_to: u64,
) -> Result<TranscriptSegment, ProtocolError> {
    if segment.text.len() > MAX_TEXT_BYTES
        || segment
            .source_window
            .as_ref()
            .is_some_and(|span| out_of_bounds(span, read_to))
        || segment
            .span
            .as_ref()
            .is_some_and(|span| out_of_bounds(span, read_to))
    {
        return Err(ProtocolError::InvalidInference);
    }
    let span = segment.span.map(|span| span.start_sample..span.end_sample);
    let mut finalized =
        TranscriptSegment::new(segment.text, span).map_err(|_| ProtocolError::InvalidInference)?;
    if !segment.words.is_empty() {
        let words = segment
            .words
            .into_iter()
            .map(|word| word.start_sample..word.end_sample)
            .collect();
        finalized = finalized
            .with_word_spans(words)
            .map_err(|_| ProtocolError::InvalidInference)?;
    }
    if let Some(window) = segment.source_window {
        finalized = finalized
            .with_source_window(window.start_sample..window.end_sample)
            .map_err(|_| ProtocolError::InvalidInference)?;
    }
    Ok(finalized)
}

fn valid_engine(engine: &str) -> bool {
    !engine.is_empty() && engine.len() <= MAX_ENGINE_NAME_BYTES
}

fn wire_span(span: Range<u64>) -> wire::InferenceSpan {
    wire::InferenceSpan {
        start_sample: span.start,
        end_sample: span.end,
    }
}

fn out_of_bounds(span: &wire::InferenceSpan, read_to: u64) -> bool {
    span.start_sample > span.end_sample || span.end_sample > read_to
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inference_rejects_unknown_strategy_and_invalid_language() {
        for strategy in [0, 3, -1] {
            assert!(Strategy::try_from(strategy).is_err());
        }
        assert!(
            Start::try_from(wire::InferenceStart {
                engine_id: "whisper".into(),
                language: "not a language".into(),
                strategy: Strategy::Whole.into(),
            })
            .is_err()
        );
    }

    #[test]
    fn inference_updates_preserve_timing_retention_and_completion() {
        let event = Event::Update {
            update: TranscriptUpdate::Finalized(
                TranscriptSegment::new("words", Some(12..24)).unwrap(),
            ),
            retained_from: 8,
            read_to: 32,
            completed: Some(Completion {
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
    fn inference_previews_preserve_optional_source_windows() {
        for window in [None, Some(8..32)] {
            let event = Event::Update {
                update: TranscriptUpdate::Provisional {
                    text: "words".into(),
                    source_window: window.clone(),
                },
                retained_from: 8,
                read_to: 32,
                completed: None,
            };
            let response = wire::InferenceResponse::from(event);
            let Event::Update {
                update:
                    TranscriptUpdate::Provisional {
                        text,
                        source_window,
                    },
                ..
            } = Event::try_from(response).unwrap()
            else {
                panic!("a preview stays provisional");
            };
            assert_eq!(text, "words");
            assert_eq!(source_window, window);
        }
    }

    fn update_mut(response: &mut wire::InferenceResponse) -> &mut wire::InferenceUpdate {
        let Some(wire::inference_response::Body::Update(update)) = &mut response.body else {
            panic!("the response carries an update");
        };
        update
    }

    fn finalized_mut(response: &mut wire::InferenceResponse) -> &mut wire::InferenceSegment {
        let Some(wire::inference_update::Body::Finalized(segment)) = &mut update_mut(response).body
        else {
            panic!("the update carries a finalized segment");
        };
        segment
    }

    #[test]
    fn inference_rejects_invalid_or_misplaced_preview_sources() {
        let response = wire::InferenceResponse::from(Event::Update {
            update: TranscriptUpdate::Provisional {
                text: "words".into(),
                source_window: Some(8..32),
            },
            retained_from: 8,
            read_to: 32,
            completed: None,
        });
        let reversed = std::ops::Range { start: 32, end: 8 };
        for window in [reversed, 8..33] {
            let mut invalid = response.clone();
            update_mut(&mut invalid).provisional_source_window = Some(wire_span(window));
            assert!(Event::try_from(invalid).is_err());
        }
        let mut invalid = response;
        update_mut(&mut invalid).body = Some(wire::inference_update::Body::Finalized(
            wire::InferenceSegment {
                text: "words".into(),
                ..Default::default()
            },
        ));
        assert!(Event::try_from(invalid).is_err());
    }

    #[test]
    fn inference_preserves_word_spans_and_rejects_invalid_alignment() {
        let event = Event::Update {
            update: TranscriptUpdate::Finalized(
                TranscriptSegment::new("one two", Some(12..24))
                    .unwrap()
                    .with_word_spans(vec![12..16, 18..24])
                    .unwrap()
                    .with_source_window(8..32)
                    .unwrap(),
            ),
            retained_from: 8,
            read_to: 32,
            completed: None,
        };
        let response = wire::InferenceResponse::from(event);
        let Event::Update {
            update: TranscriptUpdate::Finalized(segment),
            ..
        } = Event::try_from(response.clone()).unwrap()
        else {
            panic!("a finalized segment stays finalized");
        };
        assert_eq!(segment.word_spans(), Some(&[12..16, 18..24][..]));
        assert_eq!(segment.source_window(), Some(8..32));

        let mut one_word_short = response.clone();
        finalized_mut(&mut one_word_short).words = vec![wire_span(12..16)];
        assert!(Event::try_from(one_word_short).is_err());
        // 13..32 leaves out the start of the span; 9..33 reads past read_to.
        for window in [13..32, 9..33] {
            let mut invalid = response.clone();
            finalized_mut(&mut invalid).source_window = Some(wire_span(window));
            assert!(Event::try_from(invalid).is_err());
        }
    }

    #[test]
    fn inference_updates_reject_missing_fields_and_impossible_positions() {
        assert!(Event::try_from(wire::InferenceResponse { body: None }).is_err());
        let update = |retained_from, read_to, span: (u64, u64)| wire::InferenceUpdate {
            retained_from_sample: retained_from,
            read_to_sample: read_to,
            provisional_source_window: None,
            body: Some(wire::inference_update::Body::Finalized(
                wire::InferenceSegment {
                    text: "private words".into(),
                    source_window: None,
                    words: Vec::new(),
                    span: Some(wire::InferenceSpan {
                        start_sample: span.0,
                        end_sample: span.1,
                    }),
                },
            )),
            completed: None,
        };
        let cases = [
            update(0, None, (0, 10)),
            update(11, Some(10), (0, 10)),
            update(0, Some(MAX_AUDIO_SAMPLES + 1), (0, 10)),
            update(0, Some(10), (0, 11)),
            update(0, Some(10), (8, 4)),
            update(0, Some(MAX_AUDIO_SAMPLES), (0, MAX_AUDIO_SAMPLES + 1)),
            wire::InferenceUpdate {
                body: None,
                ..update(0, Some(10), (0, 10))
            },
        ];
        for update in cases {
            let response = wire::InferenceResponse {
                body: Some(wire::inference_response::Body::Update(update)),
            };
            let error = Event::try_from(response).err().unwrap();
            assert_eq!(error.to_string(), "invalid inference message");
        }
    }
}
