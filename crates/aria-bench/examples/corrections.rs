use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io::{self, BufRead, Write};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use aria_bench::stt::{reference_matches, word_errors, words};
use aria_kit::audio::{CANONICAL_SAMPLE_RATE, canonical_samples, read_wav};
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{
    Hypothesis, Language, Transcriber, TranscriptionAudio, TranscriptionError,
};
use aria_stt::parakeet::{Device, Parakeet};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Segment {
    text: String,
    span_samples: Option<Range<u64>>,
    available_samples: u64,
}

#[derive(Deserialize)]
struct Trace {
    schema: u32,
    utterance: String,
    reference: String,
    hypothesis: String,
    backend: serde_json::Value,
    error: Option<String>,
    segments: Vec<Segment>,
}

impl Trace {
    fn validate(&self, model: &str) -> Result<(), &'static str> {
        if self.schema != 5
            || self.backend["name"] != "parakeet"
            || self.backend["model"] != model
            || self.error.is_some()
            || self.segments.is_empty()
        {
            return Err(
                "expected a successful schema 5 Parakeet streaming trace with matching model",
            );
        }
        if text(
            &self
                .segments
                .iter()
                .map(|segment| segment.text.clone())
                .collect::<Vec<_>>(),
        ) != self.hypothesis
        {
            return Err("trace segments do not reproduce baseline text");
        }
        Ok(())
    }
}

fn errors(reference: &str, hypothesis: &str) -> usize {
    let errors = word_errors(reference, hypothesis);
    errors.substitutions + errors.deletions + errors.insertions
}

fn text(slots: &[String]) -> String {
    slots
        .iter()
        .filter(|slot| !slot.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

fn boundaries(segments: &[Segment], first: usize, last: usize) -> Option<Vec<u64>> {
    let start = if first == 0 {
        0
    } else {
        segments[first - 1].span_samples.as_ref()?.end
    };
    let mut bounds = vec![start];
    for segment in &segments[first..=last] {
        let span = segment.span_samples.as_ref()?;
        if span.start > span.end || span.end <= *bounds.last()? {
            return None;
        }
        bounds.push(span.end);
    }
    (bounds.last()? <= &segments[last].available_samples).then_some(bounds)
}

fn distribute(hypothesis: &Hypothesis, origin: u64, bounds: &[u64]) -> Option<Vec<String>> {
    if hypothesis.words().is_empty() {
        return None;
    }
    let mut slots = vec![Vec::new(); bounds.len() - 1];
    for (word, timing) in hypothesis
        .as_str()
        .split_whitespace()
        .zip(hypothesis.words())
    {
        if !timing.start.is_finite() || !timing.end.is_finite() || timing.start > timing.end {
            return None;
        }
        let midpoint = origin as f64
            + f64::from(timing.start + timing.end) * f64::from(CANONICAL_SAMPLE_RATE) / 2.0;
        for (index, span) in bounds.windows(2).enumerate() {
            if midpoint >= span[0] as f64 && midpoint < span[1] as f64 {
                slots[index].push(word);
                break;
            }
        }
    }
    let result: Vec<String> = slots.into_iter().map(|words| words.join(" ")).collect();
    result.iter().any(|slot| !slot.is_empty()).then_some(result)
}

struct Policy {
    chunks: usize,
    padding_ms: u64,
    full_context: bool,
    anchored: bool,
}

struct CachedDecode {
    hypothesis: Result<Hypothesis, TranscriptionError>,
    decode_ms: f64,
}

fn evaluate(
    trace: &Trace,
    samples: &[f32],
    engine: &impl Transcriber,
    policy: Policy,
    cache: &mut HashMap<(usize, usize), CachedDecode>,
) -> serde_json::Value {
    let Policy {
        chunks,
        padding_ms,
        full_context,
        anchored,
    } = policy;
    let mut slots: Vec<String> = trace
        .segments
        .iter()
        .map(|segment| segment.text.clone())
        .collect();
    let mut events = Vec::new();
    let mut cache_hits = 0;
    let padding = canonical_samples(Duration::from_millis(padding_ms));
    for last in chunks - 1..trace.segments.len() {
        let first = last + 1 - chunks;
        let Some(bounds) = boundaries(&trace.segments, first, last) else {
            events.push(json!({"last": last, "status": "invalid_span"}));
            continue;
        };
        let start = if full_context {
            0
        } else {
            bounds[0].saturating_sub(padding) as usize
        };
        let end = (if full_context {
            trace.segments[last].available_samples
        } else {
            bounds[chunks] + padding
        })
        .min(trace.segments[last].available_samples)
        .min(samples.len() as u64) as usize;
        if start >= end || trace.segments[last].available_samples > samples.len() as u64 {
            events.push(json!({"last": last, "status": "invalid_audio"}));
            continue;
        }
        cache_hits += usize::from(cache.contains_key(&(start, end)));
        let decoded = cache.entry((start, end)).or_insert_with(|| {
            let started = Instant::now();
            let hypothesis = engine.transcribe_with(
                TranscriptionAudio::Samples(&samples[start..end]),
                &Language::Automatic,
                &CancelToken::never(),
            );
            CachedDecode {
                hypothesis,
                decode_ms: started.elapsed().as_secs_f64() * 1000.0,
            }
        });
        let decode_ms = decoded.decode_ms;
        let Ok(hypothesis) = &decoded.hypothesis else {
            events.push(json!({"last": last, "status": "decode_failed", "decode_ms": decode_ms}));
            continue;
        };
        let Some(replacement) = distribute(hypothesis, start as u64, &bounds) else {
            events
                .push(json!({"last": last, "status": "empty_or_untimed", "decode_ms": decode_ms}));
            continue;
        };
        let before = text(&slots);
        let old_window = text(&slots[first..=last]);
        let new_window = text(&replacement);
        if old_window == new_window {
            events.push(json!({"last": last, "status": "unchanged", "decode_ms": decode_ms}));
            continue;
        }
        let old_words = words(&old_window);
        let new_words = words(&new_window);
        if anchored
            && (old_words.len() < 4
                || new_words.len() < 4
                || old_words[..2] != new_words[..2]
                || old_words[old_words.len() - 2..] != new_words[new_words.len() - 2..])
        {
            events.push(json!({"last": last, "status": "anchor_mismatch", "decode_ms": decode_ms}));
            continue;
        }
        slots[first..=last].clone_from_slice(&replacement);
        let after = text(&slots);
        let before_matches = reference_matches(&trace.reference, &before);
        let after_matches = reference_matches(&trace.reference, &after);
        let harmed = before_matches
            .iter()
            .zip(&after_matches)
            .filter(|(a, b)| **a && !**b)
            .count();
        let fixed = before_matches
            .iter()
            .zip(&after_matches)
            .filter(|(a, b)| !**a && **b)
            .count();
        events.push(json!({
            "first": first, "last": last, "status": "replaced",
            "before": old_window, "after": new_window,
            "before_errors": errors(&trace.reference, &before),
            "after_errors": errors(&trace.reference, &after),
            "harmed_reference_words": harmed, "fixed_reference_words": fixed,
            "normalized_change": words(&old_window) != words(&new_window),
            "audio_lookback_ms": (trace.segments[last].available_samples
                - trace.segments[first].available_samples) as f64 * 1000.0
                / f64::from(CANONICAL_SAMPLE_RATE),
            "decode_ms": decode_ms,
            "input_samples": {"start": start, "end": end},
            "available_samples": trace.segments[last].available_samples,
        }));
    }
    let corrected = text(&slots);
    json!({
        "utterance": trace.utterance, "chunks": chunks, "padding_ms": padding_ms,
        "full_context": full_context, "anchored": anchored,
        "cache_hits": cache_hits,
        "decode_timing": "original inference duration reused for each policy",
        "reference_words": words(&trace.reference).len(),
        "baseline_errors": errors(&trace.reference, &trace.hypothesis),
        "corrected_errors": errors(&trace.reference, &corrected),
        "reference": trace.reference, "baseline": trace.hypothesis,
        "corrected": corrected, "events": events,
    })
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() != 4 {
        return Err("usage: corrections MANIFEST TRACES PARAKEET_MODEL PADDING_MS|prefix".into());
    }
    let full_context = arguments[3] == "prefix";
    let padding_ms: u64 = if full_context {
        0
    } else {
        arguments[3].parse()?
    };
    if padding_ms > 1000 {
        return Err("padding must be at most 1000 ms".into());
    }
    let mut audio = HashMap::new();
    for line in fs::read_to_string(&arguments[0])?.lines() {
        let (path, _) = line.split_once('\t').ok_or("manifest row needs a tab")?;
        let path = PathBuf::from(path);
        let id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or("audio has no id")?
            .to_owned();
        if audio.insert(id, path).is_some() {
            return Err("duplicate corpus id".into());
        }
    }
    let engine = Parakeet::load(&arguments[2], Device::Cpu)?;
    let mut output = io::BufWriter::new(io::stdout().lock());
    let traces = io::BufReader::new(fs::File::open(&arguments[1])?);
    for (index, line) in traces.lines().enumerate() {
        let trace: Trace = serde_json::from_str(&line?)?;
        trace.validate(&arguments[2])?;
        let samples = read_wav(
            audio
                .get(&trace.utterance)
                .ok_or("trace id missing from corpus")?,
        )?
        .into_canonical()?
        .into_samples();
        let mut cache = HashMap::new();
        for chunks in [2, 3] {
            eprintln!(
                "clip {}: {} chunks={chunks} padding={padding_ms}",
                index + 1,
                trace.utterance
            );
            for anchored in [false, true] {
                let result = evaluate(
                    &trace,
                    &samples,
                    &engine,
                    Policy {
                        chunks,
                        padding_ms,
                        full_context,
                        anchored,
                    },
                    &mut cache,
                );
                serde_json::to_writer(&mut output, &result)?;
                writeln!(output)?;
                output.flush()?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_kit::transcription::{TimedWord, TranscriptionError};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct Decode {
        samples: Range<usize>,
        result: Result<Hypothesis, TranscriptionError>,
    }

    struct Scripted(Mutex<VecDeque<Decode>>);

    impl Transcriber for Scripted {
        fn validate_language(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            audio: TranscriptionAudio<'_>,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            let decode = self.0.lock().unwrap().pop_front().unwrap();
            let TranscriptionAudio::Samples(samples) = audio else {
                panic!("corrections decode sample slices");
            };
            assert_eq!(samples.len(), decode.samples.len());
            for (sample, expected) in samples.iter().zip(decode.samples) {
                assert_eq!(*sample, expected as f32);
            }
            decode.result
        }
    }

    fn trace(tokens: &[&str]) -> Trace {
        Trace {
            schema: 5,
            utterance: "test".into(),
            reference: tokens.join(" "),
            hypothesis: tokens.join(" "),
            backend: json!({}),
            error: None,
            segments: tokens
                .iter()
                .enumerate()
                .map(|(index, token)| Segment {
                    text: (*token).into(),
                    span_samples: Some(index as u64 * 16000..(index as u64 + 1) * 16000),
                    available_samples: (index as u64 + 1) * 16000 + 8000,
                })
                .collect(),
        }
    }

    fn timed(text: &str, midpoints: &[f32]) -> Hypothesis {
        Hypothesis::with_words(
            text,
            midpoints
                .iter()
                .map(|midpoint| TimedWord {
                    start: *midpoint,
                    end: *midpoint,
                })
                .collect(),
        )
    }

    #[test]
    fn corrections_accept_current_traces_and_reject_old_schemas() {
        let mut trace = trace(&["one", "two"]);
        trace.backend = json!({"name": "parakeet", "model": "fixture"});
        assert_eq!(trace.validate("fixture"), Ok(()));
        trace.schema = 4;
        assert!(trace.validate("fixture").is_err());
    }

    #[test]
    fn overlapping_corrections_freeze_older_text_and_never_read_future_audio() {
        let trace = trace(&["one", "two", "three"]);
        let samples: Vec<f32> = (0..64000).map(|index| index as f32).collect();
        for full_context in [false, true] {
            let second_start = if full_context { 0 } else { 8000 };
            let origin_seconds = second_start as f32 / 16000.0;
            let engine = Scripted(Mutex::new(VecDeque::from([
                Decode {
                    samples: 0..40000,
                    result: Ok(timed("first second", &[0.5, 1.5])),
                },
                Decode {
                    samples: second_start..56000,
                    result: Ok(timed(
                        "discard revised third",
                        &[
                            0.75 - origin_seconds,
                            1.5 - origin_seconds,
                            2.5 - origin_seconds,
                        ],
                    )),
                },
            ])));

            let result = evaluate(
                &trace,
                &samples,
                &engine,
                Policy {
                    chunks: 2,
                    padding_ms: 500,
                    full_context,
                    anchored: false,
                },
                &mut HashMap::new(),
            );

            assert_eq!(result["corrected"], "first revised third");
            assert_eq!(result["events"][0]["after"], "first second");
            assert_eq!(result["events"][1]["before"], "second three");
            assert_eq!(result["events"][1]["after"], "revised third");
            assert!(engine.0.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn failed_and_empty_corrections_preserve_the_previous_result() {
        let trace = trace(&["one", "two", "three", "four"]);
        let samples: Vec<f32> = (0..80000).map(|index| index as f32).collect();
        let engine = Scripted(Mutex::new(VecDeque::from([
            Decode {
                samples: 0..32000,
                result: Ok(timed("first second", &[0.5, 1.5])),
            },
            Decode {
                samples: 16000..48000,
                result: Err(TranscriptionError::Cancelled),
            },
            Decode {
                samples: 32000..64000,
                result: Ok(Hypothesis::new("")),
            },
        ])));

        let result = evaluate(
            &trace,
            &samples,
            &engine,
            Policy {
                chunks: 2,
                padding_ms: 0,
                full_context: false,
                anchored: false,
            },
            &mut HashMap::new(),
        );

        assert_eq!(result["corrected"], "first second three four");
        assert_eq!(result["events"][0]["status"], "replaced");
        assert_eq!(result["events"][1]["status"], "decode_failed");
        assert_eq!(result["events"][2]["status"], "empty_or_untimed");
        assert!(engine.0.lock().unwrap().is_empty());
    }

    #[test]
    fn anchored_corrections_require_matching_edges_and_reuse_decodes() {
        let trace = trace(&["first second wrong", "fourth fifth"]);
        let samples: Vec<f32> = (0..40000).map(|index| index as f32).collect();
        for (candidate, expected, status) in [
            (
                "first second right fourth fifth",
                "first second right fourth fifth",
                "replaced",
            ),
            (
                "other second right fourth fifth",
                "first second wrong fourth fifth",
                "anchor_mismatch",
            ),
        ] {
            let engine = Scripted(Mutex::new(VecDeque::from([Decode {
                samples: 0..32000,
                result: Ok(timed(candidate, &[0.1, 0.3, 0.5, 1.2, 1.4])),
            }])));
            let mut cache = HashMap::new();
            let unconditional = evaluate(
                &trace,
                &samples,
                &engine,
                Policy {
                    chunks: 2,
                    padding_ms: 0,
                    full_context: false,
                    anchored: false,
                },
                &mut cache,
            );
            let anchored = evaluate(
                &trace,
                &samples,
                &engine,
                Policy {
                    chunks: 2,
                    padding_ms: 0,
                    full_context: false,
                    anchored: true,
                },
                &mut cache,
            );

            assert_eq!(unconditional["corrected"], candidate);
            assert_eq!(anchored["corrected"], expected);
            assert_eq!(anchored["events"][0]["status"], status);
            assert_eq!(anchored["cache_hits"], 1);
            assert_eq!(
                anchored["events"][0]["decode_ms"],
                unconditional["events"][0]["decode_ms"]
            );
            assert!(engine.0.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn unchanged_text_does_not_move_words_into_a_later_edit_window() {
        let trace = trace(&["one", "two", "three"]);
        let samples: Vec<f32> = (0..56000).map(|index| index as f32).collect();
        let engine = Scripted(Mutex::new(VecDeque::from([
            Decode {
                samples: 0..32000,
                result: Ok(timed("one two", &[1.1, 1.3])),
            },
            Decode {
                samples: 16000..48000,
                result: Ok(timed("revised third", &[0.5, 1.5])),
            },
        ])));
        let result = evaluate(
            &trace,
            &samples,
            &engine,
            Policy {
                chunks: 2,
                padding_ms: 0,
                full_context: false,
                anchored: false,
            },
            &mut HashMap::new(),
        );
        assert_eq!(result["events"][0]["status"], "unchanged");
        assert_eq!(result["events"][1]["before"], "two three");
        assert_eq!(result["corrected"], "one revised third");
    }

    #[test]
    fn padding_words_stay_outside_the_edit_and_shared_boundaries_do_not_duplicate() {
        let hypothesis = Hypothesis::with_words(
            "old one two new",
            vec![
                TimedWord {
                    start: 0.0,
                    end: 0.1,
                },
                TimedWord {
                    start: 0.2,
                    end: 0.4,
                },
                TimedWord {
                    start: 0.5,
                    end: 0.5,
                },
                TimedWord {
                    start: 0.8,
                    end: 0.9,
                },
            ],
        );
        assert_eq!(
            distribute(&hypothesis, 0, &[1600, 8000, 12000]),
            Some(vec!["one".into(), "two".into()])
        );
        assert!(distribute(&Hypothesis::new("untimed"), 0, &[0, 16000]).is_none());
    }
}
