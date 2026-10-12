//! Bounded transfer from the microphone stream to WAV storage.

use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::capture::{RecordingMode, SilenceDuration};
use crate::audio::spool::WavSpool;
use crate::audio::{CaptureError, RecordedAudio, StopSignal, canonical_samples};
use crate::voice_activity::{SPEECH_THRESHOLD, VoiceActivityDetector};

use super::stream::{DRAIN_BUFFER_SAMPLES, LiveStream};
use super::{MicrophoneError, Result};

const EMPTY_BUFFER_WAIT: Duration = Duration::from_millis(10);
const NO_SPEECH_TIMEOUT: Duration = Duration::from_secs(10);
const PRE_SPEECH_FRAMES: usize = 4;
const SPEECH_END_THRESHOLD: f32 = 0.35;

pub(super) fn capture<V: VoiceActivityDetector>(
    live: &mut LiveStream,
    mut recording: CaptureRecording<V>,
    pre_press: &mut VecDeque<i16>,
    maximum_duration: Duration,
    stop: &StopSignal,
) -> Result<RecordedAudio> {
    if prepend_pre_press(&mut recording, pre_press)? == CaptureProgress::Continue {
        pump(live, &mut recording, maximum_duration, stop)?;
    }
    recording.drain_tail(live)?;
    live.check_failures()?;
    recording.finish()
}

fn prepend_pre_press<V: VoiceActivityDetector>(
    recording: &mut CaptureRecording<V>,
    pre_press: &mut VecDeque<i16>,
) -> Result<CaptureProgress> {
    let progress = recording.push(pre_press.make_contiguous())?;
    pre_press.clear();
    Ok(progress)
}

fn pump<V: VoiceActivityDetector>(
    live: &mut LiveStream,
    recording: &mut CaptureRecording<V>,
    maximum_duration: Duration,
    stop: &StopSignal,
) -> Result<()> {
    let deadline = Instant::now() + maximum_duration;
    let mut scratch = [0_i16; DRAIN_BUFFER_SAMPLES];
    loop {
        let input = live.pop(&mut scratch)?;
        if recording.push(input)? == CaptureProgress::Complete {
            return Ok(());
        }
        live.check_state()?;

        if stop.should_stop() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CaptureError::DeadlineExceeded(maximum_duration));
        }
        if input.is_empty() {
            thread::park_timeout(EMPTY_BUFFER_WAIT);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CaptureProgress {
    Continue,
    Complete,
}

pub(super) enum CaptureRecording<V> {
    OnSilence(Box<SilenceRecording<V>>),
    UntilStopped(WavSpool),
}

impl<V: VoiceActivityDetector> CaptureRecording<V> {
    pub(super) fn new(spool: WavSpool, mode: RecordingMode, vad: V) -> Self {
        match mode {
            RecordingMode::StopOnSilence { trailing_silence } => Self::OnSilence(Box::new(
                SilenceRecording::new(spool, vad, trailing_silence),
            )),
            RecordingMode::UntilStopped => Self::UntilStopped(spool),
        }
    }

    fn push(&mut self, samples: &[i16]) -> Result<CaptureProgress> {
        match self {
            Self::OnSilence(recording) => recording.push(samples),
            Self::UntilStopped(spool) => {
                spool.write_samples(samples)?;
                Ok(CaptureProgress::Continue)
            }
        }
    }

    /// Writes the samples still buffered when a manual stop lands, so the
    /// last words before the stop are kept.
    fn drain_tail(&mut self, live: &mut LiveStream) -> Result<()> {
        if let Self::UntilStopped(spool) = self {
            let mut scratch = [0_i16; DRAIN_BUFFER_SAMPLES];
            while let input @ [_, ..] = live.pop(&mut scratch)? {
                spool.write_samples(input)?;
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<RecordedAudio> {
        match self {
            Self::OnSilence(recording) => recording.finish(),
            Self::UntilStopped(spool) => spool.finish().map_err(Into::into),
        }
    }
}

pub(super) struct SilenceRecording<V> {
    spool: WavSpool,
    vad: V,
    trailing_silence_samples: u64,
    frame: Vec<i16>,
    pre_speech: VecDeque<i16>,
    speech_started: bool,
    samples_observed: u64,
    silence_started_at: Option<u64>,
}

impl<V: VoiceActivityDetector> SilenceRecording<V> {
    fn new(spool: WavSpool, mut vad: V, trailing_silence: SilenceDuration) -> Self {
        vad.reset();
        Self {
            spool,
            vad,
            trailing_silence_samples: canonical_samples(trailing_silence.get()),
            frame: Vec::with_capacity(V::FRAME_SAMPLES),
            pre_speech: VecDeque::with_capacity((PRE_SPEECH_FRAMES + 1) * V::FRAME_SAMPLES),
            speech_started: false,
            samples_observed: 0,
            silence_started_at: None,
        }
    }

    fn push(&mut self, mut samples: &[i16]) -> Result<CaptureProgress> {
        while !samples.is_empty() {
            let available = V::FRAME_SAMPLES - self.frame.len();
            let copied = available.min(samples.len());
            self.frame.extend_from_slice(&samples[..copied]);
            samples = &samples[copied..];

            if self.frame.len() == V::FRAME_SAMPLES {
                let progress = self.process_frame()?;
                self.frame.clear();
                if progress == CaptureProgress::Complete {
                    return Ok(CaptureProgress::Complete);
                }
            }
        }
        Ok(CaptureProgress::Continue)
    }

    fn process_frame(&mut self) -> Result<CaptureProgress> {
        self.samples_observed = self
            .samples_observed
            .saturating_add(V::FRAME_SAMPLES as u64);
        let probability = self
            .vad
            .probability(&self.frame)
            .map_err(MicrophoneError::VoiceActivity)?;

        if !self.speech_started {
            return self.wait_for_speech(probability);
        }

        self.spool.write_samples(&self.frame)?;
        if probability >= SPEECH_THRESHOLD {
            self.silence_started_at = None;
        } else if probability <= SPEECH_END_THRESHOLD && self.silence_started_at.is_none() {
            self.silence_started_at = Some(
                self.samples_observed
                    .saturating_sub(V::FRAME_SAMPLES as u64),
            );
        }
        let silence_samples = self
            .silence_started_at
            .map_or(0, |started| self.samples_observed.saturating_sub(started));
        if silence_samples >= self.trailing_silence_samples {
            Ok(CaptureProgress::Complete)
        } else {
            Ok(CaptureProgress::Continue)
        }
    }

    fn wait_for_speech(&mut self, probability: f32) -> Result<CaptureProgress> {
        if probability >= SPEECH_THRESHOLD {
            self.speech_started = true;
            self.spool
                .write_samples(self.pre_speech.make_contiguous())?;
            self.pre_speech.clear();
            self.spool.write_samples(&self.frame)?;
            return Ok(CaptureProgress::Continue);
        }
        let capacity = PRE_SPEECH_FRAMES * V::FRAME_SAMPLES;
        self.pre_speech.extend(self.frame.iter().copied());
        if self.pre_speech.len() > capacity {
            self.pre_speech.drain(..self.pre_speech.len() - capacity);
        }
        if self.samples_observed >= canonical_samples(NO_SPEECH_TIMEOUT) {
            return Err(CaptureError::NoSpeech(NO_SPEECH_TIMEOUT));
        }
        Ok(CaptureProgress::Continue)
    }

    fn finish(mut self) -> Result<RecordedAudio> {
        if !self.speech_started {
            return Err(CaptureError::NoSpeech(NO_SPEECH_TIMEOUT));
        }
        if !self.frame.is_empty() {
            self.spool.write_samples(&self.frame)?;
        }
        self.spool.finish().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use crate::audio::testing::spooled;
    use crate::audio::{RecordingWriter, canonical_duration};
    use crate::voice_activity::VoiceActivityError;

    use super::*;

    struct Loudness;

    impl VoiceActivityDetector for Loudness {
        const FRAME_SAMPLES: usize = 160;

        fn probability(&mut self, frame: &[i16]) -> std::result::Result<f32, VoiceActivityError> {
            Ok(if frame.iter().any(|&sample| sample != 0) {
                1.0
            } else {
                0.0
            })
        }
    }

    fn listening(
        trailing_silence: Duration,
    ) -> (
        tempfile::TempDir,
        RecordingWriter,
        SilenceRecording<Loudness>,
    ) {
        let (directory, writer, spool, _reader) = spooled();
        let trailing_silence =
            SilenceDuration::new(trailing_silence).expect("the trailing silence is in range");
        let recording = SilenceRecording::new(spool, Loudness, trailing_silence);
        (directory, writer, recording)
    }

    fn speech(duration: Duration) -> Vec<i16> {
        vec![1_000; canonical_samples(duration) as usize]
    }

    fn silence(duration: Duration) -> Vec<i16> {
        vec![0; canonical_samples(duration) as usize]
    }

    #[test]
    fn a_recording_stops_once_the_trailing_silence_has_lasted() {
        let (_directory, _writer, mut recording) = listening(Duration::from_millis(500));

        let spoken = recording.push(&speech(Duration::from_secs(1)));
        let one_frame_short = recording.push(&silence(Duration::from_millis(490)));
        let done = recording.push(&silence(Duration::from_millis(10)));

        assert!(matches!(spoken, Ok(CaptureProgress::Continue)));
        assert!(matches!(one_frame_short, Ok(CaptureProgress::Continue)));
        assert!(matches!(done, Ok(CaptureProgress::Complete)));
    }

    #[test]
    fn speech_resuming_mid_pause_restarts_the_silence_clock() {
        let (_directory, _writer, mut recording) = listening(Duration::from_millis(500));

        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        recording
            .push(&silence(Duration::from_millis(400)))
            .expect("the pause is written");
        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech resumes");
        let paused = recording.push(&silence(Duration::from_millis(400)));

        assert!(
            matches!(paused, Ok(CaptureProgress::Continue)),
            "two separate pauses must not add up to the trailing silence"
        );
    }

    #[test]
    fn silence_until_the_timeout_is_no_speech() {
        let (_directory, _writer, mut recording) = listening(Duration::from_millis(500));

        let outcome = recording.push(&silence(NO_SPEECH_TIMEOUT));

        assert!(matches!(outcome, Err(CaptureError::NoSpeech(_))));
    }

    #[test]
    fn the_audio_just_before_speech_is_kept() {
        let (_directory, _writer, mut recording) = listening(Duration::from_millis(500));

        recording
            .push(&silence(Duration::from_secs(1)))
            .expect("silence is buffered");
        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        recording
            .push(&silence(Duration::from_millis(500)))
            .expect("the trailing silence is written");
        let audio = recording.finish().expect("the recording heard speech");

        let pre_speech = canonical_duration((PRE_SPEECH_FRAMES * Loudness::FRAME_SAMPLES) as u64);
        assert_eq!(audio.duration(), pre_speech + Duration::from_millis(1_500));
    }

    #[test]
    fn finishing_keeps_the_partial_frame() {
        let (_directory, _writer, mut recording) = listening(Duration::from_millis(500));

        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        recording
            .push(&[1_000; 100])
            .expect("a partial frame is buffered");
        let audio = recording.finish().expect("the recording heard speech");

        assert_eq!(audio.duration(), canonical_duration(16_000 + 100));
    }

    #[test]
    fn finishing_before_any_speech_is_no_speech() {
        let (_directory, _writer, mut recording) = listening(Duration::from_millis(500));

        recording
            .push(&silence(Duration::from_secs(1)))
            .expect("silence is buffered");

        assert!(matches!(recording.finish(), Err(CaptureError::NoSpeech(_))));
    }
}
