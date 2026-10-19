//! One live synthesis run and the PCM it produces.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

#[cfg(feature = "playback")]
use super::{PlaybackError, SpeakerOutput};
use crate::audio::Clip;
use crate::backend::BackendError;
use crate::cancel::CancelToken;

use super::{Engine, SpeakingRate, SpeechStream, SpeechText, SynthesisError, Voice, stream};

const CLIP_READ_SAMPLES: usize = 4_096;

/// A loaded synthesis engine ready to start utterances.
pub struct Synthesis<T> {
    pub(super) engine: Arc<T>,
}

impl<T> Synthesis<T> {
    pub fn new(engine: T) -> Self {
        Self {
            engine: Arc::new(engine),
        }
    }
}

/// One PCM-producing utterance.
///
/// The session owns its synthesis worker. Dropping it cancels and joins that
/// worker, so an abandoned reader cannot leave model inference running.
pub struct Session {
    stream: Option<SpeechStream>,
    worker: Option<JoinHandle<Result<(), SynthesisError>>>,
    stop: CancelToken,
    #[cfg_attr(
        not(feature = "playback"),
        expect(dead_code, reason = "only playback reads it")
    )]
    cancel: CancelToken,
    backend: &'static str,
}

impl<T: Engine> Synthesis<T> {
    /// Starts an utterance with the engine's default voice and natural pace.
    pub fn start(&self, text: &SpeechText) -> Result<Session, SynthesisError> {
        self.start_with(text, None, SpeakingRate::standard(), &CancelToken::never())
    }

    /// Starts an utterance and returns its PCM as it is produced.
    ///
    /// An engine that owns playback cannot provide a PCM session. Use
    /// `Synthesis::speak_with` for that engine instead.
    pub fn start_with(
        &self,
        text: &SpeechText,
        voice: Option<&Voice>,
        rate: SpeakingRate,
        cancel: &CancelToken,
    ) -> Result<Session, SynthesisError> {
        if cancel.is_cancelled() {
            return Err(SynthesisError::Cancelled);
        }
        if self.engine.plays_audio_itself() {
            return Err(SynthesisError::PcmUnavailable { backend: T::NAME });
        }

        let (writer, speech) = stream(self.engine.sample_rate());
        let stop = CancelToken::new();
        let requested = cancel.clone();
        let stopped = stop.clone();
        let synthesis_cancel =
            CancelToken::from_fn(move || requested.is_cancelled() || stopped.is_cancelled());

        let engine = Arc::clone(&self.engine);
        let text = text.clone();
        let voice = voice.cloned();
        let worker_cancel = synthesis_cancel.clone();
        let worker = thread::Builder::new()
            .name(format!("{}-synthesis", T::NAME))
            .spawn(move || {
                engine.synthesize_with(&text, voice.as_ref(), rate, writer, &worker_cancel)
            })
            .map_err(|source| SynthesisError::Inference {
                backend: T::NAME,
                source: BackendError::engine(source),
            })?;

        Ok(Session {
            stream: Some(speech),
            worker: Some(worker),
            stop,
            cancel: synthesis_cancel,
            backend: T::NAME,
        })
    }
}

impl Session {
    pub fn sample_rate(&self) -> u32 {
        self.stream
            .as_ref()
            .expect("the stream exists until the session is consumed")
            .sample_rate()
    }

    /// Copies the next PCM samples into `out`, waiting for samples or the
    /// terminal synthesis result. Zero means the utterance completed.
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize, SynthesisError> {
        // Checked before the stream so a bad buffer never joins the worker.
        if out.is_empty() {
            return Err(SynthesisError::EmptyReadBuffer);
        }
        let result = self
            .stream
            .as_mut()
            .expect("the stream exists until the session is consumed")
            .read(out);
        if matches!(result, Ok(0) | Err(_))
            && let Err(error) = self.join()
        {
            return Err(error);
        }
        result
    }

    /// Reads the rest of the utterance as one clip at the engine's rate.
    pub fn into_clip(mut self) -> Result<Clip, SynthesisError> {
        let sample_rate = self.sample_rate();
        let mut samples = Vec::new();
        let mut block = [0.0; CLIP_READ_SAMPLES];
        loop {
            let read = self.read(&mut block)?;
            if read == 0 {
                break;
            }
            samples.extend_from_slice(&block[..read]);
        }
        Clip::new(sample_rate, samples).map_err(|source| SynthesisError::Inference {
            backend: self.backend,
            source: BackendError::engine(source),
        })
    }

    /// Plays this utterance and reports when its first source sample reaches
    /// the output callback.
    #[cfg(feature = "playback")]
    pub fn play_on(
        mut self,
        output: &SpeakerOutput,
        started: impl FnOnce(),
    ) -> Result<(), SynthesisError> {
        let speech = self
            .stream
            .take()
            .expect("the stream exists until the session is consumed");
        let played = output.play_with(speech, &self.cancel, started);
        let synthesized = self.join();

        if let Err(error) = synthesized
            && !matches!(error, SynthesisError::Cancelled)
        {
            return Err(error);
        }
        match played {
            Ok(()) => Ok(()),
            Err(PlaybackError::Cancelled) => Err(SynthesisError::Cancelled),
            Err(source) => Err(SynthesisError::Playback { source }),
        }
    }

    fn join(&mut self) -> Result<(), SynthesisError> {
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().unwrap_or(Err(SynthesisError::WorkerPanicked {
            backend: self.backend,
        }))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.cancel();
        drop(self.stream.take());
        let _finished = self.join();
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::synthesis::{SpeechWriter, Synthesizer};

    enum Behavior {
        Samples,
        WaitForCancel(Arc<AtomicBool>),
        UnsupportedVoice,
        Panic,
        Direct,
    }

    struct Fake {
        behavior: Behavior,
    }

    impl Fake {
        fn with(behavior: Behavior) -> Self {
            Self { behavior }
        }
    }

    impl Engine for Fake {
        const NAME: &'static str = "fake";

        type Config = Behavior;

        fn check_from(_config: &Self::Config) -> Result<PathBuf, SynthesisError> {
            Ok(PathBuf::from("fake"))
        }

        fn load_from(config: Self::Config) -> Result<Self, SynthesisError> {
            Ok(Self::with(config))
        }
    }

    impl Synthesizer for Fake {
        fn sample_rate(&self) -> u32 {
            24_000
        }

        fn plays_audio_itself(&self) -> bool {
            matches!(self.behavior, Behavior::Direct)
        }

        fn synthesize_with(
            &self,
            _text: &SpeechText,
            _voice: Option<&Voice>,
            _rate: SpeakingRate,
            mut out: SpeechWriter,
            cancel: &CancelToken,
        ) -> Result<(), SynthesisError> {
            match &self.behavior {
                Behavior::Samples => {
                    out.write(&[0.1, 0.2, 0.3])
                        .expect("the session is listening");
                    out.finish();
                    Ok(())
                }
                Behavior::WaitForCancel(stopped) => {
                    while !cancel.is_cancelled() {
                        std::thread::yield_now();
                    }
                    stopped.store(true, Ordering::Release);
                    out.fail(SynthesisError::Cancelled);
                    Err(SynthesisError::Cancelled)
                }
                Behavior::UnsupportedVoice => {
                    out.fail(SynthesisError::UnsupportedVoice {
                        backend: Self::NAME,
                        voice: "unheard".to_owned(),
                    });
                    Err(SynthesisError::UnsupportedVoice {
                        backend: Self::NAME,
                        voice: "original".to_owned(),
                    })
                }
                Behavior::Panic => panic!("scripted worker panic"),
                Behavior::Direct => {
                    out.finish();
                    Ok(())
                }
            }
        }
    }

    fn text() -> SpeechText {
        SpeechText::new("hello").expect("test text is valid")
    }

    #[test]
    fn a_session_streams_pcm_and_its_sample_rate() {
        let synthesis = Synthesis::new(Fake::with(Behavior::Samples));
        let mut session = synthesis.start(&text()).expect("the session starts");
        assert_eq!(session.sample_rate(), 24_000);

        let mut heard = Vec::new();
        let mut samples = [0.0; 2];
        loop {
            let copied = session.read(&mut samples).expect("synthesis succeeds");
            if copied == 0 {
                break;
            }
            heard.extend_from_slice(&samples[..copied]);
        }

        assert_eq!(heard, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn a_session_reads_to_one_clip_at_the_engine_rate() {
        let synthesis = Synthesis::new(Fake::with(Behavior::Samples));
        let session = synthesis.start(&text()).expect("the session starts");

        let clip = session.into_clip().expect("synthesis succeeds");

        assert_eq!(clip.sample_rate(), 24_000);
        assert_eq!(clip.samples(), [0.1, 0.2, 0.3]);
    }

    #[test]
    fn dropping_a_session_cancels_and_joins_its_worker() {
        let stopped = Arc::new(AtomicBool::new(false));
        let synthesis = Synthesis::new(Fake::with(Behavior::WaitForCancel(Arc::clone(&stopped))));
        let session = synthesis.start(&text()).expect("the session starts");

        drop(session);

        assert!(stopped.load(Ordering::Acquire));
    }

    #[test]
    fn a_terminal_read_returns_the_worker_cause() {
        let synthesis = Synthesis::new(Fake::with(Behavior::UnsupportedVoice));
        let mut session = synthesis.start(&text()).expect("the session starts");

        let error = session
            .read(&mut [0.0; 1])
            .expect_err("the engine rejects the voice");

        assert!(matches!(
            error,
            SynthesisError::UnsupportedVoice { voice, .. } if voice == "original"
        ));
    }

    #[test]
    fn a_panicked_worker_has_a_typed_terminal_error() {
        let synthesis = Synthesis::new(Fake::with(Behavior::Panic));
        let mut session = synthesis.start(&text()).expect("the session starts");

        let error = session.read(&mut [0.0; 1]).expect_err("the worker panics");

        assert!(matches!(
            error,
            SynthesisError::WorkerPanicked { backend: "fake" }
        ));
    }

    #[test]
    fn a_direct_playback_engine_refuses_a_pcm_session() {
        let synthesis = Synthesis::new(Fake::with(Behavior::Direct));

        let error = synthesis
            .start(&text())
            .err()
            .expect("a direct engine has no PCM");

        assert!(matches!(
            error,
            SynthesisError::PcmUnavailable { backend: "fake" }
        ));
    }

    #[test]
    fn a_boxed_or_shared_engine_still_plays_audio_itself() {
        let boxed: Box<dyn Synthesizer> = Box::new(Fake::with(Behavior::Direct));
        let shared = Arc::new(Fake::with(Behavior::Direct));

        assert!(Synthesizer::plays_audio_itself(&boxed));
        assert!(Synthesizer::plays_audio_itself(&shared));
    }

    #[cfg(feature = "playback")]
    #[test]
    fn a_direct_engine_speaks_without_a_pcm_session() {
        let synthesis = Synthesis::new(Fake::with(Behavior::Direct));

        synthesis
            .speak_with(
                &text(),
                None,
                SpeakingRate::standard(),
                &CancelToken::never(),
            )
            .expect("the engine plays the speech itself");
    }

    #[test]
    fn an_empty_read_buffer_is_rejected() {
        let synthesis = Synthesis::new(Fake::with(Behavior::Samples));
        let mut session = synthesis.start(&text()).expect("the session starts");

        assert!(matches!(
            session.read(&mut []),
            Err(SynthesisError::EmptyReadBuffer)
        ));
        assert_eq!(
            session
                .read(&mut [0.0; 4])
                .expect("the session can still read"),
            3
        );
    }
}
