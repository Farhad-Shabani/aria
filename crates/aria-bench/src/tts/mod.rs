//! What each TTS backend costs in time, and whether it stays intelligible.
//!
//! Every backend speaks the same corpus the same number of times. Each
//! attempt leaves a row behind; each backend leaves a summary. A missing
//! model skips its backend rather than failing the run — this benchmarks
//! what the machine has.

mod backends;
mod corpus;
mod latency;
mod quality;
mod record;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aria::transcription::TranscriptionError;
use aria_kit::synthesis::{SpeakingRate, SynthesisError, Voice};
use tempfile::TempDir;

pub use backends::{EngineRequest, Skipped};
pub use corpus::CorpusError;
pub use quality::AsrRequest;
pub use record::RecordError;

use backends::{Loaded, Selection};
use corpus::Corpus;
use latency::Synthesized;
use quality::Judge;
use record::{LatencyRow, Phase, QualityRow, RowWriter};

/// What to measure, and where to put the results.
pub struct Request {
    /// Text to speak, one utterance per line; `None` speaks the corpus
    /// shipped with the benchmark.
    pub corpus: Option<PathBuf>,
    pub results: PathBuf,
    pub backends: Vec<EngineRequest>,
    /// The voice every backend is asked for; `None` means each engine's
    /// configured default.
    pub voice: Option<Voice>,
    pub rate: SpeakingRate,
    /// How many times every utterance is spoken for the latency phase.
    pub runs: usize,
    /// When set, a second phase scores round-trip intelligibility.
    pub judge: Option<AsrRequest>,
}

/// What a finished run produced.
pub struct Report {
    pub results: PathBuf,
    pub corpus_utterances: usize,
    pub summaries: Vec<BackendSummary>,
    pub skipped: Vec<Skipped>,
}

/// One backend's numbers over the whole run.
pub struct BackendSummary {
    pub backend: &'static str,
    /// Model load time, paid once per run.
    pub load_seconds: f64,
    pub attempts: usize,
    pub failures: usize,
    pub median_ttfa_ms: Option<u128>,
    pub p95_ttfa_ms: Option<u128>,
    pub median_rtf: Option<f64>,
    /// Round-trip word error rate when the judge ran; every failed
    /// utterance counts its whole text against the backend.
    pub wer: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error(transparent)]
    Corpus(#[from] CorpusError),

    #[error("failed to load the ASR judge")]
    Judge(#[source] TranscriptionError),

    // Not transparent: the record errors name the results path, and the
    // chain belongs in the log rather than on the terminal.
    #[error("failed to write the results")]
    Record(#[from] RecordError),

    #[error("failed to load the {backend} model")]
    Load {
        backend: &'static str,
        #[source]
        source: SynthesisError,
    },

    #[error("no backend's model could be used; the run would measure nothing")]
    NoBackend,

    #[error("a run needs at least one attempt per utterance")]
    NoRuns,

    #[error("failed to create a directory for judged recordings")]
    Recordings(#[source] std::io::Error),
}

/// Measures every usable backend over the corpus, one model resident at a
/// time.
pub fn run(request: &Request) -> Result<Report, BenchError> {
    if request.runs == 0 {
        return Err(BenchError::NoRuns);
    }
    let spoken = corpus::load(request.corpus.as_deref())?;
    let (selections, skipped) = Selection::selections(&request.backends);
    if selections.is_empty() {
        return Err(BenchError::NoBackend);
    }
    let recordings = TempDir::new().map_err(BenchError::Recordings)?;

    let mut writer = RowWriter::create(&request.results)?;

    // Latency runs before the judge exists: an ASR model resident during
    // synthesis would contend for memory and compute and skew every timing.
    // The price is that a judged backend loads twice.
    let mut timed = Vec::new();
    for selection in &selections {
        // A model that vanishes between the skip check and this load fails
        // the run loudly rather than silently shrinking the comparison.
        timed.push(measure_latency(selection, &spoken, request, &mut writer)?);
    }

    let judge = match &request.judge {
        Some(asr) => Some(Judge::load(asr).map_err(BenchError::Judge)?),
        None => None,
    };
    let mut summaries = Vec::new();
    for (selection, timed) in selections.iter().zip(&timed) {
        let wer = match &judge {
            Some(judge) => round_trip(
                &selection.load().map_err(|source| BenchError::Load {
                    backend: selection.name,
                    source,
                })?,
                judge,
                recordings.path(),
                &spoken,
                request,
                &mut writer,
            )?,
            None => None,
        };
        summaries.push(summarize(selection.name, timed, wer));
    }

    writer.finish()?;
    Ok(Report {
        results: request.results.clone(),
        corpus_utterances: spoken.utterances.len(),
        summaries,
        skipped,
    })
}

fn measure_latency(
    selection: &Selection,
    spoken: &Corpus,
    request: &Request,
    writer: &mut RowWriter,
) -> Result<TimedAttempts, BenchError> {
    let started = Instant::now();
    let engine = selection.load().map_err(|source| BenchError::Load {
        backend: selection.name,
        source,
    })?;
    let load_seconds = started.elapsed().as_secs_f64();

    // Latency first: no attempt may inherit warm-up from another phase.
    let mut timed = TimedAttempts {
        load_seconds,
        ..TimedAttempts::default()
    };
    for run in 0..request.runs {
        for utterance in &spoken.utterances {
            let attempt = latency::synthesize(
                &engine,
                &utterance.text,
                request.voice.as_ref(),
                request.rate,
            );
            timed.observe(&attempt, engine.sample_rate());
            write_latency(
                writer,
                selection.name,
                utterance.id,
                run,
                &attempt,
                engine.sample_rate(),
            )?;
        }
    }
    Ok(timed)
}

fn summarize(backend: &'static str, timed: &TimedAttempts, wer: Option<f64>) -> BackendSummary {
    BackendSummary {
        backend,
        load_seconds: timed.load_seconds,
        attempts: timed.attempts,
        failures: timed.failures,
        median_ttfa_ms: timed.median_ttfa_ms(),
        p95_ttfa_ms: timed.p95_ttfa_ms(),
        median_rtf: timed.median_rtf(),
        wer,
    }
}

/// Every attempt one backend made, kept for the percentiles. A failed
/// attempt is counted but never ranked.
#[derive(Default)]
struct TimedAttempts {
    load_seconds: f64,
    attempts: usize,
    failures: usize,
    ttfa: Vec<Duration>,
    rtf: Vec<f64>,
}

impl TimedAttempts {
    fn observe(&mut self, attempt: &Synthesized, sample_rate: u32) {
        self.attempts += 1;
        self.failures += usize::from(attempt.error.is_some());
        // A failed attempt's partial numbers say how it broke, not how fast
        // the backend is; the row keeps them, the percentiles do not.
        if attempt.error.is_some() {
            return;
        }
        if let Some(ttfa) = attempt.ttfa {
            self.ttfa.push(ttfa);
        }
        if let Some(rtf) = attempt.rtf(sample_rate) {
            self.rtf.push(rtf);
        }
    }

    fn median_ttfa_ms(&self) -> Option<u128> {
        self.ttfa_percentile(0.5).map(|ttfa| ttfa.as_millis())
    }

    fn p95_ttfa_ms(&self) -> Option<u128> {
        self.ttfa_percentile(0.95).map(|ttfa| ttfa.as_millis())
    }

    fn ttfa_percentile(&self, fraction: f64) -> Option<Duration> {
        let mut sorted = self.ttfa.clone();
        sorted.sort();
        percentile(&sorted, fraction).copied()
    }

    fn median_rtf(&self) -> Option<f64> {
        let mut sorted = self.rtf.clone();
        sorted.sort_by(|left, right| left.total_cmp(right));
        percentile(&sorted, 0.5).copied()
    }
}

/// The nearest-rank value of a sorted slice at `fraction`, or nothing when
/// there is nothing to rank.
fn percentile<T>(sorted: &[T], fraction: f64) -> Option<&T> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (sorted.len() as f64 * fraction).ceil() as usize;
    sorted.get(rank.saturating_sub(1))
}

fn write_latency(
    writer: &mut RowWriter,
    backend: &'static str,
    utterance: usize,
    run: usize,
    attempt: &Synthesized,
    sample_rate: u32,
) -> Result<(), BenchError> {
    let audio_ms = attempt.samples_written as u128 * 1_000 / u128::from(sample_rate);
    writer.row(&LatencyRow {
        phase: Phase::Latency,
        backend,
        utterance,
        run,
        ttfa_ms: attempt.ttfa.map(|ttfa| ttfa.as_millis()),
        wall_ms: attempt.wall.as_millis(),
        audio_ms,
        rtf: attempt.rtf(sample_rate),
        error: attempt.error.clone(),
    })?;
    Ok(())
}

/// Speaks every utterance once more, judges what came back, and answers the
/// word error rate over the whole corpus. A corpus that normalizes to no
/// words at all has nothing to score and answers none.
fn round_trip(
    engine: &Loaded,
    judge: &Judge,
    recordings: &std::path::Path,
    spoken: &Corpus,
    request: &Request,
    writer: &mut RowWriter,
) -> Result<Option<f64>, BenchError> {
    let mut reference_words = 0;
    let mut errors = 0;
    for utterance in &spoken.utterances {
        let heard = quality::hear_back(
            engine,
            judge,
            recordings,
            &utterance.text,
            request.voice.as_ref(),
            request.rate,
        );
        reference_words += heard.reference_words;
        let (substitutions, deletions, insertions) = match heard.alignment {
            Some(alignment) => {
                errors += alignment.substitutions + alignment.deletions + alignment.insertions;
                (
                    alignment.substitutions,
                    alignment.deletions,
                    alignment.insertions,
                )
            }
            // A failed attempt is scored like silence: everything deleted.
            None => {
                errors += heard.reference_words;
                (0, heard.reference_words, 0)
            }
        };
        writer.row(&QualityRow {
            phase: Phase::Quality,
            backend: engine.name(),
            utterance: utterance.id,
            reference_words: heard.reference_words,
            substitutions,
            deletions,
            insertions,
            error: heard.error,
        })?;
    }

    if reference_words == 0 {
        return Ok(None);
    }
    Ok(Some(errors as f64 / reference_words as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(ttfa_ms: u64, wall_ms: u64, samples_written: usize) -> Synthesized {
        Synthesized {
            ttfa: Some(Duration::from_millis(ttfa_ms)),
            wall: Duration::from_millis(wall_ms),
            samples: Vec::new(),
            samples_written,
            error: None,
        }
    }

    fn failed_attempt() -> Synthesized {
        Synthesized {
            error: Some("broke".to_owned()),
            ..attempt(10, 20, 16_000)
        }
    }

    #[test]
    fn nothing_ranks_in_an_empty_slice() {
        assert_eq!(percentile::<u32>(&[], 0.5), None);
    }

    #[test]
    fn one_value_is_every_percentile() {
        assert_eq!(percentile(&[7], 0.5), Some(&7));
        assert_eq!(percentile(&[7], 0.95), Some(&7));
    }

    #[test]
    fn an_even_count_takes_the_lower_of_the_middle_pair() {
        assert_eq!(percentile(&[1, 2, 3, 4], 0.5), Some(&2));
    }

    #[test]
    fn the_p95_of_twenty_is_the_nineteenth() {
        let sorted: Vec<u32> = (1..=20).collect();
        assert_eq!(percentile(&sorted, 0.95), Some(&19));
    }

    #[test]
    fn a_failed_attempt_counts_but_never_ranks() {
        let mut timed = TimedAttempts::default();
        timed.observe(&attempt(30, 1_000, 16_000), 16_000);
        timed.observe(&failed_attempt(), 16_000);
        assert_eq!(timed.attempts, 2);
        assert_eq!(timed.failures, 1);
        assert_eq!(timed.median_ttfa_ms(), Some(30));
        assert_eq!(timed.median_rtf(), Some(1.0));
    }

    #[test]
    fn an_attempt_without_audio_has_no_rate() {
        assert_eq!(attempt(5, 10, 0).rtf(16_000), None);
    }
}
