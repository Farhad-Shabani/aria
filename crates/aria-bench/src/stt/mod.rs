//! Word error rate for each transcription backend, over one shared corpus.
//!
//! Latency says nothing about whether a backend heard the words. Every engine
//! gets the same audio, the same normalization, and the same scorer. Every
//! utterance leaves a row behind, so the result can be checked.

pub mod analysis;
mod backends;
pub mod baseline;
mod conditions;
mod corpus;
mod normalize;
mod record;
mod score;
mod stats;
pub mod vocabulary;

use aria_harness::artifact::{Provenance, fingerprint, fingerprint_value, write_json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use aria::primitives::error_chain;
use aria::transcription::{Language, LanguageCode, TranscriptionError};
use aria_kit::audio::{
    AudioCapture, CaptureError, Recording, RecordingMode, RecordingReader, ReplayPace,
    ResampleError, StopSignal, WavError, WavReplayCapture, read_wav,
};

pub use backends::{BackendRequest, SelectionError, Skipped};
pub use corpus::CorpusError;
pub use normalize::words;
pub use record::RecordError;
pub use score::{Alignment, reference_matches, word_errors};

use backends::{Loaded, Selection};
use conditions::{Conditions, Replay};
use corpus::{Corpus, Utterance};
use record::{Descriptor, Preview, Record, RecordWriter, Segment};

/// Longer than any single utterance a read-speech corpus holds.
const MAXIMUM_UTTERANCE: Duration = Duration::from_secs(120);

pub use aria::transcription::Cadence;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Whole,
    Streaming,
}

/// What to measure, and where to put the results.
pub struct Request {
    /// A manifest of audio paths and reference text, one utterance per line.
    pub corpus: PathBuf,
    /// Where the per-utterance result rows are written.
    pub results: Option<PathBuf>,
    pub backends: Vec<BackendRequest>,
    /// Replaces an agreement-backed engine's own decode cadence, for a run
    /// that sweeps strides rather than models.
    pub cadence: Option<Cadence>,
    pub mode: Option<Mode>,
    pub baseline: Option<PathBuf>,
    /// How many times each backend hears the whole corpus. Under --realtime,
    /// streaming engines commit on live timing, so one pass understates
    /// their variance.
    pub passes: u32,
    /// Replay streamed audio at wall-clock pace instead of one stride per
    /// decode. Slower, but reproduces a live microphone's backlog dynamics;
    /// results are not comparable across the two paces.
    pub realtime: bool,
    pub provisional: bool,
}

/// What a finished run produced.
pub struct Report {
    pub results: PathBuf,
    pub corpus_utterances: usize,
    pub corpus_words: usize,
    pub summaries: Vec<Summary>,
    /// Backends set aside because their model was not there.
    pub skipped: Vec<Skipped>,
    passes: u32,
}

/// How far one backend's transcripts fell from the reference.
pub struct Summary {
    pub backend: &'static str,
    conditions: Conditions,
    reference_words: usize,
    pub substitutions: usize,
    pub deletions: usize,
    pub insertions: usize,
    /// Attempts the backend could not transcribe at all. Their reference
    /// words still count against it, so a failing backend cannot score well by
    /// answering less.
    pub failures: usize,
    /// Scored rows in total: utterances times passes.
    pub attempts: usize,
    rows: BTreeMap<String, (usize, usize)>,
}

impl Summary {
    fn new(descriptor: &Descriptor, conditions: Conditions) -> Self {
        Self {
            backend: descriptor.name,
            conditions,
            reference_words: 0,
            substitutions: 0,
            deletions: 0,
            insertions: 0,
            failures: 0,
            attempts: 0,
            rows: BTreeMap::new(),
        }
    }

    /// Folds one scored utterance in. Every counter the report shows is a sum
    /// of the rows, so the two can never disagree.
    fn add(&mut self, record: &Record) {
        self.reference_words += record.reference_words;
        self.substitutions += record.substitutions;
        self.deletions += record.deletions;
        self.insertions += record.insertions;
        self.failures += usize::from(record.error.is_some());
        self.attempts += 1;
        let cluster = self.rows.entry(record.utterance.clone()).or_default();
        cluster.0 += record.reference_words;
        cluster.1 += record.substitutions + record.deletions + record.insertions;
    }

    pub fn wer_interval(&self) -> Option<(f64, f64)> {
        stats::wer_interval(&self.rows.values().copied().collect::<Vec<_>>())
    }

    fn errors(&self) -> usize {
        self.substitutions + self.deletions + self.insertions
    }

    pub fn word_error_rate(&self) -> f64 {
        if self.reference_words == 0 {
            return 0.0;
        }
        self.errors() as f64 / self.reference_words as f64
    }
}

/// Measures every requested backend over the corpus, one model resident at a
/// time.
pub fn run(request: &Request) -> Result<Report, BenchError> {
    if request.passes == 0 {
        return Err(BenchError::Invalid("passes must be positive".to_owned()));
    }
    let baseline = request
        .baseline
        .as_deref()
        .map(analysis::Artifact::load)
        .transpose()?;
    let corpus = Corpus::load(&request.corpus)?;
    let (selections, skipped) = backends::selections(
        &request.backends,
        request.cadence,
        request.mode,
        request.realtime,
    )?;
    let provenance = Provenance::capture()?;
    let results = match &request.results {
        Some(path) => path.clone(),
        None => {
            let directory = PathBuf::from("build/bench-stt").join(&provenance.run_id);
            std::fs::create_dir_all(&directory)
                .map_err(aria_harness::artifact::ArtifactError::from)?;
            directory.join("results.jsonl")
        }
    };
    if results.exists() || analysis::manifest_path(&results).exists() {
        return Err(BenchError::Invalid(
            "result artifact already exists".to_owned(),
        ));
    }
    let engines = selections
        .iter()
        .map(|selection| selection.identity(request.realtime, request.provisional))
        .collect::<Result<Vec<_>, _>>()?;
    let mut cases = BTreeMap::new();
    for utterance in &corpus.utterances {
        if normalize::words(&utterance.reference).is_empty() {
            return Err(BenchError::Invalid(format!(
                "utterance {} has no scored reference words",
                utterance.id
            )));
        }
        let identity = fingerprint_value(&(fingerprint(&utterance.audio)?, &utterance.reference))?;
        if cases.insert(utterance.id.clone(), identity).is_some() {
            return Err(BenchError::Invalid(format!(
                "duplicate utterance id {}",
                utterance.id
            )));
        }
    }
    let language = Language::Specific(LanguageCode::english());
    let mut manifest = analysis::Manifest {
        schema: record::SCHEMA_VERSION,
        provenance,
        corpus_sha256: fingerprint_value(&cases)?,
        scorer_sha256: analysis::scorer_fingerprint()?,
        cases,
        engines,
        language: language.as_str().to_owned(),
        timing_origin: "transcription-start-after-capture-setup".to_owned(),
        passes: request.passes,
        provisional: request.provisional,
        rows: 0,
        results_sha256: String::new(),
    };
    if let Some(baseline) = baseline {
        baseline.validate_candidate(&manifest)?;
    }
    let spool_dir = TempDir::new().map_err(BenchError::Recordings)?;

    let mut writer = RecordWriter::create(&results)?;
    let mut summaries = Vec::new();
    for selection in &selections {
        summaries.push(measure(
            selection,
            &language,
            &corpus,
            spool_dir.path(),
            &manifest.provenance.run_id,
            request.passes,
            request.realtime,
            request.provisional,
            &mut writer,
        )?);
    }

    let results = writer.finish()?;
    manifest.rows = summaries.iter().map(|summary| summary.attempts).sum();
    manifest.results_sha256 = fingerprint(&results)?;
    write_json(&analysis::manifest_path(&results), &manifest)?;
    Ok(Report {
        results,
        corpus_utterances: corpus.utterances.len(),
        corpus_words: reference_words(&corpus),
        summaries,
        skipped,
        passes: request.passes,
    })
}

#[allow(clippy::too_many_arguments)]
fn measure(
    selection: &Selection,
    language: &Language,
    corpus: &Corpus,
    spool_dir: &Path,
    run_id: &str,
    passes: u32,
    realtime: bool,
    provisional: bool,
    writer: &mut RecordWriter,
) -> Result<Summary, BenchError> {
    let descriptor = &selection.descriptor;
    let mut loaded = selection.load().map_err(|source| BenchError::Load {
        backend: descriptor.name,
        source,
    })?;
    let conditions = Conditions::new(loaded.decoding, realtime, provisional);
    let mut summary = Summary::new(descriptor, conditions);
    let total = corpus.utterances.len();
    for pass in 1..=passes {
        for (index, utterance) in corpus.utterances.iter().enumerate() {
            // Progress goes to stderr; the row streams to the results file.
            eprintln!(
                "{} pass {pass}/{passes}: {}/{total} {}",
                descriptor.name,
                index + 1,
                utterance.id
            );
            let heard = hear(&mut loaded, language, utterance, spool_dir, conditions);
            let record = Record::score(
                &heard,
                utterance,
                descriptor.clone(),
                run_id,
                pass,
                conditions,
            );
            summary.add(&record);
            writer.write(&record)?;
        }
    }
    Ok(summary)
}

/// One backend's attempt at one utterance, before it is scored.
struct Heard {
    /// Empty when the backend heard no speech, or failed.
    hypothesis: String,
    error: Option<String>,
    audio_ms: u128,
    /// Wall clock of the whole attempt.
    transcribe_ms: u128,
    /// Time the engine spent decoding, excluding the wait for audio. Under
    /// realtime replay this is the only honest real-time factor input.
    decode_ms: u128,
    segments: Vec<Segment>,
    previews: Vec<Preview>,
    decode_calls: u64,
}

impl Heard {
    fn failed(error: String) -> Self {
        Self {
            hypothesis: String::new(),
            error: Some(error),
            audio_ms: 0,
            transcribe_ms: 0,
            decode_ms: 0,
            segments: Vec::new(),
            previews: Vec::new(),
            decode_calls: 0,
        }
    }
}

/// Replays one utterance through the backend and times the attempt.
fn hear(
    loaded: &mut Loaded,
    language: &Language,
    utterance: &Utterance,
    spool_dir: &Path,
    conditions: Conditions,
) -> Heard {
    // A streaming engine must get audio as capture would feed it. Handed
    // everything at once, its first fill sees the whole utterance and
    // agreement collapses into one decode. By default the decoder is the
    // clock: one stride per finished decode. --realtime uses the wall clock
    // instead. A whole decode reads the recording once, so pacing it would
    // only slow the sweep.
    let (replayed, advance) = match conditions.replay {
        Replay::Stepped { stride_samples } => {
            let (advance, gate) = mpsc::channel();
            (
                stepped_replay(&utterance.audio, stride_samples, gate),
                Some(advance),
            )
        }
        Replay::Realtime => (
            wav_replay(utterance, spool_dir, ReplayPace::Realtime).map_err(ReplayError::Capture),
            None,
        ),
        Replay::Immediate => (
            wav_replay(utterance, spool_dir, ReplayPace::Immediate).map_err(ReplayError::Capture),
            None,
        ),
    };
    let mut recording = match replayed {
        Ok(recording) => recording,
        Err(failure) => {
            // The error names the corpus path and the user running it. Keep
            // those out of the results file — log them instead.
            tracing::warn!(utterance = %utterance.id, error = ?failure, "replay failed");
            return Heard::failed("replay failed".to_owned());
        }
    };

    let attempt = loaded.attempt(advance);
    let started = Instant::now();
    let mut segments = Vec::new();
    let mut previews = Vec::new();
    let heard = attempt.transcription.transcribe(
        &mut recording,
        language,
        started,
        &mut segments,
        conditions.previews.then_some(&mut previews),
    );
    let decode_ms = attempt.decode_ms();
    let decode_calls = attempt.decode_calls();
    drop(attempt);
    let transcribe_ms = started.elapsed().as_millis();
    // Sampled after the attempt: before then, realtime replay has not
    // delivered all the audio.
    let audio_ms = recording.duration().as_millis();

    let (hypothesis, error) = match heard {
        Ok(transcript) => (transcript.into_text(), None),
        // Silence is a legitimate transcript of an utterance the backend could
        // not make out, and scores as deletions rather than as a broken run.
        Err(TranscriptionError::NoSpeech) => (String::new(), None),
        Err(failure) => (String::new(), Some(error_chain(&failure))),
    };

    Heard {
        hypothesis,
        error,
        audio_ms,
        transcribe_ms,
        decode_ms,
        segments,
        previews,
        decode_calls,
    }
}

fn wav_replay(
    utterance: &Utterance,
    spool_dir: &Path,
    pace: ReplayPace,
) -> Result<RecordingReader, CaptureError> {
    WavReplayCapture::new(spool_dir, MAXIMUM_UTTERANCE, &utterance.audio, pace)
        .and_then(|replay| replay.start(RecordingMode::UntilStopped, StopSignal::never()))
}

/// A recording that releases one stride of audio per finished decode: the
/// first stride is pushed up front so the opening fill returns, then each
/// permit from `Timed` releases the next.
///
/// The feeder finishes the recording right after the last batch, without
/// waiting for another permit — the decode covering that batch is the one
/// that would send it, and its `fill_through` only returns once the
/// recording has ended. Dropping the sender ends the feeder early.
fn stepped_replay(
    audio: &Path,
    stride: usize,
    gate: mpsc::Receiver<()>,
) -> Result<RecordingReader, ReplayError> {
    let clip = read_wav(audio)?;
    // The WAV replay's length limit, applied before resampling pays for it.
    if clip.duration() > MAXIMUM_UTTERANCE {
        return Err(WavError::TooLong {
            path: audio.to_path_buf(),
            maximum_duration: MAXIMUM_UTTERANCE,
        }
        .into());
    }
    let samples = clip.into_canonical()?.into_samples();

    let (recording, mut push) = Recording::open();
    std::thread::spawn(move || {
        let mut batches = samples.chunks(stride);
        if let Some(first) = batches.next() {
            push.push(first);
        }
        for batch in batches {
            if gate.recv().is_err() {
                break;
            }
            push.push(batch);
        }
        push.finish();
    });
    Ok(recording)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_kit::audio::canonical_fixture;

    const FIXTURE_SAMPLES: u64 = 16_000;
    const STRIDE: u64 = 1_600;

    #[test]
    fn a_missing_baseline_fails_before_corpus_or_model_work() {
        let directory = tempfile::tempdir().unwrap();
        let request = Request {
            corpus: directory.path().join("missing.tsv"),
            results: Some(directory.path().join("results.jsonl")),
            backends: Vec::new(),
            cadence: None,
            mode: None,
            baseline: Some(directory.path().join("missing.jsonl")),
            passes: 1,
            realtime: false,
            provisional: false,
        };
        assert!(matches!(run(&request), Err(BenchError::Analysis(_))));
        assert!(!request.results.unwrap().exists());
    }

    #[test]
    fn a_stepped_replay_releases_one_stride_per_decode() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());
        let (advance, gate) = mpsc::channel();
        let mut recording =
            stepped_replay(&fixture, STRIDE as usize, gate).expect("the fixture replays");

        recording
            .fill_through(STRIDE)
            .expect("the first stride is pushed up front");
        assert_eq!(recording.held_to(), STRIDE);

        for release in 2..=FIXTURE_SAMPLES / STRIDE {
            advance.send(()).expect("the feeder is waiting on the gate");
            recording
                .fill_through(release * STRIDE)
                .expect("one permit releases exactly one stride");
            assert_eq!(recording.held_to(), release * STRIDE);
        }
        recording.fill().expect("the last batch ends the recording");
        assert!(
            recording.reached_end(),
            "no further permit is needed to finish"
        );
        assert_eq!(recording.held_to(), FIXTURE_SAMPLES);
    }

    #[test]
    fn dropping_the_gate_finishes_the_recording() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());
        let (advance, gate) = mpsc::channel();
        let mut recording =
            stepped_replay(&fixture, STRIDE as usize, gate).expect("the fixture replays");

        drop(advance);

        recording
            .fill()
            .expect("the feeder finishes on a closed gate");
        assert!(recording.reached_end(), "an abandoned replay cannot hang");
        assert_eq!(recording.held_to(), STRIDE);
    }
}

/// Counted with the scorer's own word split, so the header and the rates share
/// a denominator.
fn reference_words(corpus: &Corpus) -> usize {
    corpus
        .utterances
        .iter()
        .map(|utterance| normalize::words(&utterance.reference).len())
        .sum()
}

#[derive(Debug, thiserror::Error)]
enum ReplayError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error(transparent)]
    Wav(#[from] WavError),
    #[error(transparent)]
    Resample(#[from] ResampleError),
}

#[derive(Debug, thiserror::Error)]
pub enum BenchError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Artifact(#[from] aria_harness::artifact::ArtifactError),
    #[error(transparent)]
    Analysis(#[from] analysis::AnalysisError),
    #[error(transparent)]
    Corpus(#[from] CorpusError),

    #[error(transparent)]
    Selection(#[from] SelectionError),

    #[error(transparent)]
    Record(#[from] RecordError),

    #[error("failed to create a directory for replayed recordings")]
    Recordings(#[source] std::io::Error),

    #[error("failed to load the {backend} model")]
    Load {
        backend: &'static str,
        #[source]
        source: TranscriptionError,
    },
}
