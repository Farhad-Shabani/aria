//! In-process transcription through whisper.cpp bindings.
//!
//! Each decoded word carries whisper.cpp's token timing, which the
//! streaming path reads.
//!
//! ```no_run
//! use aria_stt::{whisper::Whisper, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let transcription = Transcription::streaming(Whisper::load("model")?)
//!     .with(Vocabulary::new(["aria-stt", "Cargo.toml"])?);
//! let mut session = Session::microphone(&transcription, &Language::Automatic)?;
//!
//! // Committed words arrive as you speak and are never revised.
//! while let Some(words) = session.next_finalized()? {
//!     print!("{}", words.as_str());
//! }
//!
//! let transcript = session.stop()?;
//! # Ok(())
//! # }
//! ```
//!
//! For audio you already hold, [`crate::Transcription::transcribe_file`] decodes a
//! WAV in one call. Streaming runs at [`CADENCE`].

pub use whisper_rs::install_logging_hooks;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
    WhisperTokenId,
};

use aria_kit::backend::{BackendError, ModelDownload, Unpack};
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{
    AudioInputKind, Cadence, Engine, Hypothesis, Language, SILENCE_FLOOR, Stream, StreamingEngine,
    TimedWord, Transcriber, TranscriptionAudio, TranscriptionError, check_cancelled, english_only,
    validate_model_file, word_timings,
};

/// Names this engine under `transcription.backend` and in its own errors.
pub const NAME: &str = "whisper";

/// large-v3-turbo, quantized to 8 bits.
pub const MODEL: &[ModelDownload] = &[ModelDownload {
    file_name: "ggml-large-v3-turbo-q8_0.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q8_0.bin",
    bytes: 874_188_075,
    sha256: "317eb69c11673c9de1e1f0d459b253999804ec71ac4c23c17ecf5fbe24e259a1",
    unpack: Unpack::Plain,
}];

const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const BACKEND: &str = NAME;
const DECODE_THREADS_FALLBACK: std::ffi::c_int = 4;
/// whisper.cpp reports token times in centiseconds.
const TOKEN_TIME_UNITS_PER_SECOND: f32 = 100.0;
/// Whisper's own silence rule: a segment is noise the model put words to only
/// when the no-speech probability is high and the words are also unlikely.
/// whisper.cpp copies the window's no-speech probability onto every segment
/// it decodes there, so the probability alone drops clear speech.
const NO_SPEECH_THRESHOLD: f32 = 0.6;
const LOG_PROBABILITY_THRESHOLD: f32 = -1.0;
/// One loudness frame: 512 samples, about 32 ms.
const LOUDNESS_FRAME_SAMPLES: usize = 512;

/// The one-second stride allows time for decoding. The twenty-second cap
/// keeps the buffer inside Whisper's thirty-second window.
pub const CADENCE: Cadence = Cadence::from_millis(1000, 20000);

/// On macOS, drop every `Whisper` before the process exits. ggml's Metal
/// teardown aborts the process when a model is still loaded, so an engine in
/// a `static` or on a thread still running at exit turns a clean exit into
/// SIGABRT.
pub struct Whisper {
    /// One resident decoding state: per-request states fail to encode on
    /// Metal after the first is freed, and re-init Metal buffers each run.
    /// Cost: the last transcript stays in memory between requests.
    state: Mutex<WhisperState>,
    /// The first special token id. Segment text leaves special tokens out,
    /// so the word timings must too.
    eot: WhisperTokenId,
    /// A `.en` model decodes English whatever the request names, so other
    /// languages are refused rather than ignored.
    multilingual: bool,
}

#[derive(Clone)]
pub struct WhisperConfig {
    /// The ggml model file.
    pub model: PathBuf,
}

impl WhisperConfig {
    pub fn new(model: impl Into<PathBuf>) -> Self {
        Self {
            model: model.into(),
        }
    }
}

impl Whisper {
    /// Loads the model once; it stays resident for every transcription.
    pub fn load(model: impl Into<PathBuf>) -> Result<Self, TranscriptionError> {
        Self::load_from(WhisperConfig::new(model), &CancelToken::never())
    }
}

impl Engine for Whisper {
    const NAME: &'static str = NAME;
    const MODEL: &'static [ModelDownload] = MODEL;
    const INPUT: AudioInputKind = AudioInputKind::Samples;

    type Config = WhisperConfig;

    fn check_from(config: &WhisperConfig) -> Result<PathBuf, TranscriptionError> {
        validate_model_file(config.model.clone(), BACKEND)
    }

    fn load_from(config: WhisperConfig, cancel: &CancelToken) -> Result<Self, TranscriptionError> {
        check_cancelled(cancel)?;
        let model = Self::check_from(&config)?;
        let context = WhisperContext::new_with_params(&model, WhisperContextParameters::default())
            .map_err(|source| load_failed(&model, source))?;
        let state = context
            .create_state()
            .map_err(|source| load_failed(&model, source))?;
        check_cancelled(cancel)?;
        Ok(Self {
            state: Mutex::new(state),
            eot: context.token_eot(),
            multilingual: context.is_multilingual(),
        })
    }
}

impl StreamingEngine for Whisper {
    const STREAM: Stream = Stream::Agreement(CADENCE);
}

impl Transcriber for Whisper {
    fn validate_language(
        &self,
        language: &Language,
        _cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        supported_language(self.multilingual, language)
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        check_cancelled(cancel)?;
        let samples = audio.require_samples(BACKEND)?;
        if !audible(samples) {
            return Ok(Hypothesis::new(""));
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        run_inference(&mut state, language, samples, cancel)?;
        collect_transcript(&state, self.eot)
    }
}

fn load_failed(model: &Path, source: whisper_rs::WhisperError) -> TranscriptionError {
    TranscriptionError::Load {
        backend: BACKEND,
        model: model.display().to_string(),
        source: BackendError::engine(source),
    }
}

fn supported_language(multilingual: bool, language: &Language) -> Result<(), TranscriptionError> {
    if multilingual {
        return Ok(());
    }
    english_only(BACKEND, language)
}

fn inference_failed(source: whisper_rs::WhisperError) -> TranscriptionError {
    TranscriptionError::Inference {
        backend: BACKEND,
        source: BackendError::engine(source),
    }
}

fn run_inference(
    state: &mut WhisperState,
    language: &Language,
    samples: &[f32],
    cancel: &CancelToken,
) -> Result<(), TranscriptionError> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let params = decode_params(language, &cancelled);
    let result = run_with_cancellation(&cancelled, cancel, || state.full(params, samples));
    match result {
        Ok(_decoded_segments) => Ok(()),
        Err(_aborted) if cancelled.load(Ordering::Acquire) => Err(TranscriptionError::Cancelled),
        Err(error) => Err(inference_failed(error)),
    }
}

fn decode_params<'language>(
    language: &'language Language,
    cancelled: &Arc<AtomicBool>,
) -> FullParams<'language, 'static> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some(language.as_str()));
    params.set_n_threads(decode_threads());
    params.set_token_timestamps(true);
    params.set_suppress_nst(true);
    let abort_flag = Arc::clone(cancelled);
    // Pre-boxed on purpose: whisper-rs 0.16 stores the callback as
    // `Box<dyn FnMut() -> bool>` but its C trampoline casts back to the
    // caller's type. A bare closure is misread as the box and aborts at
    // random; this exact type makes the cast sound.
    let abort_when_cancelled: Box<dyn FnMut() -> bool> =
        Box::new(move || abort_flag.load(Ordering::Acquire));
    params.set_abort_callback_safe(abort_when_cancelled);
    params
}

// whisper.cpp defaults to four threads regardless of core count, so pass the
// real count.
fn decode_threads() -> std::ffi::c_int {
    std::thread::available_parallelism()
        .map(|cores| cores.get() as std::ffi::c_int)
        .unwrap_or(DECODE_THREADS_FALLBACK)
}

/// Whether any frame of the window rises above the silence floor.
fn audible(samples: &[f32]) -> bool {
    samples
        .chunks(LOUDNESS_FRAME_SAMPLES)
        .map(|frame| frame.iter().map(|sample| sample * sample).sum::<f32>() / frame.len() as f32)
        .any(|energy| energy >= SILENCE_FLOOR)
}

/// The segments' text and the timing of each word in it. Segments join with
/// a space, so a word never spans two.
fn collect_transcript(
    state: &WhisperState,
    eot: WhisperTokenId,
) -> Result<Hypothesis, TranscriptionError> {
    let mut text = String::new();
    let mut words = Vec::new();
    let mut silent_segments = 0_usize;
    for segment in state.as_iter() {
        let spoken = segment.to_str_lossy().map_err(inference_failed)?;
        let spoken = spoken.trim();
        if spoken.is_empty() {
            continue;
        }
        let mut tokens = Vec::new();
        let mut log_probabilities = Vec::new();
        for index in 0..segment.n_tokens() {
            let Some(token) = segment.get_token(index) else {
                continue;
            };
            if token.token_id() >= eot {
                continue;
            }
            let timed = token.token_data();
            let timing = TimedWord {
                start: timed.t0 as f32 / TOKEN_TIME_UNITS_PER_SECOND,
                end: timed.t1 as f32 / TOKEN_TIME_UNITS_PER_SECOND,
            };
            log_probabilities.push(timed.plog);
            tokens.push((token.to_str_lossy().map_err(inference_failed)?, timing));
        }
        if is_silence(segment.no_speech_probability(), &log_probabilities) {
            silent_segments += 1;
            continue;
        }
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(spoken);
        words.extend(word_timings(
            tokens.iter().map(|(text, timing)| (text.as_ref(), *timing)),
        ));
    }
    if silent_segments > 0 {
        tracing::debug!(silent_segments, "dropped segments that read as silence");
    }
    Ok(Hypothesis::with_words(text, words))
}

fn is_silence(no_speech_probability: f32, log_probabilities: &[f32]) -> bool {
    let unlikely_words = log_probabilities.is_empty()
        || (log_probabilities.iter().sum::<f32>() / log_probabilities.len() as f32)
            < LOG_PROBABILITY_THRESHOLD;
    no_speech_probability > NO_SPEECH_THRESHOLD && unlikely_words
}

/// Runs `inference` while a watcher mirrors `cancel` into the flag
/// the abort callback reads; the watcher stops when inference returns.
fn run_with_cancellation<T: Send>(
    cancelled: &AtomicBool,
    cancel: &CancelToken,
    inference: impl FnOnce() -> T + Send,
) -> T {
    let finished = AtomicBool::new(false);
    thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            while !finished.load(Ordering::Acquire) {
                if cancel.is_cancelled() {
                    cancelled.store(true, Ordering::Release);
                    return;
                }
                thread::park_timeout(CANCEL_POLL_INTERVAL);
            }
        });
        let result = inference();
        finished.store(true, Ordering::Release);
        watcher.thread().unpark();
        result
    })
}

#[cfg(test)]
mod load_engine_tests {
    use aria_kit::transcription::LanguageCode;

    use super::*;

    #[test]
    fn silence_is_not_audible_and_a_quiet_word_is() {
        assert!(!audible(&[0.0; 16_000]));
        assert!(
            !audible(&[0.003; 16_000]),
            "room noise stays under the floor"
        );
        assert!(audible(&[0.03; 16_000]), "a quiet word rises above it");
    }

    #[test]
    fn a_segment_is_silence_only_when_its_words_are_also_unlikely() {
        let confident = [-0.1, -0.3, -0.2];
        let doubtful = [-1.5, -2.0, -1.2];

        assert!(
            !is_silence(0.72, &confident),
            "a window-wide no-speech score must not drop clear speech"
        );
        assert!(is_silence(0.72, &doubtful));
        assert!(!is_silence(0.2, &doubtful));
        assert!(is_silence(0.72, &[]));
    }

    #[test]
    fn an_english_only_model_refuses_other_languages_and_a_multilingual_one_takes_any() {
        let french = Language::Specific(LanguageCode::new("fr").expect("fr is a valid code"));
        let english = Language::Specific(LanguageCode::english());

        assert!(supported_language(true, &french).is_ok());
        assert!(supported_language(false, &english).is_ok());
        assert!(supported_language(false, &Language::Automatic).is_ok());
        assert!(matches!(
            supported_language(false, &french),
            Err(TranscriptionError::UnsupportedLanguage { backend, language })
                if backend == BACKEND && language == "fr"
        ));
    }

    fn name_of<E: Engine>() -> &'static str {
        E::NAME
    }

    #[test]
    fn the_trait_carries_the_engine_identity() {
        assert_eq!(name_of::<Whisper>(), NAME);
    }

    #[test]
    fn cancelled_load_stops_before_model_validation() {
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome = Whisper::load_from(WhisperConfig::new("/nonexistent/model.bin"), &cancel);

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
    }
}
