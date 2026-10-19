//! WAV files. Recordings read only the canonical 16 kHz mono 16-bit format;
//! clips keep the file's own rate on read and write.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::recording::RecordedAudio;
use super::{
    CANONICAL_SAMPLE_RATE, CHANNELS, CaptureError, Clip, ClipError, MAX_RECORDING_DURATION,
    RecordingReader, canonical_samples, pcm16_from_f32,
};

const BITS_PER_SAMPLE: u16 = 16;

type Result<T> = std::result::Result<T, WavError>;

pub(super) type WavFileReader = hound::WavReader<std::io::BufReader<std::fs::File>>;

pub(super) fn wav_spec() -> hound::WavSpec {
    mono_spec(CANONICAL_SAMPLE_RATE)
}

fn mono_spec(sample_rate: u32) -> hound::WavSpec {
    hound::WavSpec {
        channels: CHANNELS,
        sample_rate,
        bits_per_sample: BITS_PER_SAMPLE,
        sample_format: hound::SampleFormat::Int,
    }
}

impl RecordingReader {
    /// Opens a caller-owned canonical 16 kHz mono WAV as a finished recording.
    ///
    /// The file is validated for format, not trust — trust policy is for
    /// artifacts a daemon loads on a user's behalf, and this file is the
    /// caller's own. The file is borrowed, never deleted, and read whole into
    /// memory: about 8 bytes per sample, near 460 MB per hour of audio.
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

/// Reads a PCM WAV, 8- to 32-bit integer or 32-bit float, at any rate a
/// [`Clip`] accepts, as mono audio at the file's rate. Channels are averaged.
/// Unlike [`RecordingReader::from_wav_file`], it sets no length limit.
pub fn read_wav(path: impl AsRef<Path>) -> Result<Clip> {
    let path = path.as_ref();
    let reader = hound::WavReader::open(path).map_err(|source| WavError::Open {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    let spec = reader.spec();
    let interleaved = decode(reader, path)?;
    if interleaved.is_empty() {
        return Err(WavError::Empty {
            path: path.to_path_buf(),
        });
    }
    let channels = usize::from(spec.channels);
    let mono = match channels {
        1 => interleaved,
        _ => interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() / channels as f32)
            .collect(),
    };
    Clip::new(spec.sample_rate, mono).map_err(|source| WavError::SampleRate {
        path: path.to_path_buf(),
        source,
    })
}

fn decode(reader: WavFileReader, path: &Path) -> Result<Vec<f32>> {
    let spec = reader.spec();
    let decoded: std::result::Result<Vec<f32>, hound::Error> =
        match (spec.sample_format, spec.bits_per_sample) {
            (hound::SampleFormat::Float, 32) => reader.into_samples::<f32>().collect(),
            (hound::SampleFormat::Int, bits @ 8..=32) => {
                let full_scale = 2_f32.powi(i32::from(bits) - 1);
                reader
                    .into_samples::<i32>()
                    .map(|sample| sample.map(|value| value as f32 / full_scale))
                    .collect()
            }
            _ => {
                return Err(WavError::UnsupportedEncoding {
                    path: path.to_path_buf(),
                });
            }
        };
    decoded.map_err(|source| WavError::Read {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// Writes `clip` to `path` as mono 16-bit PCM at its own rate, replacing any
/// file there.
pub fn write_wav(path: impl AsRef<Path>, clip: &Clip) -> Result<()> {
    let path = path.as_ref();
    let failed = |source: hound::Error| WavError::Write {
        path: path.to_path_buf(),
        source: Box::new(source),
    };
    let mut writer =
        hound::WavWriter::create(path, mono_spec(clip.sample_rate())).map_err(failed)?;
    for &sample in clip.samples() {
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

    #[error("WAV file is not integer PCM up to 32 bits or 32-bit float: {}", path.display())]
    UnsupportedEncoding { path: PathBuf },

    #[error("WAV file {}: {source}", path.display())]
    SampleRate {
        path: PathBuf,
        #[source]
        source: ClipError,
    },

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
        let clip = Clip::new(24_000, vec![0.0, 0.5, -0.5, 0.25]).expect("the rate is usable");

        write_wav(&path, &clip).expect("the WAV is writable");

        assert_eq!(read_wav(&path).expect("the WAV is readable"), clip);
    }

    #[test]
    fn a_stereo_24_bit_wav_reads_as_mono_at_its_own_rate() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let path = directory.path().join("stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("the WAV is writable");
        let half_scale = 1 << 22;
        for sample in [half_scale, 0, -half_scale, 0] {
            writer.write_sample(sample).expect("the sample is writable");
        }
        writer.finalize().expect("the WAV finalizes");

        let clip = read_wav(&path).expect("the WAV is readable");

        assert_eq!(clip.sample_rate(), 44_100);
        assert_eq!(clip.samples(), [0.25, -0.25]);
    }

    #[test]
    fn a_wav_declaring_an_absurd_rate_is_refused() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        let rate = 1_000_000_007;
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let fixture = write_fixture(directory.path(), spec, 1);

        let outcome = read_wav(&fixture);

        assert!(matches!(
            outcome,
            Err(WavError::SampleRate {
                source: ClipError::SampleRate(declared),
                ..
            }) if declared == rate
        ));
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
        let over_limit_samples =
            u32::try_from(crate::audio::MAX_RECORDING_SAMPLES + 1).expect("fits a header");
        let data_bytes = over_limit_samples * 2;
        let mut header = Vec::new();
        header.extend_from_slice(b"RIFF");
        // The true RIFF size, 36 plus the data bytes, overflows a `u32` here,
        // so the largest one stands in.
        header.extend_from_slice(&u32::MAX.to_le_bytes());
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
