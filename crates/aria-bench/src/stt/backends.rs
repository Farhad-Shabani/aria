//! Which engines a run measures, and how each is named in the results.
//!
//! A loaded backend keeps no record of where it came from — the model path and
//! device are consumed at load and dropped. Identity therefore comes from what
//! a backend was asked to be, and is captured here before the model is ever
//! loaded.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Instant;

use aria::config::{TranscriptionConfig, TranscriptionDevice, backend};
use aria::transcription::{
    Cadence, ChunkStream, Emitted, Hypothesis, Language, Stream, Streaming, Transcriber,
    Transcription, TranscriptionAudio, TranscriptionError, Whole, check_model,
};
use aria_kit::audio::RecordingReader;
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{Transcript, TranscriptUpdate};
use aria_stt::{EngineConfig, moonshine, nemotron, parakeet, parakeet_mlx, whisper};

use super::Mode;
use super::conditions::{Conditions, Decoding};
use super::record::{Descriptor, Preview, Segment};

/// One engine to measure, named with everything that engine needs.
pub enum BackendRequest {
    Whisper {
        model: PathBuf,
    },
    WhisperCandle {
        model: PathBuf,
    },
    Parakeet {
        model: PathBuf,
        gpu: bool,
    },
    Moonshine {
        model: PathBuf,
    },
    Nemotron {
        model: PathBuf,
    },
    ParakeetMlx {
        /// A repository id the sidecar resolves against its own model cache.
        model: String,
        interpreter: PathBuf,
    },
}

impl BackendRequest {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Whisper { .. } => backend::WHISPER,
            Self::WhisperCandle { .. } => backend::WHISPER_CANDLE,
            Self::Parakeet { .. } => backend::PARAKEET,
            Self::Moonshine { .. } => backend::MOONSHINE,
            Self::Nemotron { .. } => backend::NEMOTRON,
            Self::ParakeetMlx { .. } => backend::PARAKEET_MLX,
        }
    }

    // Whisper, parakeet and moonshine stream by agreement; parakeet-mlx
    // streams natively inside its sidecar. Whisper-candle decodes whole,
    // and nemotron streams at its own chunk, which no cadence overrides.
    fn accepts_cadence(&self) -> bool {
        matches!(
            self,
            Self::Whisper { .. }
                | Self::Parakeet { .. }
                | Self::Moonshine { .. }
                | Self::ParakeetMlx { .. }
        )
    }
}

/// A backend this machine cannot measure, and why.
#[derive(Debug)]
pub struct Skipped {
    pub backend: &'static str,
    /// The top-level reason only; chains can name paths, so they go to the
    /// log instead.
    pub reason: String,
}

/// Defers loading so a benchmark can keep one model resident at a time.
pub(super) struct Selection {
    pub(super) descriptor: Descriptor,
    config: TranscriptionConfig,
    cadence: Option<Cadence>,
}

impl Selection {
    pub(super) fn load(&self) -> Result<Loaded, TranscriptionError> {
        let timing = Timing::default();
        let config = EngineConfig::try_from(self.config.clone())?;
        let input = config.input();
        let engine = Timed {
            inner: load_transcriber(config, self.cadence)?,
            timing: timing.clone(),
        };
        let decoding = self.decoding();
        let transcription = match decoding {
            Decoding::ChunkedFile { .. } => Composed::Whole(Transcription::complete_wav(engine)),
            Decoding::Native { .. } => {
                Composed::Streaming(Transcription::stream(Stream::Native, engine))
            }
            Decoding::Agreement { .. } => Composed::Streaming(Transcription::agreement(
                self.cadence.expect("agreement cadence"),
                engine,
            )),
            Decoding::Whole => Composed::Whole(Transcription::complete(input, engine)),
        };
        Ok(Loaded {
            transcription,
            timing,
            decoding,
        })
    }
}

/// A loaded selection: the transcription to drive, where it reports
/// decode-busy time, and how its audio must arrive.
pub(super) struct Loaded {
    transcription: Composed,
    timing: Timing,
    pub(super) decoding: Decoding,
}

impl Loaded {
    pub(super) fn attempt(&mut self, advance: Option<mpsc::Sender<()>>) -> Attempt<'_> {
        self.timing.decode_busy.store(0, Ordering::Relaxed);
        self.timing.decode_calls.store(0, Ordering::Relaxed);
        *self
            .timing
            .step
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = advance;
        Attempt {
            transcription: &self.transcription,
            timing: &self.timing,
        }
    }
}

pub(super) struct Attempt<'a> {
    pub(super) transcription: &'a Composed,
    timing: &'a Timing,
}

impl Attempt<'_> {
    pub(super) fn decode_ms(&self) -> u128 {
        u128::from(self.timing.decode_busy.load(Ordering::Relaxed)) / 1000
    }

    pub(super) fn decode_calls(&self) -> u64 {
        self.timing.decode_calls.load(Ordering::Relaxed)
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        // Closing the channel releases a feeder still waiting for its next stride.
        *self
            .timing
            .step
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

/// The bench picks a backend's mode from its inputs at run time.
pub(super) enum Composed {
    Streaming(Transcription<Timed, Streaming>),
    Whole(Transcription<Timed, Whole>),
}

impl Composed {
    pub(super) fn transcribe(
        &self,
        recording: &mut RecordingReader,
        language: &Language,
        started: Instant,
        segments: &mut Vec<Segment>,
        mut previews: Option<&mut Vec<Preview>>,
    ) -> Result<Transcript, TranscriptionError> {
        match self {
            Self::Streaming(transcription) => {
                let mut operation = transcription.start(language)?;
                loop {
                    let segment = match operation.next(recording, previews.is_some())? {
                        Some(TranscriptUpdate::Finalized(segment)) => segment,
                        Some(TranscriptUpdate::Provisional(text)) => {
                            previews
                                .as_deref_mut()
                                .expect("previews enabled")
                                .push(Preview {
                                    text,
                                    emitted_ms: started.elapsed().as_millis(),
                                    available_samples: recording.held_to(),
                                    finalized_segments: segments.len(),
                                });
                            continue;
                        }
                        None => break,
                    };
                    let emitted_ms = started.elapsed().as_millis();
                    segments.push(Segment {
                        span_samples: segment.span(),
                        text: segment.into_text(),
                        emitted_ms,
                        available_samples: recording.held_to(),
                    });
                }
                operation.finish(recording)
            }
            Self::Whole(transcription) => transcription.transcribe(recording, language),
        }
    }
}

/// Times every decode an engine runs, so a streamed run can report how much
/// wall clock was computation rather than waiting for audio to arrive. Under
/// stepped replay it is also the clock: each finished decode releases the
/// next stride of audio.
pub(super) struct Timed {
    inner: Box<dyn Transcriber>,
    timing: Timing,
}

impl Transcriber for Timed {
    fn validate_language(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        self.inner.validate_language(language, cancel)
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        let started = Instant::now();
        let heard = self.inner.transcribe_with(audio, language, cancel);
        self.timing.decoded(started);
        heard
    }

    fn open_stream(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
        let Some(inner) = self.inner.open_stream(language, cancel)? else {
            return Ok(None);
        };
        Ok(Some(Box::new(TimedStream {
            inner,
            timing: self.timing.clone(),
        })))
    }
}

/// What every decode reports: its busy time, and the permit that releases
/// the next stride of a stepped replay.
#[derive(Clone, Default)]
struct Timing {
    decode_busy: Arc<AtomicU64>,
    decode_calls: Arc<AtomicU64>,
    step: Arc<Mutex<Option<mpsc::Sender<()>>>>,
}

impl Timing {
    fn decoded(&self, started: Instant) {
        self.decode_calls.fetch_add(1, Ordering::Relaxed);
        self.decode_busy
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        // The finishing decode can outlive its utterance's feeder; a send
        // into a dropped receiver is the expected end of the run.
        let step = self
            .step
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(step) = step.as_ref() {
            let _ = step.send(());
        }
    }
}

/// A native stream timed the same way: every chunk fed is one decode.
struct TimedStream {
    inner: Box<dyn ChunkStream>,
    timing: Timing,
}

impl ChunkStream for TimedStream {
    fn chunk_samples(&self) -> usize {
        self.inner.chunk_samples()
    }

    fn feed(&mut self, chunk: &[f32]) -> Result<Emitted, TranscriptionError> {
        let started = Instant::now();
        let heard = self.inner.feed(chunk);
        self.timing.decoded(started);
        heard
    }
}

fn load_transcriber(
    config: EngineConfig,
    cadence: Option<Cadence>,
) -> Result<Box<dyn Transcriber>, TranscriptionError> {
    // With a cadence the sidecar streams natively. `EngineConfig` has no
    // option for that, so this loads the engine directly.
    if let (EngineConfig::ParakeetMlx(config), Some(cadence)) = (&config, cadence) {
        let engine =
            parakeet_mlx::ParakeetMlx::load(config.interpreter.clone(), config.model.clone())?
                .stream_chunks(cadence.stride_samples());
        return Ok(Box::new(engine));
    }
    config.load(&CancelToken::never())
}

pub(super) fn selections(
    requested: &[BackendRequest],
    cadence: Option<Cadence>,
    mode: Option<Mode>,
    realtime: bool,
) -> Result<(Vec<Selection>, Vec<Skipped>), SelectionError> {
    if requested.is_empty() {
        return Err(SelectionError::NoBackend);
    }
    let refuses = requested.iter().find(|request| !request.accepts_cadence());
    if let (Some(request), Some(_)) = (refuses, cadence) {
        return Err(SelectionError::NotAgreement {
            backend: request.name(),
        });
    }

    let mut names = std::collections::BTreeSet::new();
    let mut selected = Vec::new();
    let mut skipped = Vec::new();
    for request in requested {
        if !names.insert(request.name()) {
            return Err(SelectionError::Invalid(format!(
                "duplicate backend {}",
                request.name()
            )));
        }
        let effective = effective_cadence(request, mode, cadence, realtime)?;
        let selection = Selection::from_request(request, effective);
        match check_model(&selection.config) {
            Ok(_) => selected.push(selection),
            Err(failure) => {
                let detail = aria::primitives::error_chain(&failure);
                tracing::warn!(%detail, "a transcription backend cannot be used");
                skipped.push(Skipped {
                    backend: request.name(),
                    reason: failure.to_string(),
                });
            }
        }
    }
    if !skipped.is_empty() {
        return Err(SelectionError::AllSkipped { skipped });
    }
    Ok((selected, skipped))
}

fn effective_cadence(
    request: &BackendRequest,
    mode: Option<Mode>,
    cadence: Option<Cadence>,
    realtime: bool,
) -> Result<Option<Cadence>, SelectionError> {
    let invalid = |reason: &str| SelectionError::Invalid(format!("{}: {reason}", request.name()));
    if mode == Some(Mode::Whole) && cadence.is_some() {
        return Err(invalid("--mode whole conflicts with --cadence"));
    }
    if matches!(request, BackendRequest::Nemotron { .. }) {
        if mode == Some(Mode::Whole) {
            return Err(invalid("supports native streaming only"));
        }
        return Ok(None);
    }
    let streaming = mode == Some(Mode::Streaming) || cadence.is_some();
    if realtime && (!streaming || matches!(request, BackendRequest::ParakeetMlx { .. })) {
        return Err(invalid(
            "--realtime requires a host streaming engine; use --mode streaming",
        ));
    }
    if !streaming {
        return Ok(None);
    }
    if let Some(cadence) = cadence {
        return Ok(Some(cadence));
    }
    Ok(Some(match request {
        BackendRequest::Whisper { .. } => whisper::CADENCE,
        BackendRequest::Parakeet { .. } => parakeet::CADENCE,
        BackendRequest::Moonshine { .. } => moonshine::CADENCE,
        BackendRequest::ParakeetMlx { .. } => {
            return Err(invalid("streaming requires an explicit --cadence"));
        }
        BackendRequest::WhisperCandle { .. } => return Err(invalid("does not support streaming")),
        BackendRequest::Nemotron { .. } => unreachable!(),
    }))
}

impl Selection {
    fn decoding(&self) -> Decoding {
        match (self.cadence, &self.config) {
            (Some(cadence), TranscriptionConfig::ParakeetMlx { .. }) => Decoding::ChunkedFile {
                chunk_samples: cadence.stride_samples(),
            },
            (_, TranscriptionConfig::Nemotron { .. }) => Decoding::Native {
                chunk_samples: nemotron::CHUNK_SAMPLES,
            },
            (Some(cadence), _) => Decoding::agreement(cadence),
            (None, _) => Decoding::Whole,
        }
    }

    pub(super) fn identity(
        &self,
        realtime: bool,
        provisional: bool,
    ) -> Result<super::analysis::Engine, aria_harness::artifact::ArtifactError> {
        let model = match &self.config {
            TranscriptionConfig::Whisper { model } => Some(model),
            TranscriptionConfig::WhisperCandle { model_directory }
            | TranscriptionConfig::Parakeet {
                model_directory, ..
            }
            | TranscriptionConfig::Moonshine { model_directory }
            | TranscriptionConfig::Nemotron { model_directory } => Some(model_directory),
            TranscriptionConfig::ParakeetMlx { .. } => None,
        };
        let model_sha256 = model
            .map(|model| aria_harness::artifact::fingerprint(model))
            .transpose()?;
        Ok(super::analysis::Engine {
            name: self.descriptor.name.to_owned(),
            model_sha256,
            device: self.descriptor.device.map(str::to_owned),
            conditions: Conditions::new(self.decoding(), realtime, provisional),
        })
    }
}

impl Selection {
    fn from_request(requested: &BackendRequest, cadence: Option<Cadence>) -> Self {
        match requested {
            BackendRequest::Whisper { model } => Self {
                descriptor: Descriptor {
                    name: backend::WHISPER,
                    model: model_label(model),
                    device: Some(whisper::gpu_backend().unwrap_or("cpu")),
                    model_bytes: model_bytes(model),
                },
                config: TranscriptionConfig::Whisper {
                    model: model.clone(),
                },
                cadence,
            },
            BackendRequest::WhisperCandle { model } => Self {
                descriptor: Descriptor {
                    name: backend::WHISPER_CANDLE,
                    model: model_label(model),
                    device: None,
                    model_bytes: model_bytes(model),
                },
                config: TranscriptionConfig::WhisperCandle {
                    model_directory: model.clone(),
                },
                cadence: None,
            },
            BackendRequest::Parakeet { model, gpu } => {
                let device = match gpu {
                    true => TranscriptionDevice::Gpu,
                    false => TranscriptionDevice::Cpu,
                };
                Self {
                    descriptor: Descriptor {
                        name: backend::PARAKEET,
                        model: model_label(model),
                        device: Some(device_name(device)),
                        model_bytes: model_bytes(model),
                    },
                    config: TranscriptionConfig::Parakeet {
                        model_directory: model.clone(),
                        device,
                    },
                    cadence,
                }
            }
            BackendRequest::Moonshine { model } => Self {
                descriptor: Descriptor {
                    name: backend::MOONSHINE,
                    model: model_label(model),
                    device: None,
                    model_bytes: model_bytes(model),
                },
                config: TranscriptionConfig::Moonshine {
                    model_directory: model.clone(),
                },
                cadence,
            },
            BackendRequest::Nemotron { model } => Self {
                descriptor: Descriptor {
                    name: backend::NEMOTRON,
                    model: model_label(model),
                    device: None,
                    model_bytes: model_bytes(model),
                },
                config: TranscriptionConfig::Nemotron {
                    model_directory: model.clone(),
                },
                cadence: None,
            },
            BackendRequest::ParakeetMlx { model, interpreter } => Self {
                descriptor: Descriptor {
                    name: backend::PARAKEET_MLX,
                    model: model.clone(),
                    device: None,
                    // The weights live in the sidecar's model cache, not at a
                    // path this side of the boundary knows.
                    model_bytes: None,
                },
                config: TranscriptionConfig::ParakeetMlx {
                    model: model.clone(),
                    interpreter: interpreter.clone(),
                },
                cadence,
            },
        }
    }
}

fn device_name(device: TranscriptionDevice) -> &'static str {
    match device {
        TranscriptionDevice::Cpu => "cpu",
        TranscriptionDevice::Gpu => "gpu",
    }
}

/// A full path names the machine, and `model_sha256` already identifies the
/// weights.
fn model_label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// Total size of a model on disk, whether it is one file or a directory of
/// them. An unreadable path reports nothing rather than failing the run: the
/// size is provenance for the report, not something the run depends on.
fn model_bytes(path: &Path) -> Option<u64> {
    let metadata = path.metadata().ok()?;
    if metadata.is_file() {
        return Some(metadata.len());
    }
    let mut total = 0;
    for entry in path.read_dir().ok()? {
        let metadata = entry.ok()?.metadata().ok()?;
        if metadata.is_file() {
            total += metadata.len();
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TraceEngine {
        fail_second: bool,
    }

    impl Transcriber for TraceEngine {
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
            Ok(Hypothesis::new("one two three four"))
        }

        fn open_stream(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
            Ok(Some(Box::new(TraceStream {
                feeds: 0,
                fail_second: self.fail_second,
            })))
        }
    }

    struct TraceStream {
        feeds: usize,
        fail_second: bool,
    }

    impl ChunkStream for TraceStream {
        fn chunk_samples(&self) -> usize {
            2
        }

        fn feed(&mut self, _: &[f32]) -> Result<Emitted, TranscriptionError> {
            self.feeds += 1;
            if self.fail_second && self.feeds == 2 {
                return Err(TranscriptionError::Cancelled);
            }
            Ok(Emitted {
                words: Hypothesis::new(match self.feeds {
                    1 => "one two",
                    2 => "three four",
                    _ => "",
                }),
                joins_previous: false,
            })
        }
    }

    fn trace_engine(fail_second: bool) -> Timed {
        Timed {
            inner: Box::new(TraceEngine { fail_second }),
            timing: Timing::default(),
        }
    }

    #[test]
    fn attempts_reset_metrics_and_disconnect_replay_on_failure_and_unwind() {
        let directory = tempfile::tempdir().unwrap();
        let engine = trace_engine(true);
        let mut loaded = Loaded {
            timing: engine.timing.clone(),
            transcription: Composed::Streaming(Transcription::stream(Stream::Native, engine)),
            decoding: Decoding::Native { chunk_samples: 2 },
        };

        for unwind in [false, true] {
            let mut recording =
                aria_kit::audio::RecordingReader::from_samples_in(directory.path(), &[0.5; 4])
                    .unwrap();
            let (advance, gate) = mpsc::channel();
            loaded.timing.decode_busy.store(9_000, Ordering::Relaxed);
            loaded.timing.decode_calls.store(7, Ordering::Relaxed);

            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let attempt = loaded.attempt(Some(advance));
                assert_eq!(attempt.decode_ms(), 0);
                assert_eq!(attempt.decode_calls(), 0);

                let result = attempt.transcription.transcribe(
                    &mut recording,
                    &Language::Automatic,
                    Instant::now(),
                    &mut Vec::new(),
                    None,
                );

                assert!(matches!(result, Err(TranscriptionError::Cancelled)));
                assert_eq!(attempt.decode_calls(), 2);
                assert!(!unwind, "test attempt unwinding");
            }));

            assert_eq!(outcome.is_err(), unwind);
            assert_eq!(gate.try_recv(), Ok(()));
            assert_eq!(gate.try_recv(), Ok(()));
            assert_eq!(gate.try_recv(), Err(mpsc::TryRecvError::Disconnected));
        }
    }

    #[test]
    fn streaming_trace_preserves_emission_order_and_audio_spans() {
        let directory = tempfile::tempdir().unwrap();
        let mut recording =
            aria_kit::audio::RecordingReader::from_samples_in(directory.path(), &[0.5; 4]).unwrap();
        let composed =
            Composed::Streaming(Transcription::stream(Stream::Native, trace_engine(false)));
        let started = Instant::now();
        let mut segments = Vec::new();

        let transcript = composed
            .transcribe(
                &mut recording,
                &Language::Automatic,
                started,
                &mut segments,
                None,
            )
            .unwrap();

        assert_eq!(transcript.as_str(), "one two three four");
        assert_eq!(
            segments
                .iter()
                .map(|segment| (
                    segment.text.as_str(),
                    segment.span_samples.clone(),
                    segment.available_samples
                ))
                .collect::<Vec<_>>(),
            vec![
                ("one", Some(0..2), 4),
                ("two three", Some(0..4), 4),
                ("four", Some(2..4), 4)
            ]
        );
        assert!(
            segments
                .windows(2)
                .all(|pair| pair[0].emitted_ms <= pair[1].emitted_ms)
        );
        assert!(segments.last().unwrap().emitted_ms <= started.elapsed().as_millis());
    }

    #[test]
    fn provisional_trace_records_order_without_extra_decodes_or_final_changes() {
        let run = |enabled: bool| {
            let directory = tempfile::tempdir().expect("tempdir");
            let fixture = aria_kit::audio::canonical_fixture(directory.path());
            let (advance, gate) = mpsc::channel();
            let mut recording = crate::stt::stepped_replay(&fixture, 1600, gate).expect("replays");
            let engine = trace_engine(false);
            let calls = Arc::clone(&engine.timing.decode_calls);
            *engine.timing.step.lock().expect("no poison") = Some(advance);
            let composed = Composed::Streaming(Transcription::agreement(
                Cadence::from_millis(100, 20_000),
                engine,
            ));
            let mut segments = Vec::new();
            let mut previews = Vec::new();
            let transcript = composed
                .transcribe(
                    &mut recording,
                    &Language::Automatic,
                    Instant::now(),
                    &mut segments,
                    enabled.then_some(&mut previews),
                )
                .expect("transcribes");
            let segments = segments
                .into_iter()
                .map(|segment| {
                    (
                        segment.text,
                        segment.span_samples,
                        segment.available_samples,
                    )
                })
                .collect::<Vec<_>>();
            (
                transcript.into_text(),
                segments,
                previews,
                calls.load(Ordering::Relaxed),
            )
        };
        let (baseline, baseline_segments, _, baseline_calls) = run(false);
        let (captioned, captioned_segments, previews, captioned_calls) = run(true);
        assert_eq!(captioned, baseline);
        assert_eq!(captioned_segments, baseline_segments);
        assert_eq!(captioned_calls, baseline_calls);
        assert_eq!(captioned_calls, 10);
        assert_eq!(
            previews
                .iter()
                .map(|preview| (
                    preview.text.as_str(),
                    preview.finalized_segments,
                    preview.available_samples
                ))
                .collect::<Vec<_>>(),
            [("one two three four", 0, 1600), ("", 1, 3200),]
        );
    }

    #[test]
    fn a_later_failure_keeps_the_already_emitted_trace() {
        let directory = tempfile::tempdir().unwrap();
        let mut recording =
            aria_kit::audio::RecordingReader::from_samples_in(directory.path(), &[0.5; 4]).unwrap();
        let composed =
            Composed::Streaming(Transcription::stream(Stream::Native, trace_engine(true)));
        let mut segments = Vec::new();

        let outcome = composed.transcribe(
            &mut recording,
            &Language::Automatic,
            Instant::now(),
            &mut segments,
            None,
        );

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].text, "one");
        assert_eq!(segments[0].span_samples, Some(0..2));
        assert_eq!(segments[0].available_samples, 4);
    }

    #[test]
    fn whole_transcription_has_no_streaming_trace() {
        let directory = tempfile::tempdir().unwrap();
        let mut recording =
            aria_kit::audio::RecordingReader::from_samples_in(directory.path(), &[0.5; 4]).unwrap();
        let composed = Composed::Whole(Transcription::complete_samples(trace_engine(false)));
        let mut segments = Vec::new();

        let transcript = composed
            .transcribe(
                &mut recording,
                &Language::Automatic,
                Instant::now(),
                &mut segments,
                None,
            )
            .unwrap();

        assert_eq!(transcript.as_str(), "one two three four");
        assert!(segments.is_empty());
    }

    #[test]
    fn mode_and_pace_resolve_before_loading_a_model() {
        let whisper = BackendRequest::Whisper {
            model: PathBuf::from("unused"),
        };
        assert!(
            effective_cadence(&whisper, None, None, false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            effective_cadence(&whisper, Some(Mode::Streaming), None, true)
                .unwrap()
                .unwrap()
                .stride_samples(),
            whisper::CADENCE.stride_samples()
        );
        assert!(effective_cadence(&whisper, None, None, true).is_err());
        assert!(
            effective_cadence(&whisper, Some(Mode::Whole), Some(whisper::CADENCE), false).is_err()
        );
        let native = BackendRequest::Nemotron {
            model: PathBuf::from("unused"),
        };
        assert!(effective_cadence(&native, None, None, true).is_ok());
        assert!(effective_cadence(&native, Some(Mode::Whole), None, false).is_err());
        let candle = BackendRequest::WhisperCandle {
            model: PathBuf::from("unused"),
        };
        assert!(effective_cadence(&candle, Some(Mode::Streaming), None, false).is_err());
    }

    #[test]
    fn a_missing_engine_rejects_even_when_another_is_available() {
        let model = tempfile::NamedTempFile::new().unwrap();
        let requested = [
            BackendRequest::Whisper {
                model: model.path().to_owned(),
            },
            BackendRequest::Moonshine {
                model: PathBuf::from("/nonexistent/model"),
            },
        ];
        let outcome = selections(&requested, None, None, false);
        assert!(
            matches!(outcome, Err(SelectionError::AllSkipped { ref skipped }) if skipped.len() == 1 && skipped[0].backend == backend::MOONSHINE)
        );
    }

    #[test]
    fn parakeet_mlx_accepts_a_cadence() {
        let requested = [BackendRequest::ParakeetMlx {
            model: "repo/id".to_owned(),
            interpreter: PathBuf::from("/nonexistent/python"),
        }];

        let outcome = selections(
            &requested,
            Some(Cadence::from_millis(750, 20000)),
            None,
            false,
        );

        // The model check sets it aside; a cadence alone must not.
        assert!(matches!(
            outcome,
            Err(SelectionError::AllSkipped { ref skipped }) if skipped.len() == 1
        ));
    }

    #[test]
    fn whisper_candle_refuses_a_cadence() {
        let requested = [BackendRequest::WhisperCandle {
            model: PathBuf::from("/nonexistent"),
        }];

        let outcome = selections(
            &requested,
            Some(Cadence::from_millis(750, 20000)),
            None,
            false,
        );

        assert!(matches!(
            outcome,
            Err(SelectionError::NotAgreement { backend }) if backend == backend::WHISPER_CANDLE
        ));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SelectionError {
    #[error("{0}")]
    Invalid(String),
    #[error("no backend was named; the run would measure nothing")]
    NoBackend,
    #[error("{backend} does not stream by agreement, so it has no cadence to sweep")]
    NotAgreement { backend: &'static str },
    #[error("requested backends are unavailable — {}", .skipped.iter()
        .map(|s| format!("{}: {}", s.backend, s.reason))
        .collect::<Vec<_>>()
        .join("; "))]
    AllSkipped { skipped: Vec<Skipped> },
}
