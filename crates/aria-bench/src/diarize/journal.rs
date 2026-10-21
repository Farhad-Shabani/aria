//! What the track believed after every chunk, written as it goes.
//!
//! A diarization error rate says how wrong a setting is. It does not say
//! which way: a track that splits one person in two and a track that fuses
//! two into one can score alike. These rows say which — how many people the track holds, how
//! long it takes to settle, and every time it changed its mind about a name
//! it had already written.
//!
//! One JSON object per line, so a sweep can read a run back without loading
//! it whole.

use std::io::{self, BufWriter, Write};
use std::path::Path;

use aria::speaker::{Label, Merged, Standing};
use serde::Serialize;

use crate::diarize::{BenchError, SAMPLE_RATE};

/// One chunk, after the track grouped it.
#[derive(Serialize)]
struct Row {
    /// Seconds into the recording where the chunk starts.
    at: f64,
    /// Voices the track is holding, including ones too rare to be written.
    voices: usize,
    /// Of those, the ones a reader would actually see.
    named: usize,
    /// Voices this chunk proved to be one, oldest name kept.
    merges: Vec<Merge>,
    /// Every voice by name, so a reader can follow one across the run.
    standing: Vec<Voice>,
}

#[derive(Serialize)]
struct Merge {
    from: String,
    into: String,
    /// Seconds the losing name stood before this correction reached it —
    /// the wait a reader of a live transcript actually sees. Zero for a
    /// voice opened and retired inside one chunk, which nothing ever read.
    stood_for: f64,
}

#[derive(Serialize)]
struct Voice {
    name: String,
    voiceprints: usize,
    eligible: bool,
}

/// Records a run chunk by chunk. Lines are flushed as they are written, so
/// a run killed part way still leaves a readable journal.
pub(crate) struct Journal {
    out: BufWriter<std::fs::File>,
    /// When each name was first heard, in seconds, so a merge can say how
    /// long the name it retires had been standing.
    opened: Vec<(Label, f64)>,
}

impl Journal {
    pub(crate) fn create(path: &Path) -> Result<Self, BenchError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(BenchError::Cache)?;
        }
        let file = std::fs::File::create(path).map_err(BenchError::Cache)?;
        Ok(Self {
            out: BufWriter::new(file),
            opened: Vec::new(),
        })
    }

    /// Writes one chunk's outcome.
    pub(crate) fn chunk(
        &mut self,
        at: u64,
        merges: &[Merged],
        standing: &[Standing],
    ) -> Result<(), BenchError> {
        let at = at as f64 / SAMPLE_RATE;
        for voice in standing {
            if !self.opened.iter().any(|(label, _)| label == &voice.label) {
                self.opened.push((voice.label.clone(), at));
            }
        }
        let row = Row {
            at,
            voices: standing.len(),
            named: standing.iter().filter(|voice| voice.eligible).count(),
            merges: merges
                .iter()
                .map(|merged| Merge {
                    from: merged.from.to_string(),
                    into: merged.into.to_string(),
                    stood_for: at - self.first_heard(&merged.from).unwrap_or(at),
                })
                .collect(),
            standing: standing
                .iter()
                .map(|voice| Voice {
                    name: voice.label.to_string(),
                    voiceprints: voice.voiceprints,
                    eligible: voice.eligible,
                })
                .collect(),
        };
        self.write(&row)
    }

    /// When a name was first heard, and `None` for one this journal never
    /// saw stand: a voice opened and merged away inside a single chunk is
    /// gone from the standing list before the chunk is written.
    fn first_heard(&self, label: &Label) -> Option<f64> {
        self.opened
            .iter()
            .find(|(opened, _)| opened == label)
            .map(|(_, at)| *at)
    }

    fn write(&mut self, row: &Row) -> Result<(), BenchError> {
        let line = serde_json::to_string(row)
            .map_err(|error| BenchError::Cache(io::Error::other(error)))?;
        writeln!(self.out, "{line}").map_err(BenchError::Cache)?;
        self.out.flush().map_err(BenchError::Cache)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn standing(numbers: &[(u32, usize, bool)]) -> Vec<Standing> {
        numbers
            .iter()
            .map(|&(number, voiceprints, eligible)| Standing {
                label: Label::Anonymous(number),
                voiceprints,
                eligible,
            })
            .collect()
    }

    fn rows(text: &str) -> Vec<serde_json::Value> {
        text.lines()
            .map(|line| serde_json::from_str(line).expect("each line is one object"))
            .collect()
    }

    #[test]
    fn a_merge_reports_how_long_the_retired_name_stood() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("run.jsonl");
        let mut journal = Journal::create(&path).expect("creating");

        let rate = SAMPLE_RATE as u64;
        journal
            .chunk(0, &[], &standing(&[(1, 12, true)]))
            .expect("the first chunk");
        journal
            .chunk(10 * rate, &[], &standing(&[(1, 20, true), (2, 3, false)]))
            .expect("the second chunk");
        journal
            .chunk(
                30 * rate,
                &[Merged {
                    from: Label::Anonymous(2),
                    into: Label::Anonymous(1),
                }],
                &standing(&[(1, 25, true)]),
            )
            .expect("the third chunk");

        let text = std::fs::read_to_string(&path).expect("the journal is readable");
        let rows = rows(&text);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["voices"], 2, "a rare voice still counts as held");
        assert_eq!(rows[1]["named"], 1, "but a reader never sees it");
        assert_eq!(
            rows[2]["merges"][0]["stood_for"], 20.0,
            "Speaker 2 opened at 10 s and was retired at 30 s"
        );
    }

    #[test]
    fn a_voice_opened_and_retired_in_one_chunk_never_stood() {
        // It was gone before the chunk was written, so no reader ever saw
        // the name. Dating it from the run's start would read as a long wait
        // for a correction nobody was waiting on.
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("run.jsonl");
        let mut journal = Journal::create(&path).expect("creating");

        journal
            .chunk(
                40 * SAMPLE_RATE as u64,
                &[Merged {
                    from: Label::Anonymous(9),
                    into: Label::Anonymous(1),
                }],
                &standing(&[(1, 30, true)]),
            )
            .expect("the chunk");

        let text = std::fs::read_to_string(&path).expect("the journal is readable");
        assert_eq!(rows(&text)[0]["merges"][0]["stood_for"], 0.0);
    }
}
