//! What one engine attempt costs: the timed drive every phase shares.

use std::time::{Duration, Instant};

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::error_chain;
use aria_kit::synthesis::{SpeakingRate, SpeechText, SynthesisError, TryRead, Voice, stream};

use super::backends::Loaded;

/// Requests cancellation after this budget. A blocked native call can exceed it;
/// a hard time limit requires process isolation.
const CEILING: Duration = Duration::from_secs(120);

/// How much audio is kept for the judge. Past this the judge refuses the
/// recording anyway, so retaining more would only grow the buffer. This
/// bounds audio, not wall clock — the ceiling above does that.
const RETAINED_AUDIO: Duration = Duration::from_secs(15 * 60);

/// How often an idle reader polls for samples.
const READ_POLL: Duration = Duration::from_millis(1);

const READ_BUFFER_SAMPLES: usize = 4_096;

fn retained_samples(sample_rate: u32) -> usize {
    RETAINED_AUDIO.as_secs() as usize * sample_rate as usize
}

/// The outcome of one headless synthesis.
pub(super) struct Synthesized {
    /// Time from drive start to the first sample arriving.
    pub ttfa: Option<Duration>,
    /// Wall clock of production plus drain.
    pub wall: Duration,
    /// The samples kept for judging. Retention is capped; the count below
    /// is always the whole production.
    pub samples: Vec<f32>,
    /// How many samples the attempt actually produced.
    pub samples_written: usize,
    /// Why the attempt failed, as the outermost typed message. The chain
    /// behind it goes to the log, where paths and users belong.
    pub error: Option<String>,
}

impl Synthesized {
    /// Wall clock per second of audio produced; an attempt that produced
    /// nothing has no rate to relate to.
    pub(super) fn rtf(&self, sample_rate: u32) -> Option<f64> {
        if self.samples_written == 0 {
            return None;
        }
        let audio_seconds = self.samples_written as f64 / f64::from(sample_rate);
        Some(self.wall.as_secs_f64() / audio_seconds)
    }
}

pub(super) fn synthesize(
    engine: &Loaded,
    text: &SpeechText,
    voice: Option<&Voice>,
    rate: SpeakingRate,
) -> Synthesized {
    let cancel = CancelToken::from_fn({
        let started = Instant::now();
        move || started.elapsed() > CEILING
    });
    let (writer, mut speech) = stream(engine.sample_rate());
    let started = Instant::now();
    let retain = retained_samples(engine.sample_rate());

    let outcome = std::thread::scope(|scope| {
        let worker = scope.spawn(|| engine.synthesize_with(text, voice, rate, writer, &cancel));

        let mut ttfa = None;
        let mut samples = Vec::new();
        let mut samples_written = 0usize;
        let mut buffer = vec![0.0; READ_BUFFER_SAMPLES];

        // Draining without waiting keeps the wall clock free of reader-side
        // pacing. The deadline is checked before every read: samples keep
        // flowing through a runaway generator exactly as through a good
        // one, so starvation is not the only signal to watch.
        enum Stop {
            Ended,
            Ceiling,
        }
        let stop = loop {
            if started.elapsed() > CEILING {
                break Stop::Ceiling;
            }
            match speech.try_read(&mut buffer) {
                TryRead::Samples(copied) => {
                    if ttfa.is_none() {
                        ttfa = Some(started.elapsed());
                    }
                    samples_written += copied;
                    if samples.len() < retain {
                        samples.extend_from_slice(&buffer[..copied.min(retain - samples.len())]);
                    }
                }
                TryRead::Ended => break Stop::Ended,
                TryRead::Starved => std::thread::sleep(READ_POLL),
            }
        };

        // Only the blocking read reports why an utterance ended.
        let mut timed_out = false;
        let stream_error = match stop {
            Stop::Ended => match speech.read(&mut buffer) {
                Ok(_) | Err(SynthesisError::AlreadyFailed) => None,
                Err(failure) => Some(failure),
            },
            Stop::Ceiling => {
                // Dropping the reader releases a writer waiting for buffer space.
                drop(speech);
                timed_out = true;
                None
            }
        };
        (
            worker.join(),
            stream_error,
            timed_out,
            ttfa,
            samples,
            samples_written,
        )
    });

    let (joined, stream_error, timed_out, ttfa, samples, samples_written) = outcome;
    // A ceiling cut is this drive's verdict, not the engine's; what the
    // worker did while being stopped stays in the log.
    let error = if timed_out {
        match joined {
            Ok(outcome) => tracing::debug!(?outcome, "the synthesis was cut by the ceiling"),
            Err(_panicked) => tracing::debug!("the synthesis panicked under the ceiling"),
        }
        Some("the synthesis ran past its time ceiling".to_owned())
    } else {
        let worker = match joined {
            Ok(Ok(())) => Worker::Finished,
            Ok(Err(failure)) => Worker::Failed(failure),
            Err(_panicked) => Worker::Panicked,
        };
        attempt_error(stream_error, worker, samples_written)
    };

    Synthesized {
        ttfa,
        wall: started.elapsed(),
        samples,
        samples_written,
        error,
    }
}

/// What the synthesis worker reported once it was joined.
enum Worker {
    Finished,
    Failed(SynthesisError),
    Panicked,
}

/// Which failure the attempt reports.
fn attempt_error(
    stream: Option<SynthesisError>,
    worker: Worker,
    samples_written: usize,
) -> Option<String> {
    match worker {
        // SourceGone only says the writer vanished, which is usually the
        // symptom rather than the cause: a worker that failed knows what
        // broke first. Any other stream cause outranks the worker.
        Worker::Failed(failure) => match stream {
            Some(stream) if !matches!(stream, SynthesisError::SourceGone) => reported(&stream),
            _ => reported(&failure),
        },
        // Unwinding closes the stream with SourceGone, which says less
        // than the panic. Any other stream error is the real cause.
        Worker::Panicked => match stream {
            Some(stream) if !matches!(stream, SynthesisError::SourceGone) => reported(&stream),
            _ => Some("the synthesis worker panicked".to_owned()),
        },
        Worker::Finished => match stream {
            Some(stream) => reported(&stream),
            // A clean end with no speech at all is a failure, not silence:
            // the backend kept its contract while producing nothing to hear.
            None if samples_written == 0 => Some("the synthesizer produced no speech".to_owned()),
            None => None,
        },
    }
}

fn reported(failure: &SynthesisError) -> Option<String> {
    let detail = error_chain(failure);
    tracing::warn!(%detail, "synthesis failed");
    Some(failure.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsupported_voice() -> SynthesisError {
        SynthesisError::UnsupportedVoice {
            backend: "kokoro",
            voice: "nobody".to_owned(),
        }
    }

    #[test]
    fn source_gone_yields_to_the_worker_cause() {
        let error = attempt_error(
            Some(SynthesisError::SourceGone),
            Worker::Failed(unsupported_voice()),
            0,
        );
        assert_eq!(error, Some(unsupported_voice().to_string()));
    }

    #[test]
    fn source_gone_stands_alone_when_the_worker_finished() {
        let error = attempt_error(Some(SynthesisError::SourceGone), Worker::Finished, 16);
        assert_eq!(error, Some(SynthesisError::SourceGone.to_string()));
    }

    #[test]
    fn a_real_stream_cause_outranks_the_worker() {
        let error = attempt_error(
            Some(unsupported_voice()),
            Worker::Failed(SynthesisError::SourceGone),
            0,
        );
        assert_eq!(error, Some(unsupported_voice().to_string()));
    }

    #[test]
    fn a_clean_end_without_speech_is_a_failure() {
        assert!(attempt_error(None, Worker::Finished, 0).is_some());
        assert_eq!(attempt_error(None, Worker::Finished, 1), None);
    }

    #[test]
    fn a_panicking_worker_is_reported() {
        let panicked = Some("the synthesis worker panicked".to_owned());
        assert_eq!(attempt_error(None, Worker::Panicked, 0), panicked);
        assert_eq!(
            attempt_error(Some(SynthesisError::SourceGone), Worker::Panicked, 0),
            panicked
        );
    }

    #[test]
    fn a_real_stream_cause_outranks_a_panic() {
        let error = attempt_error(Some(unsupported_voice()), Worker::Panicked, 0);
        assert_eq!(error, Some(unsupported_voice().to_string()));
    }
}
