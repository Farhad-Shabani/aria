//! How a recording ends: on trailing silence, on a stop, or once auto-stop
//! passes without speech. Each mode works on samples, whatever their source.

use std::collections::VecDeque;
use std::time::Duration;

use crate::audio::capture::{AutoStop, MicrophoneError, RecordingMode, SilenceDuration};
use crate::audio::limit::LimitedWriter;
use crate::audio::{CaptureError, canonical_duration, canonical_samples};
use crate::voice_activity::{
    Boundary, Endpointer, LEAD_IN_FRAMES, SPEECH_THRESHOLD, VoiceActivityDetector,
};

type Result<T> = std::result::Result<T, CaptureError>;

const NO_SPEECH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CaptureProgress {
    Continue,
    Complete,
}

pub(super) enum CaptureRecording<V> {
    OnSilence(Box<SilenceRecording<V>>),
    UntilStopped(LimitedWriter),
    UntilQuiet(Box<QuietRecording<V>>),
}

impl<V: VoiceActivityDetector> CaptureRecording<V> {
    pub(super) fn new(limited: LimitedWriter, mode: RecordingMode, vad: V) -> Self {
        match mode {
            RecordingMode::StopOnSilence { trailing_silence } => Self::OnSilence(Box::new(
                SilenceRecording::new(limited, vad, trailing_silence),
            )),
            RecordingMode::UntilStopped => Self::UntilStopped(limited),
            RecordingMode::UntilQuiet { auto_stop } => {
                Self::UntilQuiet(Box::new(QuietRecording::new(limited, vad, auto_stop)))
            }
        }
    }

    pub(super) fn push(&mut self, samples: &[i16]) -> Result<CaptureProgress> {
        match self {
            Self::OnSilence(recording) => recording.push(samples),
            Self::UntilStopped(limited) => {
                limited.write_samples(samples)?;
                Ok(CaptureProgress::Continue)
            }
            Self::UntilQuiet(recording) => recording.push(samples),
        }
    }

    /// Where the samples still buffered when a manual stop lands go, so the
    /// last words before the stop are kept. `None` once silence ended it.
    pub(super) fn tail(&mut self) -> Option<&mut LimitedWriter> {
        match self {
            Self::OnSilence(_) => None,
            Self::UntilStopped(limited) => Some(limited),
            Self::UntilQuiet(recording) => Some(&mut recording.limited),
        }
    }

    pub(super) fn finish(self) -> Result<()> {
        match self {
            Self::OnSilence(recording) => recording.finish(),
            Self::UntilStopped(limited) => limited.finish().map_err(Into::into),
            Self::UntilQuiet(recording) => recording.limited.finish().map_err(Into::into),
        }
    }
}

/// Keeps every sample and completes once `auto_stop` has passed with no
/// frame scored as speech.
pub(super) struct QuietRecording<V> {
    limited: LimitedWriter,
    vad: V,
    auto_stop_samples: u64,
    frame: Vec<i16>,
    quiet_samples: u64,
}

impl<V: VoiceActivityDetector> QuietRecording<V> {
    fn new(limited: LimitedWriter, mut vad: V, auto_stop: AutoStop) -> Self {
        // A zero-sample frame is always full, so `push` would never return.
        const { assert!(V::FRAME_SAMPLES > 0) };
        vad.reset();
        Self {
            limited,
            vad,
            auto_stop_samples: canonical_samples(auto_stop.get()),
            frame: Vec::with_capacity(V::FRAME_SAMPLES),
            quiet_samples: 0,
        }
    }

    fn push(&mut self, mut samples: &[i16]) -> Result<CaptureProgress> {
        self.limited.write_samples(samples)?;
        while !samples.is_empty() {
            let copied = (V::FRAME_SAMPLES - self.frame.len()).min(samples.len());
            self.frame.extend_from_slice(&samples[..copied]);
            samples = &samples[copied..];
            if self.frame.len() == V::FRAME_SAMPLES {
                self.score_frame()?;
            }
        }
        if self.quiet_samples < self.auto_stop_samples {
            return Ok(CaptureProgress::Continue);
        }
        tracing::info!(
            stage = "recording",
            auto_stop_ms = canonical_duration(self.auto_stop_samples).as_millis() as u64,
            "auto-stop ended the recording"
        );
        Ok(CaptureProgress::Complete)
    }

    fn score_frame(&mut self) -> Result<()> {
        let probability = self
            .vad
            .probability(&self.frame)
            .map_err(MicrophoneError::VoiceActivity)?;
        self.frame.clear();
        self.quiet_samples = if probability >= SPEECH_THRESHOLD {
            0
        } else {
            self.quiet_samples + V::FRAME_SAMPLES as u64
        };
        Ok(())
    }
}

pub(super) struct SilenceRecording<V> {
    limited: LimitedWriter,
    endpointer: Endpointer<V>,
    /// Audio heard before speech opened, enough to cover the lead-in of an
    /// onset frame that began in an earlier push.
    lead_in: VecDeque<i16>,
    pushed_samples: u64,
    speech_started: bool,
}

impl<V: VoiceActivityDetector> SilenceRecording<V> {
    const LEAD_IN_CAPACITY: usize = (LEAD_IN_FRAMES + 1) * V::FRAME_SAMPLES;

    fn new(limited: LimitedWriter, vad: V, trailing_silence: SilenceDuration) -> Self {
        Self {
            limited,
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
            self.limited
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
        self.limited
            .write_samples(&heard[(from - heard_from) as usize..(kept_to - heard_from) as usize])?;
        self.lead_in.clear();
        Ok(())
    }

    fn keep_lead_in(&mut self, samples: &[i16]) {
        self.lead_in.extend(samples);
        let surplus = self.lead_in.len().saturating_sub(Self::LEAD_IN_CAPACITY);
        self.lead_in.drain(..surplus);
    }

    fn finish(self) -> Result<()> {
        if !self.speech_started {
            return Err(CaptureError::NoSpeech(NO_SPEECH_TIMEOUT));
        }
        self.limited.finish().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use crate::audio::testing::{Loudness, limited, silence, speech};
    use crate::audio::{RecordingReader, RecordingWriter, canonical_duration};

    use super::*;

    fn listening(
        trailing_silence: Duration,
    ) -> (RecordingWriter, RecordingReader, SilenceRecording<Loudness>) {
        let (writer, limited, reader) = limited();
        let trailing_silence =
            SilenceDuration::new(trailing_silence).expect("the trailing silence is in range");
        let recording = SilenceRecording::new(limited, Loudness, trailing_silence);
        (writer, reader, recording)
    }

    fn quiet_listening(
        auto_stop: Duration,
    ) -> (RecordingWriter, RecordingReader, QuietRecording<Loudness>) {
        let (writer, limited, reader) = limited();
        let auto_stop = AutoStop::new(auto_stop).expect("the auto-stop is in range");
        let recording = QuietRecording::new(limited, Loudness, auto_stop);
        (writer, reader, recording)
    }

    #[test]
    fn a_recording_stops_once_the_trailing_silence_has_lasted() {
        let (_writer, _reader, mut recording) = listening(Duration::from_millis(500));

        let spoken = recording.push(&speech(Duration::from_secs(1)));
        let one_frame_short = recording.push(&silence(Duration::from_millis(490)));
        let done = recording.push(&silence(Duration::from_millis(10)));

        assert!(matches!(spoken, Ok(CaptureProgress::Continue)));
        assert!(matches!(one_frame_short, Ok(CaptureProgress::Continue)));
        assert!(matches!(done, Ok(CaptureProgress::Complete)));
    }

    #[test]
    fn speech_resuming_mid_pause_restarts_the_silence_clock() {
        let (_writer, _reader, mut recording) = listening(Duration::from_millis(500));

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
        let (_writer, _reader, mut recording) = listening(Duration::from_millis(500));

        let outcome = recording.push(&silence(NO_SPEECH_TIMEOUT));

        assert!(matches!(outcome, Err(CaptureError::NoSpeech(_))));
    }

    #[test]
    fn the_audio_just_before_speech_is_kept() {
        let (_writer, reader, mut recording) = listening(Duration::from_millis(500));

        recording
            .push(&silence(Duration::from_secs(1)))
            .expect("silence is buffered");
        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        recording
            .push(&silence(Duration::from_millis(500)))
            .expect("the trailing silence is written");
        recording.finish().expect("the recording heard speech");

        let pre_speech = canonical_duration((LEAD_IN_FRAMES * Loudness::FRAME_SAMPLES) as u64);
        assert_eq!(reader.duration(), pre_speech + Duration::from_millis(1_500));
    }

    #[test]
    fn audio_after_the_trailing_silence_in_the_same_push_is_dropped() {
        let (_writer, reader, mut recording) = listening(Duration::from_millis(500));
        let mut batch = speech(Duration::from_secs(1));
        batch.extend(silence(Duration::from_secs(1)));
        batch.extend(speech(Duration::from_secs(1)));

        let progress = recording.push(&batch);
        recording.finish().expect("the recording heard speech");

        assert!(matches!(progress, Ok(CaptureProgress::Complete)));
        assert_eq!(reader.duration(), Duration::from_millis(1_500));
    }

    #[test]
    fn an_onset_frame_split_across_pushes_keeps_its_whole_lead_in() {
        let (_writer, reader, mut recording) = listening(Duration::from_millis(500));
        let frame = Loudness::FRAME_SAMPLES;
        // Ends 150 samples into a frame, so the onset frame starts in this push.
        let silence_before = 5 * frame + 150;

        recording
            .push(&vec![0; silence_before])
            .expect("silence is buffered");
        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        recording.finish().expect("the recording heard speech");

        let lead_in = LEAD_IN_FRAMES * frame + 150;
        assert_eq!(
            reader.duration(),
            canonical_duration(lead_in as u64) + Duration::from_secs(1)
        );
    }

    #[test]
    fn finishing_keeps_the_partial_frame() {
        let (_writer, reader, mut recording) = listening(Duration::from_millis(500));

        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        recording
            .push(&[1_000; 100])
            .expect("a partial frame is buffered");
        recording.finish().expect("the recording heard speech");

        assert_eq!(reader.duration(), canonical_duration(16_000 + 100));
    }

    #[test]
    fn finishing_before_any_speech_is_no_speech() {
        let (_writer, _reader, mut recording) = listening(Duration::from_millis(500));

        recording
            .push(&silence(Duration::from_secs(1)))
            .expect("silence is buffered");

        assert!(matches!(recording.finish(), Err(CaptureError::NoSpeech(_))));
    }

    #[test]
    fn a_quiet_recording_stops_once_auto_stop_has_lasted() {
        let (_writer, _reader, mut recording) = quiet_listening(Duration::from_secs(3));

        let spoken = recording.push(&speech(Duration::from_secs(1)));
        let one_frame_short = recording.push(&silence(Duration::from_millis(2_990)));
        let done = recording.push(&silence(Duration::from_millis(10)));

        assert!(matches!(spoken, Ok(CaptureProgress::Continue)));
        assert!(matches!(one_frame_short, Ok(CaptureProgress::Continue)));
        assert!(matches!(done, Ok(CaptureProgress::Complete)));
    }

    #[test]
    fn speech_restarts_the_auto_stop_clock() {
        let (_writer, _reader, mut recording) = quiet_listening(Duration::from_secs(3));

        recording
            .push(&silence(Duration::from_secs(2)))
            .expect("the pause is written");
        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        let paused = recording.push(&silence(Duration::from_secs(2)));

        assert!(
            matches!(paused, Ok(CaptureProgress::Continue)),
            "two separate pauses must not add up to the auto-stop"
        );
    }

    #[test]
    fn silence_from_the_start_counts_toward_auto_stop() {
        let (_writer, _reader, mut recording) = quiet_listening(Duration::from_secs(3));

        let outcome = recording.push(&silence(Duration::from_secs(3)));

        assert!(matches!(outcome, Ok(CaptureProgress::Complete)));
    }

    #[test]
    fn a_quiet_recording_keeps_all_audio() {
        let (_writer, reader, mut recording) = quiet_listening(Duration::from_secs(3));

        recording
            .push(&silence(Duration::from_secs(1)))
            .expect("the lead-in is written");
        recording
            .push(&speech(Duration::from_secs(1)))
            .expect("speech is written");
        recording
            .push(&silence(Duration::from_secs(3)))
            .expect("the quiet tail is written");
        recording.limited.finish().expect("the recording finishes");

        assert_eq!(reader.duration(), Duration::from_secs(5));
    }
}
