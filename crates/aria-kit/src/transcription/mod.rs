//! The speech-to-text contract engine crates implement.

mod agreement;
pub mod layer;
mod native;
mod session;
mod spacing;

pub use layer::{Backtrack, Layer, Layered, RemoveHesitations, RemoveRepeats, Vocabulary};
pub use spacing::Caret;

use std::path::PathBuf;

#[cfg(not(target_family = "wasm"))]
use crate::audio::RecordedAudio;
use crate::audio::{CANONICAL_SAMPLE_RATE, CaptureError, RecordingReader};
use crate::backend::{BackendError, ModelDownload, ModelError};
use crate::cancel::CancelToken;
pub use agreement::Cadence;
pub use session::Session;

/// The embedder-facing surface, re-exported by `aria-stt` so a consumer
/// imports one library. Expert knobs — cancellation, event
/// delivery, custom cadences — stay out and take an explicit `aria-kit`
/// dependency.
pub mod prelude {
    #[cfg(all(feature = "silero", not(target_family = "wasm")))]
    pub use super::session::HeardSession;
    pub use super::{
        Backtrack, Caret, Language, Layer, RemoveHesitations, RemoveRepeats, Session, Streaming,
        Transcript, TranscriptSegment, Transcription, TranscriptionError, Vocabulary, Whole,
    };
    pub use crate::audio::RecordingReader;
    #[cfg(all(feature = "microphone", not(target_family = "wasm")))]
    pub use crate::audio::{Microphone, RecordingMode};
    pub use crate::cancel::CancelToken;
    pub use crate::speaker::{Diarizer, Embedder};
    #[cfg(all(feature = "silero", not(target_family = "wasm")))]
    pub use crate::speaker::{Heard, SegmentLabeler, SegmentReader, Verdict};
}

const MAX_LANGUAGE_CODE_BYTES: usize = 64;

/// Silence cutoff near -43 dBFS, used to reduce hallucinations on quiet audio.
pub const SILENCE_FLOOR: f32 = 5e-5;

/// The contract every speech-to-text engine implements.
///
/// An implementation must:
/// - poll `cancel` at its natural pause points and answer
///   [`TranscriptionError::Cancelled`];
/// - accept only its supported [`TranscriptionAudio`] representation;
/// - wrap its own failures in [`BackendError::engine`] under
///   [`TranscriptionError::Load`] or [`TranscriptionError::Inference`];
/// - refuse languages it cannot honour — [`english_only`] covers
///   English-only models;
/// - validate model paths with [`validate_model_file`] or
///   [`validate_model_directory`] before reading them, on targets that have
///   a filesystem.
pub trait Transcriber: Send + Sync + 'static {
    fn validate_language(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError>;

    /// Transcribes one bounded audio input: a whole recording or a streaming
    /// buffer, which the engine cannot tell apart.
    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError>;

    /// A decoder with its own state that takes audio a chunk at a time, for
    /// an engine whose encoder streams. `None`, the default, means the
    /// engine decodes bounded inputs only, and streaming runs by agreement.
    /// A wrapper around another transcriber must forward this, or a native
    /// stream fails with [`TranscriptionError::NoNativeStream`].
    fn open_stream(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
        let _ = (language, cancel);
        Ok(None)
    }

    /// Runs a whole operation on the transcriber's side, for one that decodes
    /// elsewhere. Only [`Transcription::delegated`] calls it; `incremental`
    /// says whether the mode streams. The default `None` fails that
    /// transcription with [`TranscriptionError::NoDelegatedOperation`].
    fn open_operation(
        &self,
        language: &Language,
        incremental: bool,
        cancel: &CancelToken,
    ) -> Result<Option<OperationSource>, TranscriptionError> {
        let _ = (language, incremental, cancel);
        Ok(None)
    }
}

/// One live decode of an engine that streams natively: audio in by the
/// chunk, final words out. A word that came out is never revised.
pub trait ChunkStream: Send {
    /// How many canonical samples the encoder takes per step.
    fn chunk_samples(&self) -> usize;

    /// Feeds one chunk, exactly [`chunk_samples`](Self::chunk_samples)
    /// long, and answers with what it let go of.
    fn feed(&mut self, chunk: &[f32]) -> Result<Emitted, TranscriptionError>;
}

/// What one chunk let go of. A model writes words in pieces, and a word can
/// finish in the chunk after it started; `joins_previous` says the first
/// piece here continues the previous chunk's last word rather than opening
/// a new one. Word timings are seconds from the start of the chunk.
pub struct Emitted {
    pub words: Hypothesis,
    pub joins_previous: bool,
}

/// How an engine streams: by re-decoding a growing buffer until two
/// decodes agree on its words, or natively, one chunk at a time.
#[derive(Clone, Copy)]
pub enum Stream {
    Agreement(Cadence),
    Native,
}

/// Shared construction contract: an engine name, configuration, validation, and loading.
pub trait Engine: Transcriber + Sized {
    const NAME: &'static str;

    /// The files to download for this engine, all into one folder. Empty
    /// when nothing is downloaded for it. An engine that reads a second
    /// folder lists that folder's files in a const of its own.
    const MODEL: &'static [ModelDownload];

    /// The one audio representation this engine accepts. Callers read it to
    /// hand over the right input instead of guessing and failing at runtime.
    const INPUT: AudioInputKind;

    type Config;

    /// Resolves what a load would read, without reading it or starting a
    /// child.
    fn check_from(config: &Self::Config) -> Result<PathBuf, TranscriptionError>;

    fn load_from(config: Self::Config, cancel: &CancelToken) -> Result<Self, TranscriptionError>;
}

/// An engine with a tested streaming cadence or native chunking policy.
pub trait StreamingEngine: Engine {
    const STREAM: Stream;
}

impl<T: Transcriber + ?Sized> Transcriber for std::sync::Arc<T> {
    fn validate_language(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        (**self).validate_language(language, cancel)
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        (**self).transcribe_with(audio, language, cancel)
    }

    fn open_stream(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
        (**self).open_stream(language, cancel)
    }

    fn open_operation(
        &self,
        language: &Language,
        incremental: bool,
        cancel: &CancelToken,
    ) -> Result<Option<OperationSource>, TranscriptionError> {
        (**self).open_operation(language, incremental, cancel)
    }
}

impl<T: Transcriber + ?Sized> Transcriber for Box<T> {
    fn validate_language(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        (**self).validate_language(language, cancel)
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        (**self).transcribe_with(audio, language, cancel)
    }

    fn open_stream(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
        (**self).open_stream(language, cancel)
    }

    fn open_operation(
        &self,
        language: &Language,
        incremental: bool,
        cancel: &CancelToken,
    ) -> Result<Option<OperationSource>, TranscriptionError> {
        (**self).open_operation(language, incremental, cancel)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioInputKind {
    Samples,
    Wav,
}

impl std::fmt::Display for AudioInputKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Samples => formatter.write_str("canonical samples"),
            Self::Wav => formatter.write_str("a completed WAV"),
        }
    }
}

/// One bounded input to a speech-to-text engine.
pub enum TranscriptionAudio<'a> {
    Samples(&'a [f32]),
    #[cfg(not(target_family = "wasm"))]
    Wav(&'a RecordedAudio),
}

impl<'a> TranscriptionAudio<'a> {
    pub fn kind(&self) -> AudioInputKind {
        match self {
            Self::Samples(_) => AudioInputKind::Samples,
            #[cfg(not(target_family = "wasm"))]
            Self::Wav(_) => AudioInputKind::Wav,
        }
    }

    pub fn require_samples(self, backend: &'static str) -> Result<&'a [f32], TranscriptionError> {
        #[cfg(target_family = "wasm")]
        let _ = backend;
        match self {
            Self::Samples(samples) => Ok(samples),
            #[cfg(not(target_family = "wasm"))]
            Self::Wav(_) => Err(TranscriptionError::UnsupportedAudioInput {
                backend,
                received: AudioInputKind::Wav,
                required: AudioInputKind::Samples,
            }),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn require_wav(
        self,
        backend: &'static str,
    ) -> Result<&'a RecordedAudio, TranscriptionError> {
        match self {
            Self::Wav(audio) => Ok(audio),
            Self::Samples(_) => Err(TranscriptionError::UnsupportedAudioInput {
                backend,
                received: AudioInputKind::Samples,
                required: AudioInputKind::Wav,
            }),
        }
    }
}

impl std::fmt::Debug for TranscriptionAudio<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TranscriptionAudio")
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

/// One decoded word's time inside the decoded audio window, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimedWord {
    pub start: f32,
    pub end: f32,
}

impl TimedWord {
    /// Where the word sits in the stream, for a window opening at `origin`.
    pub fn span_from(&self, origin: u64) -> std::ops::Range<u64> {
        let position = |seconds: f32| {
            let samples = (f64::from(seconds) * f64::from(CANONICAL_SAMPLE_RATE)).round() as u64;
            origin.saturating_add(samples)
        };
        let start = position(self.start);
        start..position(self.end).max(start)
    }
}

/// Text decoded from one bounded audio input. Silence is empty.
pub struct Hypothesis {
    text: String,
    words: Vec<TimedWord>,
}

impl Hypothesis {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into().trim().to_owned(),
            words: Vec::new(),
        }
    }

    /// Timings aligned one-to-one with the text's whitespace words. Dropped
    /// whole when the counts disagree — misaligned timings never escape.
    pub fn with_words(text: impl Into<String>, words: Vec<TimedWord>) -> Self {
        let mut hypothesis = Self::new(text);
        if hypothesis.text.split_whitespace().count() == words.len() {
            hypothesis.words = words;
        }
        hypothesis
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Empty when the text has no aligned timings.
    pub fn words(&self) -> &[TimedWord] {
        &self.words
    }

    pub fn into_text(self) -> String {
        self.text
    }
}

/// Times the words the tokens spell. Each token is its text and its time
/// inside the window. A word runs from one non-blank to the next blank;
/// it starts with its first token and ends with its last. U+2581,
/// SentencePiece's marker, counts as a blank, so the words match what
/// [`Hypothesis::with_words`] counts once the text spells that marker as
/// a space.
pub fn word_timings<'a>(tokens: impl IntoIterator<Item = (&'a str, TimedWord)>) -> Vec<TimedWord> {
    let mut words: Vec<TimedWord> = Vec::new();
    let mut open = false;
    for (text, timing) in tokens {
        for character in text.chars() {
            if character.is_whitespace() || character == '\u{2581}' {
                open = false;
                continue;
            }
            match words.last_mut() {
                Some(word) if open => word.end = word.end.max(timing.end),
                _ => {
                    words.push(timing);
                    open = true;
                }
            }
        }
    }
    words
}

impl std::fmt::Debug for Hypothesis {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Hypothesis")
            .field("bytes", &self.text.len())
            .field("timed_words", &self.words.len())
            .finish()
    }
}

pub struct Transcription<T, M: Mode, L = ()> {
    plan: Plan<M::Config>,
    transcriber: T,
    layers: L,
}

#[derive(Clone, Copy)]
enum Plan<C> {
    Local(C),
    Delegated,
}

pub struct Operation<'a, T, L = ()> {
    transcriber: &'a T,
    language: &'a Language,
    cancel: CancelToken,
    strategy: Strategy,
    read_to: u64,
    transcript: Option<Transcript>,
    layers: &'a L,
    output: Option<layer::Output>,
}

use sealed::{Strategy, WholeRecordingInput};

struct ChunkStep {
    update: Option<TranscriptUpdate>,
    transcript: Option<Transcript>,
}

pub enum OperationStep {
    Update {
        update: TranscriptUpdate,
        retained_from: u64,
        read_to: u64,
        completed: Option<Transcript>,
    },
    Complete {
        transcript: Transcript,
        retained_from: u64,
        read_to: u64,
    },
}

type OperationCallback =
    dyn FnMut(&mut RecordingReader, bool) -> Result<OperationStep, TranscriptionError> + Send;

pub struct OperationSource {
    next: Box<OperationCallback>,
    retained_from: u64,
    read_to: u64,
    completed: Option<Transcript>,
    finished: bool,
}

impl OperationSource {
    /// `next(reader, previews)` must fill `reader` through each `read_to`,
    /// and fully before `Complete`.
    pub fn new(
        next: impl FnMut(&mut RecordingReader, bool) -> Result<OperationStep, TranscriptionError>
        + Send
        + 'static,
    ) -> Self {
        Self {
            next: Box::new(next),
            retained_from: 0,
            read_to: 0,
            completed: None,
            finished: false,
        }
    }

    fn next(
        &mut self,
        recording: &mut RecordingReader,
        previews: bool,
        cancel: &CancelToken,
    ) -> Result<Option<ChunkStep>, TranscriptionError> {
        while !self.finished {
            check_cancelled(cancel)?;
            let step = (self.next)(recording, previews)?;
            check_cancelled(cancel)?;
            match step {
                OperationStep::Update {
                    update,
                    retained_from,
                    read_to,
                    completed,
                } => {
                    self.retained_from = retained_from;
                    self.read_to = read_to;
                    if let Some(transcript) = completed {
                        self.completed = Some(transcript);
                    }
                    if previews || matches!(update, TranscriptUpdate::Finalized(_)) {
                        return Ok(Some(ChunkStep {
                            update: Some(update),
                            transcript: self.completed.take(),
                        }));
                    }
                }
                OperationStep::Complete {
                    transcript,
                    retained_from,
                    read_to,
                } => {
                    self.retained_from = retained_from;
                    self.read_to = read_to;
                    self.finished = true;
                    return Ok(Some(ChunkStep {
                        update: None,
                        transcript: Some(transcript),
                    }));
                }
            }
        }
        Ok(None)
    }
}

/// Public items here stay unnameable outside this module.
mod sealed {
    use super::{
        CancelToken, Language, OperationSource, Transcriber, TranscriptionError, agreement, native,
    };

    pub enum Strategy {
        WholeRecording(WholeRecordingInput),
        Agreement(Box<agreement::AgreementState>),
        Native(Box<native::NativeState>),
        Delegated(OperationSource),
    }

    #[derive(Clone, Copy)]
    pub enum WholeRecordingInput {
        Samples,
        #[cfg(not(target_family = "wasm"))]
        Wav,
    }

    /// What a mode runs, out of reach of the crates that name it.
    pub trait Sealed {
        type Config: Copy + Send + Sync + 'static;

        /// The strategy one operation runs. Takes the transcriber because a
        /// native stream is opened on it.
        fn strategy<T: Transcriber>(
            config: Self::Config,
            transcriber: &T,
            language: &Language,
            cancel: &CancelToken,
        ) -> Result<Strategy, TranscriptionError>;
    }
}

/// The transcription strategy a [`Transcription`] runs, carried in its type.
/// The set is closed: downstream crates can name a mode, never add one.
#[doc(hidden)]
pub trait Mode: sealed::Sealed + 'static {
    const INCREMENTAL: bool;
}

/// One decode of the whole recording.
#[derive(Clone, Copy)]
pub struct Whole;

impl Mode for Whole {
    const INCREMENTAL: bool = false;
}

impl sealed::Sealed for Whole {
    type Config = WholeRecordingInput;

    fn strategy<T: Transcriber>(
        config: Self::Config,
        _transcriber: &T,
        _language: &Language,
        _cancel: &CancelToken,
    ) -> Result<Strategy, TranscriptionError> {
        Ok(Strategy::WholeRecording(config))
    }
}

/// Words as they are spoken: LocalAgreement-2 over a growing buffer, or the
/// engine's own chunked decode.
#[derive(Clone, Copy)]
pub struct Streaming;

impl Mode for Streaming {
    const INCREMENTAL: bool = true;
}

impl sealed::Sealed for Streaming {
    type Config = Stream;

    fn strategy<T: Transcriber>(
        config: Self::Config,
        transcriber: &T,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Strategy, TranscriptionError> {
        match config {
            Stream::Agreement(cadence) => Ok(Strategy::Agreement(Box::new(
                agreement::AgreementState::new(cadence),
            ))),
            Stream::Native => match transcriber.open_stream(language, cancel)? {
                Some(stream) => Ok(Strategy::Native(Box::new(native::NativeState::new(stream)))),
                None => Err(TranscriptionError::NoNativeStream),
            },
        }
    }
}

impl<T> Transcription<T, Whole> {
    pub fn complete_samples(transcriber: T) -> Self {
        Self {
            plan: Plan::Local(WholeRecordingInput::Samples),
            transcriber,
            layers: (),
        }
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn complete_wav(transcriber: T) -> Self {
        Self {
            plan: Plan::Local(WholeRecordingInput::Wav),
            transcriber,
            layers: (),
        }
    }

    /// One decode of the whole recording, handed over as `input` — an
    /// engine's [`Engine::INPUT`].
    #[cfg(not(target_family = "wasm"))]
    pub fn complete(input: AudioInputKind, transcriber: T) -> Self {
        match input {
            AudioInputKind::Samples => Self::complete_samples(transcriber),
            AudioInputKind::Wav => Self::complete_wav(transcriber),
        }
    }
}

impl<T> Transcription<T, Streaming> {
    /// The escape hatch: agreement over an engine that has not verified a
    /// cadence, for experiments. A supported streaming path goes through
    /// [`Transcription::streaming`].
    pub fn agreement(cadence: Cadence, transcriber: T) -> Self {
        Self::stream(Stream::Agreement(cadence), transcriber)
    }

    /// Streams the way `stream` says, over any transcriber. A wrapper around
    /// an engine passes that engine's [`StreamingEngine::STREAM`] here.
    pub fn stream(stream: Stream, transcriber: T) -> Self {
        Self {
            plan: Plan::Local(stream),
            transcriber,
            layers: (),
        }
    }
}

impl<T, M: Mode> Transcription<T, M> {
    /// Runs each operation through the transcriber's own
    /// [`Transcriber::open_operation`].
    pub fn delegated(transcriber: T) -> Self {
        Self {
            plan: Plan::Delegated,
            transcriber,
            layers: (),
        }
    }
}

impl<T: StreamingEngine> Transcription<T, Streaming> {
    pub fn streaming(engine: T) -> Self {
        Self::stream(T::STREAM, engine)
    }
}

impl<T, M: Mode, L: Layer> Transcription<T, M, L> {
    pub fn with<N: Layer>(self, layer: N) -> Transcription<T, M, Layered<N, L>> {
        Transcription {
            plan: self.plan,
            transcriber: self.transcriber,
            layers: Layered::new(self.layers, layer),
        }
    }
}

impl<T: Transcriber, M: Mode, L: Layer> Transcription<T, M, L> {
    /// Starts an operation without a cancellation token.
    pub fn start<'a>(
        &'a self,
        language: &'a Language,
    ) -> Result<Operation<'a, T, L>, TranscriptionError> {
        self.start_with(language, &CancelToken::never())
    }

    pub fn start_with<'a>(
        &'a self,
        language: &'a Language,
        cancel: &CancelToken,
    ) -> Result<Operation<'a, T, L>, TranscriptionError> {
        check_cancelled(cancel)?;
        self.transcriber.validate_language(language, cancel)?;
        check_cancelled(cancel)?;
        let strategy = match self.plan {
            Plan::Local(config) => M::strategy(config, &self.transcriber, language, cancel)?,
            Plan::Delegated => self
                .transcriber
                .open_operation(language, M::INCREMENTAL, cancel)?
                .map(Strategy::Delegated)
                .ok_or(TranscriptionError::NoDelegatedOperation)?,
        };
        check_cancelled(cancel)?;
        Ok(Operation {
            transcriber: &self.transcriber,
            language,
            cancel: cancel.clone(),
            strategy,
            read_to: 0,
            transcript: None,
            layers: &self.layers,
            output: self.layers.enabled().then(layer::Output::default),
        })
    }

    pub fn transcribe_with(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Transcript, TranscriptionError> {
        self.start_with(language, cancel)?.finish(recording)
    }

    pub fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
    ) -> Result<Transcript, TranscriptionError> {
        self.transcribe_with(recording, language, &CancelToken::never())
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn transcribe_file(
        &self,
        path: impl AsRef<std::path::Path>,
    ) -> Result<Transcript, TranscriptionError> {
        let mut recording =
            RecordingReader::from_wav_file(path).map_err(TranscriptionError::Capture)?;
        self.transcribe(&mut recording, &Language::Automatic)
    }
}

impl<T: Transcriber, L: Layer> Operation<'_, T, L> {
    pub fn next_finalized(
        &mut self,
        recording: &mut RecordingReader,
    ) -> Result<Option<TranscriptSegment>, TranscriptionError> {
        loop {
            match self.next(recording, false)? {
                Some(TranscriptUpdate::Finalized(segment)) => return Ok(Some(segment)),
                Some(TranscriptUpdate::Provisional(_)) => {}
                None => return Ok(None),
            }
        }
    }

    /// The completed transcript, available from the step that ends the
    /// recording — before the caller has drained the operation. A streaming
    /// caller checks it so a delivery that fails afterwards cannot lose the
    /// utterance.
    pub fn completed(&self) -> Option<&Transcript> {
        self.transcript.as_ref()
    }

    /// How far audio had been read for decoding when the latest update was
    /// produced. A local strategy reads the caller's recording; a delegated
    /// one reports where the transcriber read to.
    pub fn read_to(&self) -> u64 {
        self.read_to
    }

    /// The earliest stream position this operation may still read. A caller
    /// done with everything behind it can release that audio.
    ///
    /// Whole-recording transcription holds position zero: it hands the
    /// transcriber the entire recording at once.
    pub fn retained_from(&self) -> u64 {
        let strategy = self.strategy_retained_from();
        self.output
            .as_ref()
            .and_then(|output| output.retained_from)
            .map_or(strategy, |retained| retained.min(strategy))
    }

    fn strategy_retained_from(&self) -> u64 {
        match &self.strategy {
            Strategy::WholeRecording(_) => 0,
            Strategy::Agreement(state) => state.retained_from(),
            Strategy::Native(state) => state.retained_from(),
            Strategy::Delegated(source) => source.retained_from,
        }
    }

    pub fn next_update(
        &mut self,
        recording: &mut RecordingReader,
    ) -> Result<Option<TranscriptUpdate>, TranscriptionError> {
        self.next(recording, true)
    }

    /// The next update, with provisional words only when `previews` asks
    /// for them. Without previews, none are built.
    pub fn next(
        &mut self,
        recording: &mut RecordingReader,
        previews: bool,
    ) -> Result<Option<TranscriptUpdate>, TranscriptionError> {
        let Some(mut output) = self.output.take() else {
            return self.next_raw_step(recording, previews);
        };
        let result = check_cancelled(&self.cancel)
            .and_then(|()| self.next_processed(recording, previews, &mut output));
        if result.is_err() {
            output.finished = true;
            output.pending.clear();
            output.text.clear();
            self.transcript = None;
        }
        self.output = Some(output);
        result
    }

    fn next_processed(
        &mut self,
        recording: &mut RecordingReader,
        previews: bool,
        output: &mut layer::Output,
    ) -> Result<Option<TranscriptUpdate>, TranscriptionError> {
        loop {
            if let Some(update) = output.pending.pop_front() {
                return Ok(Some(update));
            }
            if output.finished {
                return Ok(None);
            }
            let update = self.next_raw_step(recording, previews)?;
            let finished = update.is_none() || self.transcript.is_some();
            if let Some(update) = update {
                output.push(update, self.layers, &self.cancel)?;
            }
            if finished {
                // Completion includes queued segments; process that tail before exposing it.
                output.retained_from = Some(self.strategy_retained_from());
                while let Some(update) = self.next_raw_step(recording, previews)? {
                    output.push(update, self.layers, &self.cancel)?;
                }
                self.transcript = if output.text.is_empty() {
                    None
                } else {
                    Some(Transcript(std::mem::take(&mut output.text)))
                };
                output.finished = true;
            }
        }
    }

    fn next_raw_step(
        &mut self,
        recording: &mut RecordingReader,
        previews: bool,
    ) -> Result<Option<TranscriptUpdate>, TranscriptionError> {
        let update = self.decode_step(recording, previews);
        self.read_to = match &self.strategy {
            Strategy::Delegated(source) => source.read_to,
            Strategy::WholeRecording(_) | Strategy::Agreement(_) | Strategy::Native(_) => {
                recording.held_to()
            }
        };
        update
    }

    fn decode_step(
        &mut self,
        recording: &mut RecordingReader,
        previews: bool,
    ) -> Result<Option<TranscriptUpdate>, TranscriptionError> {
        let step = match &mut self.strategy {
            Strategy::WholeRecording(input) => {
                if self.transcript.is_some() {
                    return Ok(None);
                }
                let completed = transcribe_complete(
                    *input,
                    recording,
                    self.transcriber,
                    self.language,
                    &self.cancel,
                )?;
                let segment = TranscriptSegment {
                    text: completed.as_str().to_owned(),
                    span: None,
                };
                self.transcript = Some(completed);
                ChunkStep {
                    update: Some(TranscriptUpdate::Finalized(segment)),
                    transcript: None,
                }
            }
            Strategy::Agreement(state) => {
                let cancel = self.cancel.clone();
                match state.next_with(recording, &cancel, previews, |samples| {
                    self.transcriber.transcribe_with(
                        TranscriptionAudio::Samples(samples),
                        self.language,
                        &cancel,
                    )
                })? {
                    Some(step) => step,
                    None => return Ok(None),
                }
            }
            Strategy::Native(state) => match state.next_finalized_with(recording, &self.cancel)? {
                Some(step) => step,
                None => return Ok(None),
            },
            Strategy::Delegated(source) => match source.next(recording, previews, &self.cancel)? {
                Some(step) => step,
                None => return Ok(None),
            },
        };
        if let Some(transcript) = step.transcript {
            self.transcript = Some(transcript);
        }
        Ok(step.update)
    }

    pub fn finish(
        &mut self,
        recording: &mut RecordingReader,
    ) -> Result<Transcript, TranscriptionError> {
        while self.next_finalized(recording)?.is_some() {}
        self.transcript.take().ok_or(TranscriptionError::NoSpeech)
    }
}

fn transcribe_complete<T: Transcriber>(
    input: WholeRecordingInput,
    recording: &mut RecordingReader,
    transcriber: &T,
    language: &Language,
    cancel: &CancelToken,
) -> Result<Transcript, TranscriptionError> {
    check_cancelled(cancel)?;
    let hypothesis = match input {
        WholeRecordingInput::Samples => {
            recording.fill().map_err(TranscriptionError::Capture)?;
            check_cancelled(cancel)?;
            transcriber.transcribe_with(
                TranscriptionAudio::Samples(recording.samples()),
                language,
                cancel,
            )?
        }
        #[cfg(not(target_family = "wasm"))]
        WholeRecordingInput::Wav => {
            let audio = recording
                .finished_audio()
                .map_err(TranscriptionError::Capture)?;
            check_cancelled(cancel)?;
            transcriber.transcribe_with(TranscriptionAudio::Wav(audio), language, cancel)?
        }
    };
    check_cancelled(cancel)?;
    Transcript::new(hypothesis.into_text())
}

/// [`TranscriptionError::Cancelled`] once `cancel` fires.
pub fn check_cancelled(cancel: &CancelToken) -> Result<(), TranscriptionError> {
    if cancel.is_cancelled() {
        return Err(TranscriptionError::Cancelled);
    }
    Ok(())
}

pub enum TranscriptUpdate {
    Finalized(TranscriptSegment),
    Provisional(String),
}

#[derive(Clone)]
pub struct TranscriptSegment {
    text: String,
    span: Option<std::ops::Range<u64>>,
}

impl TranscriptSegment {
    pub fn new(
        text: impl Into<String>,
        span: Option<std::ops::Range<u64>>,
    ) -> Result<Self, TranscriptionError> {
        let text = text.into().trim().to_owned();
        if text.is_empty() || span.as_ref().is_some_and(|span| span.start > span.end) {
            return Err(TranscriptionError::InvalidSegment);
        }
        Ok(Self { text, span })
    }

    fn spanned(words: &[String], span: Option<std::ops::Range<u64>>) -> Option<Self> {
        let text = words.join(" ");
        (!text.is_empty()).then_some(Self { text, span })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Where the segment's audio sits in its recording, when word timestamps
    /// pin it. Lets a caller measure the silence between two segments.
    pub fn span(&self) -> Option<std::ops::Range<u64>> {
        self.span.clone()
    }

    pub fn into_text(self) -> String {
        self.text
    }
}

impl std::fmt::Debug for TranscriptSegment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TranscriptSegment")
            .field("bytes", &self.text.len())
            .field("span", &self.span)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Language {
    Automatic,
    Specific(LanguageCode),
}

impl Language {
    const AUTOMATIC: &str = "auto";

    pub fn as_str(&self) -> &str {
        match self {
            Self::Automatic => Self::AUTOMATIC,
            Self::Specific(code) => code.as_str(),
        }
    }
}

/// Reads back what [`Language::as_str`] writes.
impl std::str::FromStr for Language {
    type Err = LanguageCodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.trim().eq_ignore_ascii_case(Self::AUTOMATIC) {
            return Ok(Self::Automatic);
        }
        LanguageCode::new(value).map(Self::Specific)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageCode(String);

impl LanguageCode {
    pub fn english() -> Self {
        Self("en".to_owned())
    }

    pub fn new(value: impl Into<String>) -> Result<Self, LanguageCodeError> {
        let value = value.into();
        let value = value.trim();
        if value.is_empty() {
            return Err(LanguageCodeError::Empty);
        }
        if value.len() > MAX_LANGUAGE_CODE_BYTES {
            return Err(LanguageCodeError::TooLong {
                actual_bytes: value.len(),
                maximum_bytes: MAX_LANGUAGE_CODE_BYTES,
            });
        }
        let mut segments = value.split('-');
        let valid_primary = segments.next().is_some_and(|segment| {
            !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_alphabetic())
        });
        let valid_subtags = segments.all(|segment| {
            !segment.is_empty() && segment.bytes().all(|byte| byte.is_ascii_alphanumeric())
        });
        if !valid_primary || !valid_subtags {
            return Err(LanguageCodeError::Malformed(value.to_owned()));
        }
        Ok(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LanguageCodeError {
    #[error("recognition language code must not be empty")]
    Empty,

    #[error(
        "recognition language code contains {actual_bytes} bytes; maximum is {maximum_bytes} bytes"
    )]
    TooLong {
        actual_bytes: usize,
        maximum_bytes: usize,
    },

    #[error("recognition language code is malformed: {0:?}")]
    Malformed(String),
}

#[derive(Clone, PartialEq, Eq)]
pub struct Transcript(String);

impl Transcript {
    pub fn new(text: impl Into<String>) -> Result<Self, TranscriptionError> {
        let text = text.into();
        let text = text.trim();
        if text.is_empty() {
            return Err(TranscriptionError::NoSpeech);
        }
        Ok(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_text(self) -> String {
        self.0
    }
}

impl std::fmt::Debug for Transcript {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Transcript")
            .field("bytes", &self.0.len())
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TranscriptionError {
    #[error("the {backend} model cannot be used")]
    Model {
        backend: &'static str,
        #[source]
        source: ModelError,
    },

    #[error("failed to load the {backend} model {model}")]
    Load {
        backend: &'static str,
        model: String,
        #[source]
        source: BackendError,
    },

    #[error("the recording being transcribed failed")]
    Capture(#[source] CaptureError),

    #[error("{backend} inference failed")]
    Inference {
        backend: &'static str,
        #[source]
        source: BackendError,
    },

    #[error("{backend} cannot transcribe {language:?}")]
    UnsupportedLanguage {
        backend: &'static str,
        language: String,
    },

    #[error("{backend} needs a UTF-8 recording path; got {}", path.display())]
    UnsupportedAudioPath {
        backend: &'static str,
        path: PathBuf,
    },

    #[error("{backend} requires {required}, but received {received}")]
    UnsupportedAudioInput {
        backend: &'static str,
        received: AudioInputKind,
        required: AudioInputKind,
    },

    #[error("the {backend} backend is not available on {}", std::env::consts::OS)]
    UnsupportedBackend { backend: &'static str },

    #[error("the {backend} backend has no gpu support on {}", std::env::consts::OS)]
    UnsupportedDevice { backend: &'static str },

    #[error("the engine was asked to stream natively but offers no stream")]
    NoNativeStream,

    #[error("the model loader stopped unexpectedly")]
    LoadPanicked,

    #[error("transcription was cancelled")]
    Cancelled,

    #[error("no speech was transcribed")]
    NoSpeech,

    #[error("the transcriber was asked to run an operation but offers none")]
    NoDelegatedOperation,

    #[error("transcript segment has empty text or a reversed audio span")]
    InvalidSegment,

    #[error("transcript processing failed")]
    Processing(#[source] layer::Error),
}

/// The shared model validators, answering in this capability's vocabulary.
#[cfg(not(target_family = "wasm"))]
pub fn validate_model_file(
    path: PathBuf,
    backend: &'static str,
) -> Result<PathBuf, TranscriptionError> {
    crate::backend::validate_model_file(path)
        .map_err(|source| TranscriptionError::Model { backend, source })
}

#[cfg(not(target_family = "wasm"))]
pub fn validate_model_directory(
    path: PathBuf,
    backend: &'static str,
    required: &[&'static str],
) -> Result<PathBuf, TranscriptionError> {
    crate::backend::validate_model_directory(path, required)
        .map_err(|source| TranscriptionError::Model { backend, source })
}

// Accept automatic detection and English, regional variants included; reject
// other explicit languages.
pub fn english_only(backend: &'static str, language: &Language) -> Result<(), TranscriptionError> {
    let Language::Specific(code) = language else {
        return Ok(());
    };
    if code.as_str().split('-').next() == Some(LanguageCode::english().as_str()) {
        return Ok(());
    }
    Err(TranscriptionError::UnsupportedLanguage {
        backend,
        language: code.as_str().to_owned(),
    })
}

#[cfg(test)]
mod operation_source_tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    struct Delegated {
        openings: Arc<Mutex<Vec<bool>>>,
    }

    impl Transcriber for Delegated {
        fn validate_language(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            _: TranscriptionAudio<'_>,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            panic!("the delegated operation owns decoding")
        }

        fn open_operation(
            &self,
            _: &Language,
            incremental: bool,
            _: &CancelToken,
        ) -> Result<Option<OperationSource>, TranscriptionError> {
            self.openings.lock().unwrap().push(incremental);
            let mut steps = VecDeque::from([
                OperationStep::Update {
                    update: TranscriptUpdate::Provisional("hello".into()),
                    retained_from: 0,
                    read_to: 20,
                    completed: None,
                },
                OperationStep::Update {
                    update: TranscriptUpdate::Finalized(TranscriptSegment::new(
                        "hello world",
                        Some(10..30),
                    )?),
                    retained_from: 10,
                    read_to: 30,
                    completed: Some(Transcript::new("hello world")?),
                },
                OperationStep::Complete {
                    transcript: Transcript::new("hello world")?,
                    retained_from: 30,
                    read_to: 40,
                },
            ]);
            Ok(Some(OperationSource::new(move |recording, _| {
                recording.fill().map_err(TranscriptionError::Capture)?;
                Ok(steps.pop_front().expect("completion stops polling"))
            })))
        }
    }

    #[test]
    fn delegated_updates_preserve_completion_and_audio_spans() {
        let openings = Arc::new(Mutex::new(Vec::new()));
        let transcriber: Arc<dyn Transcriber> = Arc::new(Delegated {
            openings: Arc::clone(&openings),
        });
        let transcription = Transcription::<_, Streaming>::delegated(transcriber);
        let mut recording = RecordingReader::from_samples(&[0.5; 40]);
        let mut operation = transcription.start(&Language::Automatic).unwrap();

        assert!(matches!(
            operation.next_update(&mut recording).unwrap(),
            Some(TranscriptUpdate::Provisional(text)) if text == "hello"
        ));
        assert!(operation.completed().is_none());
        assert_eq!(recording.held_to(), 40);
        assert_eq!(operation.read_to(), 20);
        let Some(TranscriptUpdate::Finalized(segment)) =
            operation.next_update(&mut recording).unwrap()
        else {
            panic!("the next update is finalized");
        };
        assert_eq!(segment.span(), Some(10..30));
        assert_eq!(operation.retained_from(), 10);
        assert_eq!(operation.read_to(), 30);
        assert_eq!(operation.completed().unwrap().as_str(), "hello world");
        assert!(operation.next_update(&mut recording).unwrap().is_none());
        assert_eq!(operation.retained_from(), 30);
        assert_eq!(operation.read_to(), 40);
        assert_eq!(
            operation.finish(&mut recording).unwrap().as_str(),
            "hello world"
        );
        assert_eq!(*openings.lock().unwrap(), [true]);
    }

    #[test]
    fn boxed_whole_operations_skip_previews_and_open_once() {
        let openings = Arc::new(Mutex::new(Vec::new()));
        let transcriber: Box<dyn Transcriber> = Box::new(Delegated {
            openings: Arc::clone(&openings),
        });
        let transcription = Transcription::<_, Whole>::delegated(transcriber);
        let mut recording = RecordingReader::from_samples(&[0.5; 40]);
        let mut operation = transcription.start(&Language::Automatic).unwrap();

        assert!(matches!(
            operation.next(&mut recording, false).unwrap(),
            Some(TranscriptUpdate::Finalized(_))
        ));
        assert_eq!(
            operation.finish(&mut recording).unwrap().as_str(),
            "hello world"
        );
        assert_eq!(*openings.lock().unwrap(), [false]);
    }

    struct Local;

    impl Transcriber for Local {
        fn validate_language(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            _: TranscriptionAudio<'_>,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            Ok(Hypothesis::new("local words"))
        }
    }

    #[test]
    fn only_a_delegated_plan_runs_the_transcribers_operation() {
        let openings = Arc::new(Mutex::new(Vec::new()));
        let delegating = Delegated {
            openings: Arc::clone(&openings),
        };
        assert!(
            Transcription::agreement(Cadence::from_millis(10, 20), delegating)
                .start(&Language::Automatic)
                .is_ok()
        );
        assert!(openings.lock().unwrap().is_empty());
        assert!(matches!(
            Transcription::<_, Whole>::delegated(Local).start(&Language::Automatic),
            Err(TranscriptionError::NoDelegatedOperation)
        ));
    }

    #[test]
    fn cancellation_during_a_delegated_step_discards_its_result() {
        let cancel = CancelToken::new();
        let trigger = cancel.clone();
        let mut source = OperationSource::new(move |_, _| {
            trigger.cancel();
            Ok(OperationStep::Complete {
                transcript: Transcript::new("must not escape")?,
                retained_from: 0,
                read_to: 0,
            })
        });
        let mut recording = RecordingReader::from_samples(&[]);

        assert!(matches!(
            source.next(&mut recording, true, &cancel),
            Err(TranscriptionError::Cancelled)
        ));
    }

    #[test]
    fn external_segments_reject_empty_text_and_reversed_spans() {
        assert!(TranscriptSegment::new(" ", None).is_err());
        let reversed = std::ops::Range { start: 30, end: 10 };
        assert!(TranscriptSegment::new("hello", Some(reversed)).is_err());
        let segment = TranscriptSegment::new(" hello ", Some(10..10)).unwrap();
        assert_eq!(segment.as_str(), "hello");
        assert_eq!(segment.span(), Some(10..10));
    }
}

#[cfg(test)]
mod word_timing_tests {
    use super::*;

    fn token(text: &str, start: f32, end: f32) -> (&str, TimedWord) {
        (text, TimedWord { start, end })
    }

    #[test]
    fn tokens_group_into_words_at_the_sentencepiece_marker() {
        let tokens = [
            token("\u{2581}hel", 0.0, 0.2),
            token("lo", 0.2, 0.4),
            token(",", 0.4, 0.5),
            token("\u{2581}world", 0.6, 0.9),
        ];
        let words = word_timings(tokens);
        assert_eq!(words.len(), 2);
        assert_eq!((words[0].start, words[0].end), (0.0, 0.5));
        assert_eq!((words[1].start, words[1].end), (0.6, 0.9));
    }

    #[test]
    fn tokens_group_into_words_at_a_leading_space() {
        let tokens = [
            token(" hel", 0.0, 0.2),
            token("lo", 0.2, 0.4),
            token(",", 0.4, 0.5),
            token(" world", 0.6, 0.9),
        ];
        let words = word_timings(tokens);
        assert_eq!(words.len(), 2);
        assert_eq!((words[0].start, words[0].end), (0.0, 0.5));
        assert_eq!((words[1].start, words[1].end), (0.6, 0.9));
    }

    #[test]
    fn word_timings_count_what_the_text_splits_into() {
        // A bare marker, a digit run, and a token holding two words.
        let tokens = [
            token("\u{2581}", 0.0, 0.1),
            token("\u{2581}1", 0.1, 0.2),
            token("8", 0.2, 0.3),
            token("9", 0.3, 0.4),
            token("\u{2581}years old", 0.4, 0.8),
            token(".", 0.8, 0.9),
        ];
        let text: String = tokens
            .iter()
            .map(|(text, _)| text.replace('\u{2581}', " "))
            .collect();
        let words = word_timings(tokens);
        assert_eq!(words.len(), text.split_whitespace().count());
        assert_eq!((words[0].start, words[0].end), (0.1, 0.4));
        assert_eq!((words[2].start, words[2].end), (0.4, 0.9));
    }

    #[test]
    fn a_word_timing_rounds_to_the_nearest_sample() {
        // 0.10003 s is 1600.48 samples and 0.20004 s is 3200.64.
        let word = TimedWord {
            start: 0.100_03,
            end: 0.200_04,
        };

        assert_eq!(word.span_from(1_000), 2_600..4_201);
    }
}

#[cfg(test)]
mod language_tests {
    use super::*;

    const BACKEND: &str = "test-backend";

    #[test]
    fn detection_and_english_are_accepted() {
        assert!(english_only(BACKEND, &Language::Automatic).is_ok());
        assert!(english_only(BACKEND, &Language::Specific(LanguageCode::english())).is_ok());

        let regional = LanguageCode::new("en-GB").expect("en-GB is a valid code");
        assert!(english_only(BACKEND, &Language::Specific(regional)).is_ok());
    }

    #[test]
    fn language_parses_what_as_str_writes() {
        for language in [
            Language::Automatic,
            Language::Specific(LanguageCode::new("en-GB").expect("en-GB is a valid code")),
        ] {
            assert_eq!(language.as_str().parse::<Language>().ok(), Some(language));
        }
        assert_eq!(" AUTO".parse::<Language>().ok(), Some(Language::Automatic));
        assert!(matches!(
            "en_GB".parse::<Language>(),
            Err(LanguageCodeError::Malformed(_))
        ));
    }

    #[test]
    fn another_language_is_refused_by_name() {
        let code = LanguageCode::new("de").expect("de is a valid code");

        let error = english_only(BACKEND, &Language::Specific(code))
            .expect_err("another language must be refused");

        assert!(matches!(
            error,
            TranscriptionError::UnsupportedLanguage { language, .. } if language == "de"
        ));
    }
}

#[cfg(test)]
mod coordinator_tests {
    use std::sync::{Arc, Mutex};

    use crate::audio::testing::recorded;

    use super::*;

    struct FakeTranscriber {
        response: &'static str,
        reject_language: bool,
        inputs: Mutex<Vec<AudioInputKind>>,
    }

    impl FakeTranscriber {
        fn answering(response: &'static str) -> Self {
            Self {
                response,
                reject_language: false,
                inputs: Mutex::new(Vec::new()),
            }
        }

        fn inputs(&self) -> Vec<AudioInputKind> {
            self.inputs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl Transcriber for FakeTranscriber {
        fn validate_language(
            &self,
            language: &Language,
            _cancel: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            if self.reject_language {
                return Err(TranscriptionError::UnsupportedLanguage {
                    backend: "fake",
                    language: language.as_str().to_owned(),
                });
            }
            Ok(())
        }

        fn transcribe_with(
            &self,
            audio: TranscriptionAudio<'_>,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            self.inputs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(audio.kind());
            Ok(Hypothesis::new(self.response))
        }
    }

    /// An engine that streams natively: every chunk answers "hello".
    struct Chunked;

    struct HelloStream;

    impl ChunkStream for HelloStream {
        fn chunk_samples(&self) -> usize {
            2
        }

        fn feed(&mut self, chunk: &[f32]) -> Result<Emitted, TranscriptionError> {
            Ok(Emitted {
                words: Hypothesis::new(match chunk.iter().any(|sample| *sample != 0.0) {
                    true => "hello",
                    false => "",
                }),
                joins_previous: false,
            })
        }
    }

    impl Transcriber for Chunked {
        fn validate_language(
            &self,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            Ok(())
        }

        fn transcribe_with(
            &self,
            _audio: TranscriptionAudio<'_>,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            unreachable!("a native stream never decodes a bounded input")
        }

        fn open_stream(
            &self,
            _language: &Language,
            _cancel: &CancelToken,
        ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
            Ok(Some(Box::new(HelloStream)))
        }
    }

    #[test]
    fn a_boxed_or_shared_transcriber_still_opens_its_stream() {
        let (_directory, mut boxed_recording) = recorded(&[1, 2, 3, 4]);
        let (_directory, mut shared_recording) = recorded(&[1, 2, 3, 4]);
        let boxed: Box<dyn Transcriber> = Box::new(Chunked);
        let shared = Arc::new(Chunked);

        let from_boxed = Transcription::stream(Stream::Native, boxed)
            .transcribe(&mut boxed_recording, &Language::Automatic)
            .expect("the boxed stream opens");
        let from_shared = Transcription::stream(Stream::Native, shared)
            .transcribe(&mut shared_recording, &Language::Automatic)
            .expect("the shared stream opens");

        assert_eq!(from_boxed.as_str(), "hello hello");
        assert_eq!(from_shared.as_str(), "hello hello");
    }

    #[test]
    fn completion_is_available_before_pending_final_segments_and_preview_clear() {
        struct Ending(std::sync::atomic::AtomicBool);
        impl Transcriber for Ending {
            fn validate_language(
                &self,
                _: &Language,
                _: &CancelToken,
            ) -> Result<(), TranscriptionError> {
                Ok(())
            }
            fn transcribe_with(
                &self,
                _: TranscriptionAudio<'_>,
                _: &Language,
                _: &CancelToken,
            ) -> Result<Hypothesis, TranscriptionError> {
                Ok(Hypothesis::new(
                    if self.0.swap(true, std::sync::atomic::Ordering::Relaxed) {
                        "one two three"
                    } else {
                        "one two"
                    },
                ))
            }
        }
        let (mut recording, mut capture) = crate::audio::Recording::open();
        let transcription = Transcription::agreement(
            Cadence::from_millis(100, 20_000),
            Ending(std::sync::atomic::AtomicBool::new(false)),
        );
        let mut operation = transcription.start(&Language::Automatic).expect("starts");
        capture.push(&[0.5; 1600]);
        assert!(
            matches!(operation.next_update(&mut recording).expect("preview"), Some(TranscriptUpdate::Provisional(text)) if text == "one two")
        );
        capture.push(&[0.5; 1600]);
        capture.finish();
        recording.fill().expect("capture ended");
        assert!(
            matches!(operation.next_update(&mut recording).expect("final"), Some(TranscriptUpdate::Finalized(segment)) if segment.as_str() == "one two")
        );
        assert_eq!(
            operation.completed().expect("already complete").as_str(),
            "one two three"
        );
        assert!(
            matches!(operation.next_update(&mut recording).expect("tail"), Some(TranscriptUpdate::Finalized(segment)) if segment.as_str() == "three")
        );
        assert!(
            matches!(operation.next_update(&mut recording).expect("clear"), Some(TranscriptUpdate::Provisional(text)) if text.is_empty())
        );
        assert!(
            operation
                .next_update(&mut recording)
                .expect("ended")
                .is_none()
        );
    }

    #[test]
    fn native_and_whole_updates_are_finalized_only() {
        fn check<T: Transcriber, M: Mode>(transcription: Transcription<T, M>, expected: &str) {
            let (_directory, mut recording) = recorded(&[1, 2, 3, 4]);
            let mut operation = transcription.start(&Language::Automatic).expect("starts");
            let mut segments = Vec::new();
            while let Some(update) = operation.next_update(&mut recording).expect("reads") {
                let TranscriptUpdate::Finalized(segment) = update else {
                    panic!("only agreement exposes provisional words");
                };
                segments.push(segment.into_text());
            }
            assert_eq!(segments.join(" "), expected);
            assert_eq!(operation.completed().expect("completed").as_str(), expected);
            assert_eq!(
                operation.finish(&mut recording).expect("finished").as_str(),
                expected
            );
        }
        check(
            Transcription::stream(Stream::Native, Chunked),
            "hello hello",
        );
        check(
            Transcription::complete_samples(FakeTranscriber::answering("hello there")),
            "hello there",
        );
    }

    #[test]
    fn streaming_natively_without_a_stream_is_an_error() {
        let transcription =
            Transcription::stream(Stream::Native, FakeTranscriber::answering("hello"));

        let outcome = transcription.start(&Language::Automatic).map(|_| ());

        assert!(matches!(outcome, Err(TranscriptionError::NoNativeStream)));
    }

    #[test]
    fn complete_samples_invokes_the_engine_once() {
        let (_directory, mut recording) = recorded(&[1, 2, 3]);
        let transcriber = Arc::new(FakeTranscriber::answering("hello there"));
        let transcription = Transcription::complete_samples(Arc::clone(&transcriber));

        let transcript = transcription
            .transcribe(&mut recording, &Language::Automatic)
            .expect("the fake returns speech");

        assert_eq!(transcript.as_str(), "hello there");
        assert_eq!(transcriber.inputs(), [AudioInputKind::Samples]);
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn complete_wav_preserves_the_file_input_boundary() {
        let (_directory, mut recording) = recorded(&[1, 2, 3]);
        let transcriber = Arc::new(FakeTranscriber::answering("hello there"));
        let transcription = Transcription::complete_wav(Arc::clone(&transcriber));

        transcription
            .transcribe(&mut recording, &Language::Automatic)
            .expect("the fake returns speech");

        assert_eq!(transcriber.inputs(), [AudioInputKind::Wav]);
        assert!(recording.samples().is_empty());
    }

    #[test]
    fn an_empty_hypothesis_becomes_terminal_no_speech() {
        let (_directory, mut recording) = recorded(&[1, 2, 3]);
        let transcription = Transcription::complete_samples(FakeTranscriber::answering("   "));

        let outcome = transcription.transcribe(&mut recording, &Language::Automatic);

        assert!(matches!(outcome, Err(TranscriptionError::NoSpeech)));
    }

    #[test]
    fn cancellation_prevents_complete_inference() {
        let (_directory, mut recording) = recorded(&[1, 2, 3]);
        let transcriber = Arc::new(FakeTranscriber::answering("unreachable"));
        let transcription = Transcription::complete_samples(Arc::clone(&transcriber));
        let cancel = CancelToken::new();
        cancel.cancel();

        let outcome = transcription.transcribe_with(&mut recording, &Language::Automatic, &cancel);

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
        assert!(transcriber.inputs().is_empty());
    }

    #[test]
    fn language_rejection_prevents_complete_inference() {
        let (_directory, mut recording) = recorded(&[1, 2, 3]);
        let mut fake = FakeTranscriber::answering("unreachable");
        fake.reject_language = true;
        let transcriber = Arc::new(fake);
        let transcription = Transcription::complete_samples(Arc::clone(&transcriber));

        let outcome = transcription.transcribe(&mut recording, &Language::Automatic);

        assert!(matches!(
            outcome,
            Err(TranscriptionError::UnsupportedLanguage { .. })
        ));
        assert!(transcriber.inputs().is_empty());
    }

    #[test]
    fn sensitive_transcription_types_redact_their_text() {
        let secret = "words that must not reach diagnostics";
        let hypothesis = Hypothesis::new(secret);
        let transcript = Transcript::new(secret).expect("the text is not empty");
        let segment =
            TranscriptSegment::spanned(&[secret.to_owned()], None).expect("the text is not empty");

        assert!(!format!("{hypothesis:?}").contains(secret));
        assert!(!format!("{transcript:?}").contains(secret));
        assert!(!format!("{segment:?}").contains(secret));
    }
}
