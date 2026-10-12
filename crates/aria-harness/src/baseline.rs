//! A recorded run that later runs are judged against.
//!
//! Comparisons require matching conditions. Baseline files can be checked in
//! with the code that produced them.

use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Bumped when a field changes meaning, so an old file is re-recorded rather
/// than misread.
const SCHEMA_VERSION: u32 = 1;

/// One run's summary, small enough to check in. `C` is what must match
/// before two runs are comparable; `M` is what one backend measured.
#[derive(Serialize, Deserialize)]
pub struct Baseline<C, M> {
    schema: u32,
    conditions: C,
    backends: Vec<Entry<M>>,
}

/// Read first to distinguish an unsupported schema from invalid data.
#[derive(Deserialize)]
struct Header {
    schema: u32,
}

#[derive(Serialize, Deserialize)]
pub struct Entry<M> {
    pub backend: String,
    #[serde(flatten)]
    pub measured: M,
}

/// One backend both runs measured, side by side.
pub struct Movement<'a, M> {
    pub backend: &'a str,
    pub recorded: &'a M,
    pub run: &'a M,
}

/// Classifies non-overlapping intervals as a change; lower values are better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Regressed,
    Improved,
    /// Intervals overlap, or one side has none to compare.
    Inconclusive,
}

impl Standing {
    pub fn of(baseline: Option<(f64, f64)>, run: Option<(f64, f64)>) -> Self {
        match (baseline, run) {
            (Some((_, upper)), Some((lower, _))) if lower > upper => Self::Regressed,
            (Some((lower, _)), Some((_, upper))) if upper < lower => Self::Improved,
            _ => Self::Inconclusive,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Regressed => "REGRESSED",
            Self::Improved => "improved",
            Self::Inconclusive => "inconclusive",
        }
    }
}

impl<C: PartialEq + Debug, M> Baseline<C, M> {
    pub fn new(conditions: C, backends: Vec<Entry<M>>) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            conditions,
            backends,
        }
    }

    /// Compares only backends measured in both runs.
    pub fn against<'a>(
        &'a self,
        baseline: &'a Self,
    ) -> Result<Vec<Movement<'a, M>>, BaselineError> {
        if self.conditions != baseline.conditions {
            return Err(BaselineError::Conditions {
                baseline: format!("{:?}", baseline.conditions),
                run: format!("{:?}", self.conditions),
            });
        }
        Ok(self
            .backends
            .iter()
            .filter_map(|entry| {
                let recorded = baseline
                    .backends
                    .iter()
                    .find(|candidate| candidate.backend == entry.backend)?;
                Some(Movement {
                    backend: &entry.backend,
                    recorded: &recorded.measured,
                    run: &entry.measured,
                })
            })
            .collect())
    }
}

impl<C: Serialize + DeserializeOwned, M: Serialize + DeserializeOwned> Baseline<C, M> {
    /// Returns `None` when the baseline file is missing.
    pub fn load(path: &Path) -> Result<Option<Self>, BaselineError> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(BaselineError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let parse = |source| BaselineError::Parse {
            path: path.to_owned(),
            source,
        };
        let header: Header = serde_json::from_str(&raw).map_err(parse)?;
        if header.schema != SCHEMA_VERSION {
            return Err(BaselineError::Schema {
                path: path.to_owned(),
            });
        }
        serde_json::from_str(&raw).map(Some).map_err(parse)
    }

    pub fn save(&self, path: &Path) -> Result<(), BaselineError> {
        let encoded = serde_json::to_string_pretty(self).map_err(BaselineError::Encode)?;
        fs::write(path, encoded + "\n").map_err(|source| BaselineError::Write {
            path: path.to_owned(),
            source,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    #[error("failed to read the baseline {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the baseline {} could not be decoded", path.display())]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("the baseline {} predates this tool; re-record it", path.display())]
    Schema { path: PathBuf },

    #[error(
        "the baseline was recorded under {baseline}, this run is {run}; \
         match the conditions or re-record it"
    )]
    Conditions { baseline: String, run: String },

    #[error("failed to write the baseline {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to encode the baseline")]
    Encode(#[source] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Runs(u32);

    #[derive(Serialize, Deserialize)]
    struct Score {
        score: f64,
        ci: Option<(f64, f64)>,
    }

    fn baseline(runs: u32, score: f64) -> Baseline<Runs, Score> {
        Baseline::new(
            Runs(runs),
            vec![Entry {
                backend: "parakeet".to_owned(),
                measured: Score {
                    score,
                    ci: Some((score - 0.5, score + 0.5)),
                },
            }],
        )
    }

    #[test]
    fn differing_conditions_refuse_to_compare() {
        assert!(matches!(
            baseline(10, 1.0).against(&baseline(5, 1.0)),
            Err(BaselineError::Conditions { .. })
        ));
    }

    #[test]
    fn a_backend_only_one_side_ran_is_left_out() {
        let run = baseline(10, 1.0);
        let recorded = Baseline::<Runs, Score>::new(Runs(10), vec![]);
        assert!(run.against(&recorded).expect("comparable").is_empty());
    }

    #[test]
    fn a_disjoint_interval_above_the_baseline_regresses() {
        let standing = Standing::of(Some((0.015, 0.03)), Some((0.04, 0.06)));
        assert_eq!(standing, Standing::Regressed);
    }

    #[test]
    fn a_disjoint_interval_below_the_baseline_improves() {
        let standing = Standing::of(Some((0.015, 0.03)), Some((0.005, 0.014)));
        assert_eq!(standing, Standing::Improved);
    }

    #[test]
    fn overlapping_intervals_are_inconclusive() {
        let standing = Standing::of(Some((0.015, 0.03)), Some((0.02, 0.035)));
        assert_eq!(standing, Standing::Inconclusive);
    }

    #[test]
    fn a_missing_interval_is_inconclusive() {
        let standing = Standing::of(Some((0.015, 0.03)), None);
        assert_eq!(standing, Standing::Inconclusive);
    }

    #[test]
    fn a_saved_baseline_loads_back_with_its_measurements_inline() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("baseline.json");
        baseline(10, 2.0).save(&path).expect("saves");

        let raw = fs::read_to_string(&path).expect("written");
        assert!(raw.contains("\"score\": 2.0"), "{raw}");

        let loaded = Baseline::<Runs, Score>::load(&path)
            .expect("loads")
            .expect("present");
        assert_eq!(loaded.conditions, Runs(10));
        assert_eq!(loaded.backends[0].measured.ci, Some((1.5, 2.5)));
    }

    #[test]
    fn an_old_schema_is_told_apart_from_a_broken_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("baseline.json");
        fs::write(
            &path,
            r#"{"schema": 0, "conditions": "gone", "backends": 1}"#,
        )
        .expect("written");

        let loaded = Baseline::<Runs, Score>::load(&path);
        assert!(matches!(loaded, Err(BaselineError::Schema { .. })));
    }

    #[test]
    fn a_missing_file_is_no_baseline() {
        assert!(
            Baseline::<Runs, Score>::load(Path::new("/nonexistent/baseline.json"))
                .expect("not an error")
                .is_none()
        );
    }
}
