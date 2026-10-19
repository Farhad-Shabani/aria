//! Bounded transfer from the microphone stream to WAV storage.

use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::capture::{RecordingMode, SilenceDuration};
use crate::audio::spool::WavSpool;
use crate::audio::{CaptureError, RecordedAudio, StopSignal, canonical_samples};
use crate::voice_activity::{Boundary, Endpointer, LEAD_IN_FRAMES, VoiceActivityDetector};

use super::stream::{DRAIN_BUFFER_SAMPLES, LiveStream};
use super::{MicrophoneError, Result};

const EMPTY_BUFFER_WAIT: Duration = Duration::from_millis(10);
const NO_SPEECH_TIMEOUT: Duration = Duration::from_secs(10);

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
    endpointer: Endpointer<V>,
    /// Audio heard before speech opened, enough to cover the lead-in of an
    /// onset frame that began in an earlier push.
    lead_in: VecDeque<i16>,
    pushed_samples: u64,
    speech_started: bool,
}

impl<V: VoiceActivityDetector> SilenceRecording<V> {
    const LEAD_IN_CAPACITY: usize = (LEAD_IN_FRAMES + 1) * V::FRAME_SAMPLES;

    fn new(spool: WavSpool, vad: V, trailing_silence: SilenceDuration) -> Self {
        Self {
            spool,
            endpointer: Endpointer::new(vad, trailing_silence),
            lead_in: VecDeque::with_capacity(Self::LEAD_IN_CAPACITY),
            pushed_samples: 0,
            speech_started: false,
        }
    }

    fn push(&mut self, samples: &[i16]) -> Result<CaptureProgress> {
        let pushed_from = self.pushed_samples;
        let boundaries = self
            .endpointer
            .push(samples)
            .map_err(MicrophoneError::VoiceActivity)?;
        self.pushed_samples += samples.len() as u64;
        let end = boundaries.iter().find_map(|boundary| match boundary {
            Boundary::End(at) => Some(*at),
            Boundary::Start(_) => None,
        });
        let kept_to = end.unwrap_or(self.pushed_samples);

        if self.speech_started {
            self.spool
                .write_samples(&samples[..(kept_to - pushed_from) as usize])?;
        } else if let Some(Boundary::Start(at)) = boundaries.first() {
            self.open_speech(*at, samples, kept_to)?;
        } else {
            self.keep_lead_in(samples);
            if self.pushed_samples >= canonical_samples(NO_SPEECH_TIMEOUT) {
                return Err(CaptureError::NoSpeech(NO_SPEECH_TIMEOUT));
            }
        }

        Ok(if end.is_some() {
            CaptureProgress::Complete
        } else {
            CaptureProgress::Continue
        })
    }

    fn open_speech(&mut self, start: u64, samples: &[i16], kept_to: u64) -> Result<()> {
        self.speech_started = true;
        self.lead_in.extend(samples);
        let heard_from = self.pushed_samples - self.lead_in.len() as u64;
        let from = start.saturating_sub((LEAD_IN_FRAMES * V::FRAME_SAMPLES) as u64);
        let heard = self.lead_in.make_contiguous();
        self.spool
            .write_samples(&heard[(from - heard_from) as usize..(kept_to - heard_from) as usize])?;
        self.lead_in.clear();
        Ok(())
    }

    fn keep_lead_in(&mut self, samples: &[i16]) {
        self.lead_in.extend(samples);
        let surplus = self.lead_in.len().saturating_sub(Self::LEAD_IN_CAPACITY);
        self.lead_in.drain(..surplus);
    }

    fn finish(self) -> Result<RecordedAudio> {
        if !self.speech_started {
            return Err(CaptureError::NoSpeech(NO_SPEECH_TIMEOUT));
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

        let pre_speech = canonical_duration((LEAD_IN_FRAMES * Loudness::FRAME_SAMPLES) as u64);
        assert_eq!(audio.duration(), pre_speech + Duration::from_millis(1_500));
    }

    #[test]
    fn audio_after_the_trailing_silence_in_the_same_push_is_dropped() {
        let (_directory, _writer, mut recording) = listening(Duration::from_millis(500));
        let mut batch = speech(Duration::from_secs(1));
        batch.extend(silence(Duration::from_secs(1)));
        batch.extend(speech(Duration::from_secs(1)));

        let progress = recording.push(&batch);
        let audio = recording.finish().expect("the recording heard speech");

        assert!(matches!(progress, Ok(CaptureProgress::Complete)));
        assert_eq!(audio.duration(), Duration::from_millis(1_500));
    }

    #[test]
    fn an_onset_frame_split_across_pushes_keeps_its_whole_lead_in() {
        let (_directory, _writer, mut recording) = listening(Duration::from_millis(500));
        let frame = Loudness::FRAME_SAMPLES;
        // Ends 150 samples into a frame, so the onset frame starts in this push.
        let silence_before = 5 * frame + 150;

        recording
            .push(&vec![0; silence_before])
            .expect("silence is buffered");
        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        let audio = recording.finish().expect("the recording heard speech");

        let lead_in = LEAD_IN_FRAMES * frame + 150;
        assert_eq!(
            audio.duration(),
            canonical_duration(lead_in as u64) + Duration::from_secs(1)
        );
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
