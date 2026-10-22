//! The per-utterance result rows a run leaves behind.
//!
//! Every number the benchmark reports is derived from these rows. A finished
//! run can then be re-scored or sliced by speaker without transcribing
//! anything again.

use std::fs::{File, Permissions};
use std::io::{BufWriter, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use aria::primitives::private_fs::PRIVATE_FILE_MODE;
use rustix::fs::{Mode, OFlags};
use serde::Serialize;

use super::corpus::Utterance;
use super::{Conditions, Heard, normalize, score};

/// Bumped when a field changes meaning, so old rows are never read as new
/// ones.
pub(super) const SCHEMA_VERSION: u32 = 8;

#[derive(Debug, Clone, Serialize)]
pub(super) struct Segment {
    pub(super) text: String,
    pub(super) span_samples: Option<std::ops::Range<u64>>,
    pub(super) emitted_ms: u128,
    pub(super) available_samples: u64,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct Preview {
    pub(super) text: String,
    pub(super) emitted_ms: u128,
    pub(super) available_samples: u64,
    pub(super) finalized_segments: usize,
}

/// Which engine produced a transcript, and from which weights.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Descriptor {
    /// The backend name as configuration and logs spell it.
    pub(super) name: &'static str,
    /// A model's file or directory name, or the repository id the sidecar
    /// resolves from its cache.
    pub(super) model: String,
    pub(super) device: Option<&'static str>,
    /// Total size of the model on disk. Weights that differ in size are
    /// certainly not the same weights; this is not a content hash.
    pub(super) model_bytes: Option<u64>,
}

/// One backend's attempt at one utterance.
#[derive(Debug, Serialize)]
pub(super) struct Record {
    pub(super) schema: u32,
    /// Groups every row produced by the same invocation.
    pub(super) run_id: String,
    /// Which hearing of the corpus this row came from, counted from one.
    pub(super) pass: u32,
    pub(super) backend: Descriptor,
    pub(super) utterance: String,
    pub(super) reference: String,
    /// Empty when the backend heard no speech, which scores as deletions.
    pub(super) hypothesis: String,
    pub(super) normalized_reference: String,
    pub(super) normalized_hypothesis: String,
    pub(super) reference_words: usize,
    pub(super) substitutions: usize,
    pub(super) deletions: usize,
    pub(super) insertions: usize,
    /// Set when the backend failed instead of transcribing. The row still
    /// carries the reference, so a failure counts as words missed rather than
    /// quietly shrinking the corpus.
    pub(super) error: Option<String>,
    pub(super) audio_ms: u128,
    pub(super) transcribe_ms: u128,
    /// Engine decode time, excluding the wait for replayed audio.
    pub(super) decode_ms: u128,
    pub(super) conditions: Conditions,
    pub(super) segments: Vec<Segment>,
    pub(super) previews: Vec<Preview>,
    pub(super) decode_calls: u64,
}

impl Record {
    /// Scores one attempt against its reference and assembles the row.
    ///
    /// Both sides are normalized the same way before they are compared, which
    /// is the only reason `TWENTY TWO` and `22` count as agreement.
    pub(super) fn score(
        heard: &Heard,
        utterance: &Utterance,
        backend: Descriptor,
        run_id: &str,
        pass: u32,
        conditions: Conditions,
    ) -> Self {
        let reference = normalize::words(&utterance.reference);
        let hypothesis = normalize::words(&heard.hypothesis);
        let alignment = score::align(&reference, &hypothesis);

        Self {
            schema: SCHEMA_VERSION,
            run_id: run_id.to_owned(),
            pass,
            backend,
            utterance: utterance.id.clone(),
            reference: utterance.reference.clone(),
            hypothesis: heard.hypothesis.clone(),
            normalized_reference: reference.join(" "),
            normalized_hypothesis: hypothesis.join(" "),
            reference_words: reference.len(),
            substitutions: alignment.substitutions,
            deletions: alignment.deletions,
            insertions: alignment.insertions,
            error: heard.error.clone(),
            audio_ms: heard.audio_ms,
            transcribe_ms: heard.transcribe_ms,
            decode_ms: heard.decode_ms,
            conditions,
            segments: heard.segments.clone(),
            previews: heard.previews.clone(),
            decode_calls: heard.decode_calls,
        }
    }
}

pub(super) struct RecordWriter {
    path: PathBuf,
    rows: BufWriter<File>,
}

impl RecordWriter {
    /// Every row carries a reference and a transcript. A corpus of the user's
    /// own speech makes those personal content, so the file is private from
    /// the moment it exists, and a symlink in its place is refused rather than
    /// followed.
    pub(super) fn create(path: impl Into<PathBuf>) -> Result<Self, RecordError> {
        let path = path.into();
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let descriptor =
            rustix::fs::open(&path, flags, Mode::RUSR | Mode::WUSR).map_err(|source| {
                RecordError::Create {
                    path: path.clone(),
                    source: source.into(),
                }
            })?;

        let file = File::from(descriptor);
        file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
            .map_err(|source| RecordError::Secure {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            path,
            rows: BufWriter::new(file),
        })
    }

    pub(super) fn write(&mut self, record: &Record) -> Result<(), RecordError> {
        let row = serde_json::to_string(record).map_err(RecordError::Encode)?;
        writeln!(self.rows, "{row}").map_err(|source| RecordError::Write {
            path: self.path.clone(),
            source,
        })
    }

    /// Flushes explicitly, because a dropped writer swallows the error and a
    /// truncated result file must not look like a finished run.
    pub(super) fn finish(mut self) -> Result<PathBuf, RecordError> {
        self.rows.flush().map_err(|source| RecordError::Write {
            path: self.path.clone(),
            source,
        })?;
        Ok(self.path)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("failed to create the result file {}", path.display())]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to secure the result file {}", path.display())]
    Secure {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write to the result file {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to encode a result row")]
    Encode(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::Cadence;
    use crate::stt::conditions::Decoding;
    use serde_json::json;

    #[test]
    fn result_rows_record_effective_conditions_even_when_replay_fails() {
        let descriptor = Descriptor {
            name: "fake",
            model: "fixture".into(),
            device: None,
            model_bytes: None,
        };
        let utterance = Utterance {
            id: "fixture".into(),
            audio: "unused.wav".into(),
            reference: "two words".into(),
        };
        let agreement = Decoding::agreement(Cadence::from_millis(500, 20_000));
        for (decoding, realtime, expected) in [
            (
                agreement,
                false,
                json!({
                    "decoding": {
                        "mode": "agreement", "stride_samples": 8000, "max_buffer_samples": 320000
                    },
                    "replay": {"pace": "stepped", "stride_samples": 8000},
                    "previews": true
                }),
            ),
            (
                agreement,
                true,
                json!({
                    "decoding": {
                        "mode": "agreement", "stride_samples": 8000, "max_buffer_samples": 320000
                    },
                    "replay": {"pace": "realtime"},
                    "previews": true
                }),
            ),
            (
                Decoding::Whole,
                true,
                json!({
                    "decoding": {"mode": "whole"},
                    "replay": {"pace": "immediate"},
                    "previews": false
                }),
            ),
            (
                Decoding::ChunkedFile {
                    chunk_samples: 4000,
                },
                true,
                json!({
                    "decoding": {"mode": "chunked_file", "chunk_samples": 4000},
                    "replay": {"pace": "immediate"},
                    "previews": false
                }),
            ),
            (
                Decoding::Native {
                    chunk_samples: 12800,
                },
                false,
                json!({
                    "decoding": {"mode": "native", "chunk_samples": 12800},
                    "replay": {"pace": "stepped", "stride_samples": 12800},
                    "previews": false
                }),
            ),
            (
                Decoding::Native {
                    chunk_samples: 12800,
                },
                true,
                json!({
                    "decoding": {"mode": "native", "chunk_samples": 12800},
                    "replay": {"pace": "realtime"},
                    "previews": false
                }),
            ),
        ] {
            let conditions = Conditions::new(decoding, realtime, true);
            let row = Record::score(
                &Heard::failed("replay failed".into()),
                &utterance,
                descriptor.clone(),
                "test-run",
                1,
                conditions,
            );
            let row = serde_json::to_value(row).unwrap();
            assert_eq!(row["schema"], SCHEMA_VERSION);
            assert_eq!(row["conditions"], expected);
            assert_eq!(row["deletions"], 2);
            assert_eq!(row["error"], "replay failed");
        }
    }

    #[test]
    fn an_existing_artifact_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("results.jsonl");
        std::fs::write(&path, "existing result").unwrap();
        assert!(RecordWriter::create(&path).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "existing result");
    }
}
