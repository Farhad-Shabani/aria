use wasm_bindgen::prelude::*;

use aria_kit::audio::{Recording, RecordingReader, RecordingWriter};
use aria_kit::primitives::error_chain;
use aria_kit::transcription::{Language, Transcription, TranscriptionError, Whole};
use aria_stt::whisper_candle::WhisperCandle;

use crate::silence::SilenceGate;

/// One dictation: audio pushed in, a transcript out.
#[wasm_bindgen]
pub struct Dictation {
    capture: RecordingWriter,
    recording: RecordingReader,
    transcription: Transcription<WhisperCandle, Whole>,
    gate: SilenceGate,
}

#[wasm_bindgen]
impl Dictation {
    /// Loads a Whisper model the page has already fetched, and opens a
    /// recording to push into.
    ///
    /// `trailing_silence_ms` is how long the speaker must stay quiet after
    /// speaking before [`Dictation::speaker_stopped`] returns true.
    #[wasm_bindgen(constructor)]
    pub fn new(
        weights: &[u8],
        tokenizer: &[u8],
        config: &[u8],
        trailing_silence_ms: u32,
    ) -> Result<Self, JsError> {
        let gate = SilenceGate::new(trailing_silence_ms)
            .map_err(|error| JsError::new(&error_chain(&error)))?;
        let engine = WhisperCandle::from_bytes(weights, tokenizer, config).map_err(failed)?;
        let transcription = Transcription::complete_samples(engine);
        let (recording, capture) = Recording::open();
        Ok(Self {
            capture,
            recording,
            transcription,
            gate,
        })
    }

    /// Takes one frame of mono audio, already resampled to 16 kHz.
    pub fn push_audio(&mut self, samples: &[f32]) {
        self.capture.push(samples);
        self.gate.hear(samples);
    }

    /// Whether the speaker has spoken and then gone quiet for the configured
    /// pause. A page polls this to decide when to call [`Dictation::finish`].
    pub fn speaker_stopped(&self) -> bool {
        self.gate.stopped()
    }

    /// Ends the recording and transcribes everything pushed. A recording with
    /// no speech in it answers with empty text rather than an error.
    pub fn finish(mut self) -> Result<String, JsError> {
        self.capture.finish();
        match self
            .transcription
            .transcribe(&mut self.recording, &Language::Automatic)
        {
            Ok(transcript) => Ok(transcript.into_text()),
            Err(TranscriptionError::NoSpeech) => Ok(String::new()),
            Err(error) => Err(failed(error)),
        }
    }
}

fn failed(error: TranscriptionError) -> JsError {
    JsError::new(&error_chain(&error))
}
