//! The temporary WAV that capture writes, and cleanup of ones left behind.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tempfile::{Builder, TempPath};

use super::recording::{RecordedAudio, Recording, RecordingWriter};
use super::wav::wav_spec;
use super::{CANONICAL_SAMPLE_RATE, MAX_RECORDING_DURATION, canonical_samples};

const FILE_PREFIX: &str = "aria-recording-";
const FILE_SUFFIX: &str = ".wav";
const MAX_CLEANUP_ENTRIES: usize = 256;
const MIN_RECORDING_DURATION: Duration =
    Duration::from_nanos(1_000_000_000 / CANONICAL_SAMPLE_RATE as u64);

type WavWriter = hound::WavWriter<BufWriter<File>>;
type Result<T> = std::result::Result<T, SpoolError>;

/// An unfinished, bounded 16 kHz mono PCM WAV.
///
/// Every sample reaches the [`Recording`] in the order the WAV gets it, so a
/// reader following from the start and a reader opening the finished file
/// see the same audio.
pub(super) struct WavSpool {
    writer: WavWriter,
    recording: Arc<Recording>,
    path: TempPath,
    samples_written: u64,
    maximum_samples: u64,
}

impl WavSpool {
    /// Creates a private temporary WAV inside `directory`.
    pub(super) fn create(
        directory: impl AsRef<Path>,
        maximum_duration: Duration,
        writer: &RecordingWriter,
    ) -> Result<Self> {
        let maximum_samples = maximum_samples(maximum_duration)?;
        let directory = directory.as_ref();
        let prefix = format!("{FILE_PREFIX}{}-", std::process::id());
        let temporary = Builder::new()
            .prefix(&prefix)
            .suffix(FILE_SUFFIX)
            .tempfile_in(directory)
            .map_err(|source| SpoolError::CreateTemporary {
                directory: directory.to_path_buf(),
                source,
            })?;
        let (file, path) = temporary.into_parts();
        let wav = hound::WavWriter::new(BufWriter::new(file), wav_spec())
            .map_err(|source| SpoolError::Initialize(Box::new(source)))?;

        Ok(Self {
            writer: wav,
            recording: writer.recording(),
            path,
            samples_written: 0,
            maximum_samples,
        })
    }

    /// Appends samples to the WAV and the recording. Fails once the maximum
    /// length would be exceeded.
    pub(super) fn write_samples(&mut self, samples: &[i16]) -> Result<()> {
        let sample_count =
            u64::try_from(samples.len()).map_err(|_error| SpoolError::SampleCountOverflow)?;
        let attempted_samples = self
            .samples_written
            .checked_add(sample_count)
            .ok_or(SpoolError::SampleCountOverflow)?;
        if attempted_samples > self.maximum_samples {
            return Err(SpoolError::LimitExceeded {
                attempted_samples,
                maximum_samples: self.maximum_samples,
            });
        }

        for &sample in samples {
            self.writer
                .write_sample(sample)
                .map_err(|source| SpoolError::Write(Box::new(source)))?;
        }
        self.samples_written = attempted_samples;
        self.recording.append(samples);
        Ok(())
    }

    /// Finalizes the WAV header and transfers deletion ownership to the result.
    pub(super) fn finish(self) -> Result<RecordedAudio> {
        if self.samples_written == 0 {
            return Err(SpoolError::Empty);
        }

        let Self {
            writer,
            path,
            samples_written,
            ..
        } = self;
        writer
            .finalize()
            .map_err(|source| SpoolError::Finalize(Box::new(source)))?;

        Ok(RecordedAudio::owned(path, samples_written))
    }
}

// Best-effort: failures are logged, never returned, so cleanup cannot block
// startup. The caller must hold the daemon's exclusive lock, because every
// matching WAV in `directory` is deleted.
pub fn remove_all_spools(directory: &Path) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                directory = %directory.display(),
                %error,
                "cannot list temporary recordings"
            );
            return;
        }
    };

    let mut removed = 0;
    for (scanned, entry) in entries.enumerate() {
        if scanned == MAX_CLEANUP_ENTRIES {
            tracing::warn!(
                scanned,
                "temporary recording cleanup reached its scan limit"
            );
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "cannot read a temporary recording entry");
                continue;
            }
        };
        if !is_recording_artifact(&entry.file_name()) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %entry.path().display(),
                %error,
                "cannot remove an abandoned temporary recording"
            ),
        }
    }

    if removed > 0 {
        tracing::info!(removed, "removed abandoned temporary recordings");
    }
}

// The spool directory is user-configurable, so cleanup removes only files
// named like this crate's spools.
fn is_recording_artifact(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX))
}

pub(in crate::audio) fn maximum_samples(duration: Duration) -> Result<u64> {
    if !(MIN_RECORDING_DURATION..=MAX_RECORDING_DURATION).contains(&duration) {
        return Err(SpoolError::InvalidDuration {
            actual: duration,
            minimum: MIN_RECORDING_DURATION,
            maximum: MAX_RECORDING_DURATION,
        });
    }

    Ok(canonical_samples(duration))
}

#[derive(Debug, thiserror::Error)]
pub enum SpoolError {
    #[error("recording duration must be between {minimum:?} and {maximum:?}; got {actual:?}")]
    InvalidDuration {
        actual: Duration,
        minimum: Duration,
        maximum: Duration,
    },

    #[error("failed to create a temporary recording in {}", directory.display())]
    CreateTemporary {
        directory: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to initialize the temporary WAV recording")]
    Initialize(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error(
        "recording would contain {attempted_samples} samples; maximum is {maximum_samples} samples"
    )]
    LimitExceeded {
        attempted_samples: u64,
        maximum_samples: u64,
    },

    #[error("recording sample count overflowed")]
    SampleCountOverflow,

    #[error("failed to write PCM samples to the temporary recording")]
    Write(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("cannot finalize an empty recording")]
    Empty,

    #[error("failed to finalize the temporary recording")]
    Finalize(#[source] Box<dyn std::error::Error + Send + Sync>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plant(directory: &Path, name: &str) {
        std::fs::write(directory.join(name), []).expect("fixture must be writable");
    }

    #[test]
    fn cleanup_removes_only_recording_artifacts() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        plant(directory.path(), "aria-recording-999999-abc.wav");
        plant(directory.path(), "aria-recording-notapid.wav");
        plant(directory.path(), "keep-me.txt");

        remove_all_spools(directory.path());

        assert!(directory.path().join("keep-me.txt").exists());
        assert!(
            !directory
                .path()
                .join("aria-recording-999999-abc.wav")
                .exists()
        );
        assert!(!directory.path().join("aria-recording-notapid.wav").exists());
    }

    #[test]
    fn cleanup_continues_past_an_unremovable_entry() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        std::fs::create_dir(directory.path().join("aria-recording-666-trap.wav"))
            .expect("trap must be creatable");
        plant(directory.path(), "aria-recording-1-orphan.wav");

        remove_all_spools(directory.path());

        assert!(
            !directory
                .path()
                .join("aria-recording-1-orphan.wav")
                .exists()
        );
        assert!(
            directory
                .path()
                .join("aria-recording-666-trap.wav")
                .exists()
        );
    }
}
