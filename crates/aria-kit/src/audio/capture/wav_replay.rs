//! WAV-replay capture for unattended benchmarking and testing.
//!
//! Replays one fixed recording instead of opening the microphone. `new`
//! validates the file and reads it into memory, so every recording replays
//! the same audio and a bad fixture fails before the first one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::StopSignal;
use crate::audio::limit::{LimitedWriter, maximum_samples};
use crate::audio::recording;
use crate::audio::wav::{
    WavFileReader, check_declared_length, collect_samples, open_canonical_wav,
};
use crate::audio::{RecordingReader, RecordingSource, canonical_duration, canonical_samples};
use crate::primitives::file_inspection::{InspectedFile, InspectionError};

use super::RecordingMode;
use super::{AudioCapture, CaptureError, Inputs};

type Result<T> = std::result::Result<T, WavReplayError>;

/// Short enough that a reader is never far behind the fixture's clock.
const PACED_CHUNK: Duration = Duration::from_millis(100);
const CHUNK_SAMPLES: usize = canonical_samples(PACED_CHUNK) as usize;

/// How a WAV replay delivers its samples. `Immediate` writes the whole file
/// before `start` returns. `Realtime` writes it at recorded speed, so
/// transcription overlaps capture as on a real press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayPace {
    Immediate,
    Realtime,
}

#[derive(Debug)]
pub struct WavReplayCapture {
    maximum_duration: Duration,
    pace: ReplayPace,
    samples: Arc<Vec<i16>>,
}

impl WavReplayCapture {
    pub fn new(
        maximum_duration: Duration,
        wav_path: &Path,
        pace: ReplayPace,
    ) -> std::result::Result<Self, CaptureError> {
        let samples = read_replay_samples(wav_path, maximum_duration)?;
        Ok(Self {
            maximum_duration,
            pace,
            samples: Arc::new(samples),
        })
    }
}

impl AudioCapture for WavReplayCapture {
    /// Replays the WAV into a new recording. A `Realtime` replay ends early
    /// when `stop` fires. `mode` is ignored: replay runs no voice-activity
    /// detection.
    fn start(
        &self,
        _mode: RecordingMode,
        _inputs: Inputs,
        stop: StopSignal,
    ) -> std::result::Result<RecordingReader, CaptureError> {
        let mut recording = match self.pace {
            ReplayPace::Immediate => RecordingReader::from_pcm16(&self.samples),
            ReplayPace::Realtime => {
                let (recording, writer) = recording::ends();
                let limited = LimitedWriter::new(self.maximum_duration, &writer)?;
                let samples = Arc::clone(&self.samples);
                writer.begin(RecordingSource::Prerecorded);
                thread::Builder::new()
                    .name("aria-wav-replay".to_owned())
                    .spawn(move || {
                        let result = replay_paced(limited, &samples, &stop);
                        // Drop `stop` before closing the writer, so state it
                        // owns is released before a reader sees capture end.
                        drop(stop);
                        writer.close(result);
                    })
                    .map_err(WavReplayError::SpawnReplay)?;
                recording
            }
        };
        recording.wait_for_capture()?;
        Ok(recording)
    }
}

impl WavReplayCapture {
    /// Runs the checks `new` runs on a fixture, without reading its samples.
    /// Returns the validated path.
    pub fn check(
        wav_path: &Path,
        maximum_duration: Duration,
    ) -> std::result::Result<PathBuf, CaptureError> {
        checked_reader(wav_path, maximum_duration).map(|(path, _unread)| path)
    }
}

/// Writes the samples to `limited` at recorded speed until they run out or
/// `stop` fires. Each chunk is due at a fixed offset from the start, so one
/// slow write does not delay every later chunk. That offset is where the
/// chunk's audio ends: a microphone cannot hand over a sample before it is
/// spoken.
fn replay_paced(
    mut limited: LimitedWriter,
    samples: &[i16],
    stop: &StopSignal,
) -> std::result::Result<(), CaptureError> {
    let started = Instant::now();
    let mut written = 0;
    for batch in samples.chunks(CHUNK_SAMPLES) {
        written += batch.len() as u64;
        let due = started + canonical_duration(written);
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        limited.push(batch)?;
        if stop.should_stop() {
            break;
        }
    }
    Ok(limited.finish()?)
}

#[derive(Debug, thiserror::Error)]
pub enum WavReplayError {
    #[error("the capture WAV file cannot be used")]
    UnusableFile(#[source] InspectionError),

    #[error("failed to start the paced replay thread")]
    SpawnReplay(#[source] std::io::Error),
}

/// Rejects fixtures another user could have planted or tampered with — the
/// same trust policy model files get.
fn validate_replay_file(path: &Path) -> Result<PathBuf> {
    InspectedFile::inspect(path.to_path_buf())
        .and_then(InspectedFile::require_regular_file)
        .and_then(InspectedFile::require_trusted)
        .map_err(WavReplayError::UnusableFile)
}

fn read_replay_samples(
    path: &Path,
    maximum_duration: Duration,
) -> std::result::Result<Vec<i16>, CaptureError> {
    let (path, reader) = checked_reader(path, maximum_duration)?;
    Ok(collect_samples(reader, &path)?)
}

/// Validates the fixture and returns the reader the checks ran on. Samples
/// are read from this same reader, so a file swapped in after the length
/// check is never read.
fn checked_reader(
    path: &Path,
    maximum_duration: Duration,
) -> std::result::Result<(PathBuf, WavFileReader), CaptureError> {
    maximum_samples(maximum_duration)?;
    let path = validate_replay_file(path)?;
    let reader = open_canonical_wav(&path)?;
    check_declared_length(&reader, &path, maximum_duration)?;
    Ok((path, reader))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::WavError;
    use crate::audio::testing::{FIXTURE_SAMPLES, canonical_fixture, write_fixture};
    use crate::audio::wav::wav_spec;

    const MAXIMUM_DURATION: Duration = Duration::from_secs(10);
    const PACE: ReplayPace = ReplayPace::Immediate;

    #[test]
    fn replays_the_fixture_as_recorded_audio() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());
        let capture =
            WavReplayCapture::new(MAXIMUM_DURATION, &fixture, PACE).expect("fixture must load");
        let mut recording = capture
            .start(
                RecordingMode::UntilStopped,
                Inputs::Local,
                StopSignal::never(),
            )
            .expect("replay must succeed");
        assert_eq!(recording.source(), Some(RecordingSource::Prerecorded));
        recording.fill().expect("the replay already ended");
        assert_eq!(recording.duration(), Duration::from_secs(1));
    }

    /// The point of a paced replay: a reader sees the opening while the rest
    /// is still arriving.
    #[test]
    fn a_paced_replay_arrives_while_the_reader_waits() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());
        let capture = WavReplayCapture::new(MAXIMUM_DURATION, &fixture, ReplayPace::Realtime)
            .expect("fixture must load");

        let mut recording = capture
            .start(
                RecordingMode::UntilStopped,
                Inputs::Local,
                StopSignal::never(),
            )
            .expect("replay must start");
        recording
            .fill_through(CHUNK_SAMPLES as u64)
            .expect("the first chunk arrives");
        let arriving = recording.reached_end();
        recording.fill().expect("the rest arrives");

        assert!(!arriving, "the replay must not be over after one chunk");
        assert_eq!(recording.samples().len(), FIXTURE_SAMPLES);
    }

    #[test]
    fn a_paced_chunk_arrives_once_its_audio_has_been_spoken() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());
        let capture = WavReplayCapture::new(MAXIMUM_DURATION, &fixture, ReplayPace::Realtime)
            .expect("fixture must load");

        let started = Instant::now();
        let mut recording = capture
            .start(
                RecordingMode::UntilStopped,
                Inputs::Local,
                StopSignal::never(),
            )
            .expect("replay must start");
        recording.fill_through(1).expect("the first chunk arrives");

        assert!(started.elapsed() >= PACED_CHUNK);
    }

    #[test]
    fn rejects_a_non_canonical_sample_rate() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let mut spec = wav_spec();
        spec.sample_rate = 44_100;
        let fixture = write_fixture(directory.path(), spec, FIXTURE_SAMPLES);
        let error = WavReplayCapture::new(MAXIMUM_DURATION, &fixture, PACE)
            .expect_err("a 44.1 kHz fixture must be rejected");
        assert!(matches!(
            error,
            CaptureError::Wav(WavError::UnsupportedFormat { .. })
        ));
    }

    #[test]
    fn rejects_a_fixture_writable_by_other_users() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o666))
            .expect("fixture permissions must be settable");
        let error = WavReplayCapture::new(MAXIMUM_DURATION, &fixture, PACE)
            .expect_err("a group/other-writable fixture must be rejected");
        assert!(matches!(
            error,
            CaptureError::WavReplay(WavReplayError::UnusableFile(
                InspectionError::WritableByOthers { .. }
            ))
        ));
    }

    #[test]
    fn rejects_a_relative_path() {
        let error = WavReplayCapture::new(MAXIMUM_DURATION, Path::new("fixture.wav"), PACE)
            .expect_err("a relative path must be rejected");
        assert!(matches!(
            error,
            CaptureError::WavReplay(WavReplayError::UnusableFile(
                InspectionError::NotAbsolute { .. }
            ))
        ));
    }

    #[test]
    fn rejects_a_fixture_longer_than_the_recording_timeout() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = write_fixture(directory.path(), wav_spec(), 2 * FIXTURE_SAMPLES);
        let error = WavReplayCapture::new(Duration::from_secs(1), &fixture, PACE)
            .expect_err("an over-long fixture must be rejected");
        assert!(matches!(error, CaptureError::Wav(WavError::TooLong { .. })));
    }
}
