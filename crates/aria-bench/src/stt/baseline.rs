//! The word error rate baseline: what `aria_harness::baseline` records for
//! this bench.

use std::path::Path;

use aria_harness::baseline::{self, Entry};
use serde::{Deserialize, Serialize};

pub use aria_harness::baseline::{BaselineError, Standing};

use super::Report;
use super::conditions::Conditions as ExecutionConditions;

pub struct Baseline(baseline::Baseline<Conditions, Wer>);

impl Baseline {
    pub fn load(path: &Path) -> Result<Option<Self>, BaselineError> {
        baseline::Baseline::load(path).map(|loaded| loaded.map(Self))
    }

    pub fn save(&self, path: &Path) -> Result<(), BaselineError> {
        self.0.save(path)
    }

    pub fn against<'a>(
        &'a self,
        recorded: &'a Self,
    ) -> Result<Vec<baseline::Movement<'a, Wer>>, BaselineError> {
        let movements = self.0.against(&recorded.0)?;
        for movement in &movements {
            if movement.run.execution != movement.recorded.execution {
                return Err(BaselineError::Conditions {
                    baseline: format!("{}: {:?}", movement.backend, movement.recorded.execution),
                    run: format!("{}: {:?}", movement.backend, movement.run.execution),
                });
            }
        }
        Ok(movements)
    }
}

/// What must match before two runs are comparable.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conditions {
    utterances: usize,
    reference_words: usize,
    passes: u32,
}

#[derive(Serialize, Deserialize)]
pub struct Wer {
    execution: ExecutionConditions,
    pub wer: f64,
    /// 95% bootstrap interval, absent when one row was all there was.
    pub ci: Option<(f64, f64)>,
}

pub fn capture(report: &Report) -> Baseline {
    let conditions = Conditions {
        utterances: report.corpus_utterances,
        reference_words: report.corpus_words,
        passes: report.passes,
    };
    let backends = report
        .summaries
        .iter()
        .map(|summary| Entry {
            backend: summary.backend.to_owned(),
            measured: Wer {
                execution: summary.conditions,
                wer: summary.word_error_rate(),
                ci: summary.wer_interval(),
            },
        })
        .collect();
    Baseline(baseline::Baseline::new(conditions, backends))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::conditions::Decoding;
    use crate::stt::record::Descriptor;
    use crate::stt::{Cadence, Summary};

    #[test]
    fn comparisons_use_the_reports_effective_execution_conditions() {
        let descriptor = Descriptor {
            name: "parakeet",
            model: "fixture".into(),
            device: Some("cpu"),
            model_bytes: None,
        };
        let decoding = Decoding::agreement(Cadence::from_millis(500, 20_000));
        let original = ExecutionConditions::new(decoding, false, true);
        let mut report = Report {
            results: "unused.jsonl".into(),
            corpus_utterances: 1,
            corpus_words: 2,
            summaries: vec![Summary::new(&descriptor, original)],
            skipped: Vec::new(),
            passes: 1,
        };
        let recorded = capture(&report);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("baseline.json");
        recorded.save(&path).unwrap();
        let recorded = Baseline::load(&path).unwrap().unwrap();
        assert_eq!(capture(&report).against(&recorded).unwrap().len(), 1);

        let other = Descriptor {
            name: "whisper",
            ..descriptor
        };
        report.summaries.push(Summary::new(
            &other,
            ExecutionConditions::new(Decoding::Whole, false, false),
        ));
        let extended = capture(&report);
        assert_eq!(extended.against(&recorded).unwrap().len(), 1);
        assert_eq!(recorded.against(&extended).unwrap().len(), 1);
        report.summaries.pop();

        for changed in [
            ExecutionConditions::new(decoding, true, true),
            ExecutionConditions::new(decoding, false, false),
            ExecutionConditions::new(
                Decoding::agreement(Cadence::from_millis(750, 20_000)),
                false,
                true,
            ),
            ExecutionConditions::new(
                Decoding::agreement(Cadence::from_millis(500, 10_000)),
                false,
                true,
            ),
            ExecutionConditions::new(
                Decoding::Native {
                    chunk_samples: 8_000,
                },
                false,
                true,
            ),
        ] {
            report.summaries[0].conditions = changed;
            assert!(
                matches!(
                    capture(&report).against(&recorded),
                    Err(BaselineError::Conditions { .. })
                ),
                "{changed:?}"
            );
        }
        report.summaries[0].conditions = original;
        report.passes = 2;
        assert!(matches!(
            capture(&report).against(&recorded),
            Err(BaselineError::Conditions { .. })
        ));
    }

    #[test]
    fn a_baseline_without_execution_conditions_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("baseline.json");
        let legacy = serde_json::json!({
            "schema": 1,
            "conditions": { "utterances": 1, "reference_words": 2, "passes": 1 },
            "backends": [{
                "backend": "parakeet",
                "wer": 0.0,
                "ci": null
            }]
        });
        std::fs::write(&path, legacy.to_string()).unwrap();
        assert!(matches!(
            Baseline::load(&path),
            Err(BaselineError::Parse { .. })
        ));
    }
}
