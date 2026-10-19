//! Round-trip intelligibility: what an ASR judge hears back.
//!
//! Speed says nothing about whether a synthesizer is understood. Each
//! utterance is spoken once more, written as canonical audio, transcribed,
//! and scored against its own text with the corpus scorer.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::stt::{Alignment, word_errors, words};
use aria::config::TranscriptionDevice;
use aria::transcription::{
    Language, LanguageCode, Transcriber, Transcript, Transcription, TranscriptionError, Whole,
};
use aria_kit::audio::{CaptureError, Clip, ClipError, RecordingReader, ResampleError};
use aria_kit::cancel::CancelToken;
use aria_kit::primitives::error_chain;
use aria_kit::synthesis::{SpeakingRate, SpeechText, Voice};

use super::backends::Loaded;
use super::latency;

/// Requests cancellation after this budget. The backend must poll the token;
/// a blocked native call can exceed the limit.
const JUDGE_CEILING: Duration = Duration::from_secs(120);

/// How the round-trip judge transcribes, chosen from the benchmark's flags.
#[derive(Clone)]
pub enum AsrRequest {
    Parakeet { model_directory: PathBuf, gpu: bool },
    WhisperCandle { model_directory: PathBuf },
}

/// One loaded judge over one loaded ASR backend.
pub(super) struct Judge {
    transcription: Transcription<Box<dyn Transcriber>, Whole>,
}

impl Judge {
    pub(super) fn load(request: &AsrRequest) -> Result<Self, TranscriptionError> {
        let backend: Box<dyn Transcriber> = match request {
            AsrRequest::Parakeet {
                model_directory,
                gpu,
            } => Box::new(aria_stt::parakeet::Parakeet::load(
                model_directory.clone(),
                match gpu {
                    true => TranscriptionDevice::Gpu,
                    false => TranscriptionDevice::Cpu,
                },
            )?),
            AsrRequest::WhisperCandle { model_directory } => Box::new(
                aria_stt::whisper_candle::WhisperCandle::load(model_directory.clone())?,
            ),
        };
        Ok(Self {
            // A whole-buffer decode: the recording is already complete, so
            // streaming would add nothing but pacing.
            transcription: Transcription::complete_samples(backend),
        })
    }

    /// What the judge hears in a finished WAV file.
    fn hear(&self, mut recording: RecordingReader) -> Result<String, TranscriptionError> {
        let language = Language::Specific(LanguageCode::english());
        let cancel = CancelToken::from_fn({
            let started = Instant::now();
            move || started.elapsed() > JUDGE_CEILING
        });
        self.transcription
            .transcribe_with(&mut recording, &language, &cancel)
            .map(Transcript::into_text)
    }
}

/// What came back for one utterance.
pub(super) struct Heard {
    pub reference_words: usize,
    pub alignment: Option<Alignment>,
    pub error: Option<String>,
}

impl Heard {
    fn failed(reference_words: usize, error: String) -> Self {
        Self {
            reference_words,
            alignment: None,
            error: Some(error),
        }
    }
}

/// Why a synthesized attempt could not become canonical audio for the judge.
#[derive(Debug, thiserror::Error)]
enum RecordingFailure {
    #[error("the engine reported an unusable sample rate")]
    SampleRate(#[from] ClipError),

    #[error("cannot convert the speech to 16 kHz")]
    Resample(#[from] ResampleError),

    #[error("cannot spool the judged recording")]
    Spool(#[from] CaptureError),
}

/// Speaks one utterance, judges what came back, and scores it against the
/// text. Failures land in the row they belong to; the run continues.
pub(super) fn hear_back(
    engine: &Loaded,
    judge: &Judge,
    recordings: &Path,
    text: &SpeechText,
    voice: Option<&Voice>,
    rate: SpeakingRate,
) -> Heard {
    let reference_words = words(text.as_str()).len();
    let synthesized = latency::synthesize(engine, text, voice, rate);
    if synthesized.error.is_none() && synthesized.samples.len() < synthesized.samples_written {
        return Heard::failed(
            reference_words,
            "the speech ran past its retained length".to_owned(),
        );
    }
    if let Some(error) = synthesized.error {
        return Heard::failed(reference_words, error);
    }

    let recording = match judged_recording(recordings, engine.sample_rate(), synthesized.samples) {
        Ok(recording) => recording,
        Err(failure) => {
            let detail = error_chain(&failure);
            tracing::warn!(%detail, "cannot spool the judged recording");
            return Heard::failed(reference_words, "recording failed".to_owned());
        }
    };

    match judge.hear(recording) {
        Ok(hypothesis) => {
            // An empty hearing is legitimate — it scores as deletions, not
            // as a broken run.
            Heard {
                reference_words,
                alignment: Some(word_errors(text.as_str(), &hypothesis)),
                error: None,
            }
        }
        Err(TranscriptionError::Cancelled) => Heard::failed(
            reference_words,
            "the judge ran past its time ceiling".to_owned(),
        ),
        Err(failure) => {
            tracing::warn!(error = %failure, "the judge heard nothing");
            Heard::failed(reference_words, "transcription failed".to_owned())
        }
    }
}

/// Canonical audio for the judge, spooled as 16-bit PCM the way capture
/// spools it. The spool deletes itself when the judge drops it.
fn judged_recording(
    recordings: &Path,
    sample_rate: u32,
    speech: Vec<f32>,
) -> Result<RecordingReader, RecordingFailure> {
    let canonical = Clip::new(sample_rate, speech)?.into_canonical()?;
    Ok(RecordingReader::from_samples_in(
        recordings,
        canonical.samples(),
    )?)
}
