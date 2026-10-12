//! Every measurement leaves a row behind, so a result can be checked.

use std::io::BufWriter;
use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Phase {
    Latency,
    Quality,
}

/// What one synthesis attempt cost. `ttfa` is time to first audio; `rtf`
/// relates wall clock to the audio produced, so 1.0 is real-time speech.
#[derive(Debug, Serialize)]
pub(crate) struct LatencyRow {
    pub phase: Phase,
    pub backend: &'static str,
    pub utterance: usize,
    pub run: usize,
    pub ttfa_ms: Option<u128>,
    pub wall_ms: u128,
    pub audio_ms: u128,
    pub rtf: Option<f64>,
    pub error: Option<String>,
}

/// What an ASR judge heard back from one synthesized utterance. The counts
/// are word errors against the utterance's own text.
#[derive(Debug, Serialize)]
pub(crate) struct QualityRow {
    pub phase: Phase,
    pub backend: &'static str,
    pub utterance: usize,
    pub reference_words: usize,
    pub substitutions: usize,
    pub deletions: usize,
    pub insertions: usize,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecordError {
    #[error("failed to create {path}")]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to serialize a result row")]
    Serialize(#[source] serde_json::Error),

    #[error("failed to write a result row")]
    Write(#[source] std::io::Error),
}

pub(crate) struct RowWriter {
    file: BufWriter<std::fs::File>,
}

impl RowWriter {
    pub(crate) fn create(path: &Path) -> Result<Self, RecordError> {
        let file = std::fs::File::create(path).map_err(|source| RecordError::Create {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            file: BufWriter::new(file),
        })
    }

    pub(crate) fn row(&mut self, row: &impl Serialize) -> Result<(), RecordError> {
        use std::io::Write;
        serde_json::to_writer(&mut self.file, row).map_err(RecordError::Serialize)?;
        writeln!(self.file).map_err(RecordError::Write)
    }

    pub(crate) fn finish(mut self) -> Result<(), RecordError> {
        use std::io::Write;
        // A dropped writer swallows the error, and a truncated result file
        // must not look like a finished run.
        self.file.flush().map_err(RecordError::Write)
    }
}
