//! The one WAV format this crate reads and writes: 16 kHz mono 16-bit PCM.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::recording::RecordedAudio;
use super::{
    CANONICAL_SAMPLE_RATE, CHANNELS, CaptureError, MAX_RECORDING_DURATION, RecordingReader,
    canonical_samples, f32_from_pcm16, pcm16_from_f32,
};

const BITS_PER_SAMPLE: u16 = 16;

type Result<T> = std::result::Result<T, WavError>;

pub(super) type WavFileReader = hound::WavReader<std::io::BufReader<std::fs::File>>;

pub(super) fn wav_spec() -> hound::WavSpec {
    hound::WavSpec {
        channels: CHANNELS,
        sample_rate: CANONICAL_SAMPLE_RATE,
        bits_per_sample: BITS_PER_SAMPLE,
        sample_format: hound::SampleFormat::Int,
    }
}

impl RecordingReader {
    /// Opens a caller-owned canonical 16 kHz mono WAV as a finished recording.
    ///
    /// The file is validated for format, not trust — trust policy is for
    /// artifacts a daemon loads on a user's behalf, and this file is the
    /// caller's own. The file is borrowed, never deleted.
    pub fn from_wav_file(path: impl AsRef<Path>) -> std::result::Result<Self, CaptureError> {
        let path = path.as_ref();
        let reader = open_canonical_wav(path)?;
        check_declared_length(&reader, path, MAX_RECORDING_DURATION)?;
        let samples = collect_samples(reader, path)?;

        let (recording, writer) = super::recording::ends();
        writer.begin();
        writer.push_pcm16(&samples);
        let sample_count = samples.len() as u64;
        writer.close(Ok(RecordedAudio::borrowed(
            path.to_path_buf(),
            sample_count,
        )));
        Ok(recording)
    }
}

/// Reads every sample of a canonical WAV. Unlike
/// [`RecordingReader::from_wav_file`], it sets no length limit.
pub fn read_wav(path: impl AsRef<Path>) -> Result<Vec<f32>> {
    let path = path.as_ref();
    let samples = collect_samples(open_canonical_wav(path)?, path)?;
    Ok(samples.into_iter().map(f32_from_pcm16).collect())
}

/// Writes `samples` to `path` as a canonical WAV, replacing any file there.
pub fn write_wav(path: impl AsRef<Path>, samples: &[f32]) -> Result<()> {
    let path = path.as_ref();
    let failed = |source: hound::Error| WavError::Write {
        path: path.to_path_buf(),
        source: Box::new(source),
    };
    let mut writer = hound::WavWriter::create(path, wav_spec()).map_err(failed)?;
    for &sample in samples {
        writer
            .write_sample(pcm16_from_f32(sample))
            .map_err(failed)?;
    }
    writer.finalize().map_err(failed)
}

/// Opens the WAV and rejects anything but the canonical 16 kHz mono format.
pub(super) fn open_canonical_wav(path: &Path) -> Result<WavFileReader> {
    let reader = hound::WavReader::open(path).map_err(|source| WavError::Open {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    if reader.spec() != wav_spec() {
        return Err(WavError::UnsupportedFormat {
            path: path.to_path_buf(),
        });
    }
    Ok(reader)
}

/// Rejects an empty or over-long file from its header, before reading any
/// samples.
pub(super) fn check_declared_length(
    reader: &WavFileReader,
    path: &Path,
    maximum_duration: Duration,
) -> Result<()> {
    let declared_samples = u64::from(reader.len());
    if declared_samples == 0 {
        return Err(WavError::Empty {
            path: path.to_path_buf(),
        });
    }
    if declared_samples > canonical_samples(maximum_duration) {
        return Err(WavError::TooLong {
            path: path.to_path_buf(),
            maximum_duration,
        });
    }
    Ok(())
}

pub(super) fn collect_samples(mut reader: WavFileReader, path: &Path) -> Result<Vec<i16>> {
    let samples: Vec<i16> = reader
        .samples()
        .collect::<std::result::Result<_, _>>()
        .map_err(|source| WavError::Read {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
    if samples.is_empty() {
        return Err(WavError::Empty {
            path: path.to_path_buf(),
        });
    }
    Ok(samples)
}

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("failed to open WAV file {}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("WAV file must be 16 kHz mono 16-bit PCM: {}", path.display())]
    UnsupportedFormat { path: PathBuf },

    #[error("failed to read WAV file {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("WAV file contains no samples: {}", path.display())]
    Empty { path: PathBuf },

    #[error("failed to write WAV file {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(
        "WAV file is longer than the recording limit {maximum_duration:?}: {}",
        path.display()
    )]
    TooLong {
        path: PathBuf,
        maximum_duration: Duration,
    },
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::testing::{FIXTURE_SAMPLES, canonical_fixture, fixture_of, write_fixture};
    use super::*;

    #[test]
    fn a_written_wav_reads_back() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let path = directory.path().join("written.wav");
        let samples = [0.0, 0.5, -0.5, 0.25];

        write_wav(&path, &samples).expect("the WAV is writable");

        assert_eq!(read_wav(&path).expect("the WAV is readable"), samples);
    }

    #[test]
    fn a_canonical_file_becomes_a_finished_recording() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = canonical_fixture(directory.path());

        let mut recording =
            RecordingReader::from_wav_file(&fixture).expect("the fixture is canonical");

        recording.fill().expect("the recording already ended");
        assert_eq!(recording.samples().len(), FIXTURE_SAMPLES);
        let audio = recording
            .finished_audio()
            .expect("the recording already ended");
        assert_eq!(audio.path(), Some(fixture.as_path()));
        assert_eq!(audio.duration(), Duration::from_secs(1));
    }

    #[test]
    fn an_empty_file_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let fixture = fixture_of(directory.path(), 0);

        let outcome = RecordingReader::from_wav_file(&fixture);

        assert!(matches!(
            outcome,
            Err(CaptureError::Wav(WavError::Empty { .. }))
        ));
    }

    /// The header alone declares the length, so a file over the limit is
    /// refused before any sample is read.
    #[test]
    fn a_file_over_the_recording_limit_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let path = directory.path().join("huge.wav");
        let over_limit_samples: u32 = 15 * 60 * 16_000 + 1;
        let data_bytes = over_limit_samples * 2;
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        header.extend_from_slice(b"WAVEfmt ");
        header.extend_from_slice(&16_u32.to_le_bytes());
        header.extend_from_slice(&1_u16.to_le_bytes()); // PCM
        header.extend_from_slice(&1_u16.to_le_bytes()); // mono
        header.extend_from_slice(&16_000_u32.to_le_bytes());
        header.extend_from_slice(&32_000_u32.to_le_bytes()); // byte rate
        header.extend_from_slice(&2_u16.to_le_bytes()); // block align
        header.extend_from_slice(&16_u16.to_le_bytes()); // bits
        header.extend_from_slice(b"data");
        header.extend_from_slice(&data_bytes.to_le_bytes());
        std::fs::write(&path, header).expect("the header must be writable");

        let outcome = RecordingReader::from_wav_file(&path);

        assert!(matches!(
            outcome,
            Err(CaptureError::Wav(WavError::TooLong { .. }))
        ));
    }

    #[test]
    fn a_non_canonical_file_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let fixture = write_fixture(directory.path(), spec, 100);

        let outcome = RecordingReader::from_wav_file(&fixture);

        assert!(matches!(
            outcome,
            Err(CaptureError::Wav(WavError::UnsupportedFormat { .. }))
        ));
    }
}
