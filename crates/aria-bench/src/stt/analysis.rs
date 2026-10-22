use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use aria_harness::artifact::{ArtifactError, Provenance, fingerprint, fingerprint_value};
use aria_harness::stats::{Decision, bootstrap, summarize};
use serde::{Deserialize, Serialize};

use super::conditions::{Conditions, Replay};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Engine {
    pub name: String,
    pub model_sha256: Option<String>,
    pub device: Option<String>,
    pub conditions: Conditions,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Manifest {
    pub schema: u32,
    pub provenance: Provenance,
    pub corpus_sha256: String,
    pub scorer_sha256: String,
    pub cases: BTreeMap<String, String>,
    pub engines: Vec<Engine>,
    pub language: String,
    pub timing_origin: String,
    pub passes: u32,
    pub provisional: bool,
    pub rows: usize,
    pub results_sha256: String,
}

pub fn manifest_path(results: &Path) -> PathBuf {
    let mut path = results.as_os_str().to_owned();
    path.push(".manifest.json");
    PathBuf::from(path)
}

pub(super) fn scorer_fingerprint() -> Result<String, ArtifactError> {
    fingerprint_value(&[
        include_str!(concat!(env!("OUT_DIR"), "/scorer-dependencies.txt")),
        include_str!("score.rs"),
        include_str!("normalize/mod.rs"),
        include_str!("normalize/lexicon.rs"),
        include_str!("normalize/numbers.rs"),
        include_str!("normalize/spelling.rs"),
        include_str!("normalize/symbols.rs"),
        include_str!("normalize/british-american-spelling.json"),
    ])
}

#[derive(Deserialize)]
struct Row {
    schema: u32,
    run_id: String,
    pass: u32,
    backend: RowBackend,
    utterance: String,
    normalized_reference: String,
    normalized_hypothesis: String,
    reference_words: usize,
    substitutions: usize,
    deletions: usize,
    insertions: usize,
    error: Option<String>,
    conditions: Conditions,
    decode_ms: u128,
    transcribe_ms: u128,
    decode_calls: u64,
    segments: Vec<Event>,
    previews: Vec<Event>,
}

#[derive(Deserialize)]
struct Event {
    text: String,
    emitted_ms: u64,
}

#[derive(Default)]
struct Timing {
    decode_ms: u128,
    transcribe_ms: u128,
    decode_calls: u64,
    finalized: Vec<u64>,
    previews: Vec<u64>,
}

#[derive(Deserialize)]
struct RowBackend {
    name: String,
}

#[derive(Default)]
struct Counts {
    words: usize,
    errors: usize,
    failures: usize,
    reference: String,
    /// Keyed by pass.
    hypotheses: BTreeMap<u32, String>,
}

pub struct Artifact {
    manifest: Manifest,
    clusters: BTreeMap<String, BTreeMap<String, Counts>>,
    timing: BTreeMap<String, Timing>,
}

pub struct Measurement {
    pub backend: String,
    pub wer: f64,
    pub interval: Option<(f64, f64)>,
    pub failures: usize,
    pub utterances: usize,
    pub passes: u32,
    pub decode_ms: u128,
    pub transcribe_ms: u128,
    pub decode_calls: u64,
    pub first_finalized_median_ms: Option<u64>,
    pub first_preview_median_ms: Option<u64>,
    pub finalized_attempts: usize,
    pub preview_attempts: usize,
    pub realtime: bool,
    pub timing_origin: String,
}

pub struct Comparison {
    pub backend: String,
    pub baseline_wer: f64,
    pub candidate_wer: f64,
    pub difference_interval: Option<(f64, f64)>,
    pub decision: Decision,
    pub changes: Vec<Change>,
}

/// An utterance whose set of distinct hypotheses differs between the runs.
/// Each side lists every pass.
pub struct Change {
    pub utterance: String,
    pub reference: String,
    pub baseline: Vec<(u32, String)>,
    pub candidate: Vec<(u32, String)>,
}

impl Artifact {
    pub fn load(path: &Path) -> Result<Self, AnalysisError> {
        let manifest: Manifest = serde_json::from_slice(&fs::read(manifest_path(path))?)?;
        if manifest.schema != super::record::SCHEMA_VERSION
            || manifest.passes == 0
            || manifest.cases.is_empty()
            || manifest.engines.is_empty()
        {
            return Err(AnalysisError::Invalid(
                "unsupported or empty manifest".to_owned(),
            ));
        }
        if fingerprint(path)? != manifest.results_sha256 {
            return Err(AnalysisError::Invalid(
                "result digest differs from completed manifest".to_owned(),
            ));
        }
        let expected = manifest
            .cases
            .len()
            .checked_mul(manifest.engines.len())
            .and_then(|count| count.checked_mul(manifest.passes as usize));
        if expected != Some(manifest.rows)
            || fingerprint_value(&manifest.cases)? != manifest.corpus_sha256
        {
            return Err(AnalysisError::Invalid(
                "manifest coverage is inconsistent".to_owned(),
            ));
        }
        let engines: BTreeMap<_, _> = manifest
            .engines
            .iter()
            .map(|engine| (engine.name.as_str(), engine))
            .collect();
        if engines.len() != manifest.engines.len() {
            return Err(AnalysisError::Invalid("duplicate engine".to_owned()));
        }
        let mut timing: BTreeMap<String, Timing> = BTreeMap::new();
        let mut seen = BTreeSet::new();
        let mut clusters: BTreeMap<String, BTreeMap<String, Counts>> = BTreeMap::new();
        for line in fs::read_to_string(path)?.lines() {
            let row: Row = serde_json::from_str(line)?;
            let engine = engines
                .get(row.backend.name.as_str())
                .ok_or_else(|| AnalysisError::Invalid("unexpected engine".to_owned()))?;
            if row.schema != manifest.schema
                || row.run_id != manifest.provenance.run_id
                || row.pass == 0
                || row.pass > manifest.passes
                || !manifest.cases.contains_key(&row.utterance)
                || row.reference_words == 0
                || row.conditions != engine.conditions
            {
                return Err(AnalysisError::Invalid(
                    "row identity differs from manifest".to_owned(),
                ));
            }
            if !seen.insert((row.backend.name.clone(), row.utterance.clone(), row.pass)) {
                return Err(AnalysisError::Invalid("duplicate result row".to_owned()));
            }
            let measured = timing.entry(row.backend.name.clone()).or_default();
            measured.decode_ms += row.decode_ms;
            measured.transcribe_ms += row.transcribe_ms;
            measured.decode_calls += row.decode_calls;
            if let Some(event) = row
                .segments
                .iter()
                .find(|event| !event.text.trim().is_empty())
            {
                measured.finalized.push(event.emitted_ms);
            }
            if let Some(event) = row
                .previews
                .iter()
                .find(|event| !event.text.trim().is_empty())
            {
                measured.previews.push(event.emitted_ms);
            }
            let count = clusters
                .entry(row.backend.name)
                .or_default()
                .entry(row.utterance)
                .or_default();
            count.words += row.reference_words;
            count.errors += row.substitutions + row.deletions + row.insertions;
            count.failures += usize::from(row.error.is_some());
            count.reference = row.normalized_reference;
            count.hypotheses.insert(row.pass, row.normalized_hypothesis);
        }
        if seen.len() != manifest.rows {
            return Err(AnalysisError::Invalid(
                "incomplete result coverage".to_owned(),
            ));
        }
        Ok(Self {
            manifest,
            clusters,
            timing,
        })
    }

    pub fn measurements(&self) -> Vec<Measurement> {
        self.clusters
            .iter()
            .map(|(backend, cases)| {
                let rows: Vec<_> = cases
                    .values()
                    .map(|counts| (counts.words, counts.errors))
                    .collect();
                let timing = &self.timing[backend];
                Measurement {
                    backend: backend.clone(),
                    wer: rate(&rows),
                    interval: super::stats::wer_interval(&rows),
                    failures: cases.values().map(|counts| counts.failures).sum(),
                    utterances: cases.len(),
                    passes: self.manifest.passes,
                    decode_ms: timing.decode_ms,
                    transcribe_ms: timing.transcribe_ms,
                    decode_calls: timing.decode_calls,
                    first_finalized_median_ms: summarize(&timing.finalized)
                        .map(|summary| summary.median),
                    first_preview_median_ms: summarize(&timing.previews)
                        .map(|summary| summary.median),
                    finalized_attempts: timing.finalized.len(),
                    preview_attempts: timing.previews.len(),
                    realtime: self.manifest.engines.iter().any(|engine| {
                        &engine.name == backend && engine.conditions.replay == Replay::Realtime
                    }),
                    timing_origin: self.manifest.timing_origin.clone(),
                }
            })
            .collect()
    }

    pub(super) fn validate_candidate(&self, candidate: &Manifest) -> Result<(), AnalysisError> {
        let baseline = &self.manifest;
        let differences: Vec<_> = [
            (
                "corpus",
                candidate.corpus_sha256 != baseline.corpus_sha256
                    || candidate.cases != baseline.cases,
            ),
            ("scorer", candidate.scorer_sha256 != baseline.scorer_sha256),
            (
                "engine model, device, mode, cadence, or pace",
                candidate.engines != baseline.engines,
            ),
            ("language", candidate.language != baseline.language),
            ("captions", candidate.provisional != baseline.provisional),
            (
                "build profile",
                candidate.provenance.debug_assertions != baseline.provenance.debug_assertions,
            ),
        ]
        .into_iter()
        .filter_map(|(name, differs)| differs.then_some(name))
        .collect();
        if !differences.is_empty() {
            return Err(AnalysisError::Invalid(format!(
                "incomparable runs: the {} differ",
                differences.join(", ")
            )));
        }
        if candidate
            .engines
            .iter()
            .any(|engine| engine.model_sha256.is_none())
        {
            return Err(AnalysisError::Invalid(
                "unresolved model or sidecar runtime identity; comparison is inconclusive"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    pub fn compare(&self, baseline: &Self, margin: f64) -> Result<Vec<Comparison>, AnalysisError> {
        if !margin.is_finite() || margin < 0.0 {
            return Err(AnalysisError::Invalid(
                "WER margin must be finite and nonnegative".to_owned(),
            ));
        }
        baseline.validate_candidate(&self.manifest)?;
        let mut comparisons = Vec::new();
        for (backend, cases) in &self.clusters {
            let reference = &baseline.clusters[backend];
            let rows: Vec<_> = cases
                .iter()
                .map(|(id, candidate)| {
                    let baseline = &reference[id];
                    (
                        candidate.words,
                        candidate.errors,
                        baseline.words,
                        baseline.errors,
                    )
                })
                .collect();
            if rows.iter().any(|&(cw, _, bw, _)| {
                cw / self.manifest.passes as usize != bw / baseline.manifest.passes as usize
            }) {
                return Err(AnalysisError::Invalid(
                    "reference word counts differ".to_owned(),
                ));
            }
            let failures = cases
                .values()
                .chain(reference.values())
                .any(|counts| counts.failures > 0);
            let interval = if failures {
                None
            } else {
                bootstrap(&rows, difference)
            };
            let changes = cases
                .iter()
                .filter(|(id, candidate)| heard(candidate) != heard(&reference[*id]))
                .map(|(id, candidate)| Change {
                    utterance: id.clone(),
                    reference: candidate.reference.clone(),
                    baseline: passes(&reference[id]),
                    candidate: passes(candidate),
                })
                .collect();
            let candidate_rows: Vec<_> = rows
                .iter()
                .map(|&(words, errors, _, _)| (words, errors))
                .collect();
            let baseline_rows: Vec<_> = rows
                .iter()
                .map(|&(_, _, words, errors)| (words, errors))
                .collect();
            comparisons.push(Comparison {
                backend: backend.clone(),
                baseline_wer: rate(&baseline_rows),
                candidate_wer: rate(&candidate_rows),
                difference_interval: interval,
                decision: Decision::of(interval, margin),
                changes,
            });
        }
        Ok(comparisons)
    }
}

/// A rejected run leaves the old reference in place. The two files are
/// replaced one at a time, so a crash between them leaves a pair the next
/// load refuses.
pub fn save_reference(results: &Path, destination: &Path) -> Result<(), AnalysisError> {
    let artifact = Artifact::load(results)?;
    artifact.validate_candidate(&artifact.manifest)?;
    let failed = artifact
        .clusters
        .values()
        .flat_map(BTreeMap::values)
        .filter(|counts| counts.failures > 0)
        .count();
    if failed > 0 {
        return Err(AnalysisError::Invalid(format!(
            "{failed} utterances failed; every comparison against them would be inconclusive"
        )));
    }
    let mut manifest = artifact.manifest;
    manifest.provenance.host = None;
    let directory = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(directory)?;
    let mut pending_rows = tempfile::NamedTempFile::new_in(directory)?;
    std::io::copy(&mut fs::File::open(results)?, &mut pending_rows)?;
    pending_rows.as_file().sync_all()?;
    if fingerprint(pending_rows.path())? != manifest.results_sha256 {
        return Err(AnalysisError::Invalid(
            "the results changed while being saved".to_owned(),
        ));
    }
    let mut encoded = serde_json::to_vec_pretty(&manifest)?;
    encoded.push(b'\n');
    let mut pending_manifest = tempfile::NamedTempFile::new_in(directory)?;
    pending_manifest.write_all(&encoded)?;
    pending_manifest.as_file().sync_all()?;
    pending_rows
        .persist(destination)
        .map_err(|error| error.error)?;
    pending_manifest
        .persist(manifest_path(destination))
        .map_err(|error| error.error)?;
    Ok(())
}

fn heard(counts: &Counts) -> BTreeSet<&str> {
    counts.hypotheses.values().map(String::as_str).collect()
}

fn passes(counts: &Counts) -> Vec<(u32, String)> {
    counts
        .hypotheses
        .iter()
        .map(|(pass, hypothesis)| (*pass, hypothesis.clone()))
        .collect()
}

fn rate(rows: &[(usize, usize)]) -> f64 {
    rows.iter().map(|row| row.1).sum::<usize>() as f64
        / rows.iter().map(|row| row.0).sum::<usize>() as f64
}

fn difference(rows: &[(usize, usize, usize, usize)]) -> f64 {
    let candidate_words: usize = rows.iter().map(|row| row.0).sum();
    let candidate_errors: usize = rows.iter().map(|row| row.1).sum();
    let baseline_words: usize = rows.iter().map(|row| row.2).sum();
    let baseline_errors: usize = rows.iter().map(|row| row.3).sum();
    candidate_errors as f64 / candidate_words as f64
        - baseline_errors as f64 / baseline_words as f64
}

#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Artifact(#[from] ArtifactError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::conditions::Decoding;
    use aria_harness::artifact::write_json;
    use serde_json::json;

    fn artifact(directory: &Path, name: &str, passes: u32, errors: &[usize]) -> PathBuf {
        let path = directory.join(format!("{name}.jsonl"));
        let provenance = Provenance::capture().unwrap();
        let cases: BTreeMap<_, _> = errors
            .iter()
            .enumerate()
            .map(|(index, _)| (index.to_string(), format!("audio-reference-{index}")))
            .collect();
        let mut rows = String::new();
        for pass in 1..=passes {
            for (index, errors) in errors.iter().enumerate() {
                rows.push_str(
                    &json!({
                        "schema": super::super::record::SCHEMA_VERSION,
                        "run_id": provenance.run_id,
                        "pass": pass,
                        "backend": { "name": "test" },
                        "utterance": index.to_string(),
                        "normalized_reference": "the reference",
                        "normalized_hypothesis": format!("heard with {errors} errors"),
                        "reference_words": 10,
                        "substitutions": errors,
                        "deletions": 0,
                        "insertions": 0,
                        "error": null,
                        "conditions": Conditions::new(Decoding::Whole, false, false),
                        "decode_ms": 12,
                        "transcribe_ms": 20,
                        "decode_calls": 3,
                        "segments": [{ "text": "hello", "emitted_ms": 9 }],
                        "previews": []
                    })
                    .to_string(),
                );
                rows.push('\n');
            }
        }
        fs::write(&path, rows).unwrap();
        let manifest = Manifest {
            schema: super::super::record::SCHEMA_VERSION,
            provenance,
            corpus_sha256: fingerprint_value(&cases).unwrap(),
            scorer_sha256: scorer_fingerprint().unwrap(),
            cases,
            engines: vec![Engine {
                name: "test".to_owned(),
                model_sha256: Some("model".to_owned()),
                device: None,
                conditions: Conditions::new(Decoding::Whole, false, false),
            }],
            passes,
            language: "en".to_owned(),
            timing_origin: "transcription-start-after-capture-setup".to_owned(),
            provisional: false,
            rows: passes as usize * errors.len(),
            results_sha256: fingerprint(&path).unwrap(),
        };
        write_json(&manifest_path(&path), &manifest).unwrap();
        path
    }

    /// Refingerprints the manifest, so the edited rows still load.
    fn rewrite_rows(path: &Path, edit: impl FnOnce(&mut [serde_json::Value])) {
        let mut rows: Vec<serde_json::Value> = fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        edit(&mut rows);
        let text: Vec<_> = rows.iter().map(|row| row.to_string()).collect();
        fs::write(path, text.join("\n") + "\n").unwrap();
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(manifest_path(path)).unwrap()).unwrap();
        manifest.results_sha256 = fingerprint(path).unwrap();
        fs::write(manifest_path(path), serde_json::to_vec(&manifest).unwrap()).unwrap();
    }

    #[test]
    fn repeats_keep_the_utterance_cluster_and_compare_with_a_smaller_budget() {
        let directory = tempfile::tempdir().unwrap();
        let baseline =
            Artifact::load(&artifact(directory.path(), "baseline", 5, &[1, 4, 2, 3])).unwrap();
        let candidate =
            Artifact::load(&artifact(directory.path(), "candidate", 1, &[1, 4, 2, 3])).unwrap();
        assert_eq!(
            baseline.measurements()[0].interval,
            candidate.measurements()[0].interval
        );
        let measured = &candidate.measurements()[0];
        assert_eq!(measured.decode_ms, 48);
        assert_eq!(measured.decode_calls, 12);
        assert_eq!(measured.transcribe_ms, 80);
        assert_eq!(measured.first_finalized_median_ms, Some(9));
        assert_eq!(measured.finalized_attempts, 4);
        assert_eq!(measured.first_preview_median_ms, None);
        assert!(!measured.realtime);
        let comparison = candidate.compare(&baseline, 0.0).unwrap();
        assert_eq!(comparison[0].decision, Decision::Accepted);
        assert_eq!(comparison[0].difference_interval, Some((0.0, 0.0)));
        assert!(comparison[0].changes.is_empty());
        let regression =
            Artifact::load(&artifact(directory.path(), "regression", 1, &[1, 6, 4, 5])).unwrap();
        let comparison = regression.compare(&baseline, 0.0).unwrap();
        assert_eq!(comparison[0].decision, Decision::Regressed);
        let changed: Vec<_> = comparison[0]
            .changes
            .iter()
            .map(|change| {
                (
                    change.utterance.as_str(),
                    change.baseline[0].1.as_str(),
                    change.candidate[0].1.as_str(),
                )
            })
            .collect();
        assert_eq!(
            changed,
            [
                ("1", "heard with 4 errors", "heard with 6 errors"),
                ("2", "heard with 2 errors", "heard with 4 errors"),
                ("3", "heard with 3 errors", "heard with 5 errors"),
            ]
        );
    }

    #[test]
    fn a_hypothesis_that_changed_on_a_later_pass_is_listed_with_its_pass() {
        let directory = tempfile::tempdir().unwrap();
        let baseline = Artifact::load(&artifact(directory.path(), "baseline", 2, &[1, 2])).unwrap();
        let path = artifact(directory.path(), "candidate", 2, &[1, 2]);
        rewrite_rows(&path, |rows| {
            let second_pass = rows
                .iter_mut()
                .find(|row| row["pass"] == 2 && row["utterance"] == "0")
                .unwrap();
            second_pass["normalized_hypothesis"] = "heard nothing".into();
            second_pass["substitutions"] = 9.into();
        });
        let candidate = Artifact::load(&path).unwrap();

        let comparison = candidate.compare(&baseline, 0.0).unwrap();

        assert_eq!(comparison[0].changes.len(), 1);
        let change = &comparison[0].changes[0];
        assert_eq!(change.utterance, "0");
        assert_eq!(
            change.baseline,
            [
                (1, "heard with 1 errors".to_owned()),
                (2, "heard with 1 errors".to_owned())
            ]
        );
        assert_eq!(
            change.candidate,
            [
                (1, "heard with 1 errors".to_owned()),
                (2, "heard nothing".to_owned())
            ]
        );
    }

    #[test]
    fn a_reference_names_no_host_and_refuses_failed_utterances() {
        let directory = tempfile::tempdir().unwrap();
        let path = artifact(directory.path(), "run", 1, &[1, 2]);
        let destination = directory.path().join("baselines").join("reference.jsonl");

        save_reference(&path, &destination).unwrap();

        let saved: Manifest =
            serde_json::from_slice(&fs::read(manifest_path(&destination)).unwrap()).unwrap();
        assert_eq!(saved.provenance.host, None);
        assert_eq!(fs::read(&path).unwrap(), fs::read(&destination).unwrap());
        assert!(Artifact::load(&destination).is_ok());

        let failing = artifact(directory.path(), "failing", 1, &[1, 2]);
        rewrite_rows(&failing, |rows| {
            rows[0]["error"] = "the engine gave up".into();
        });

        assert!(save_reference(&failing, &destination).is_err());
        assert_eq!(fs::read(&path).unwrap(), fs::read(&destination).unwrap());
        assert!(Artifact::load(&destination).is_ok());
    }

    #[test]
    fn a_single_utterance_and_overlapping_differences_are_inconclusive() {
        let directory = tempfile::tempdir().unwrap();
        let one = Artifact::load(&artifact(directory.path(), "one", 8, &[1])).unwrap();
        assert_eq!(
            one.compare(&one, 0.0).unwrap()[0].decision,
            Decision::Inconclusive
        );
        let baseline = Artifact::load(&artifact(directory.path(), "baseline", 1, &[1, 4])).unwrap();
        let candidate =
            Artifact::load(&artifact(directory.path(), "candidate", 1, &[4, 1])).unwrap();
        assert_eq!(
            candidate.compare(&baseline, 0.0).unwrap()[0].decision,
            Decision::Inconclusive
        );
    }

    #[test]
    fn incomplete_and_duplicate_rows_cannot_look_like_complete_runs() {
        let directory = tempfile::tempdir().unwrap();
        let path = artifact(directory.path(), "complete", 1, &[1, 2]);
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.lines().next().unwrap()).unwrap();
        assert!(Artifact::load(&path).is_err());
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(manifest_path(&path)).unwrap()).unwrap();
        manifest.results_sha256 = fingerprint(&path).unwrap();
        fs::write(manifest_path(&path), serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(Artifact::load(&path).is_err());
        fs::write(&path, format!("{0}\n{0}\n", text.lines().next().unwrap())).unwrap();
        manifest.results_sha256 = fingerprint(&path).unwrap();
        fs::write(manifest_path(&path), serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(Artifact::load(&path).is_err());
    }

    #[test]
    fn row_conditions_must_match_the_completed_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let path = artifact(directory.path(), "conditions", 1, &[1]);
        let original: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(manifest_path(&path)).unwrap()).unwrap();
        for conditions in [
            Conditions::new(
                Decoding::agreement(crate::stt::Cadence::from_millis(750, 20_000)),
                false,
                true,
            ),
            Conditions::new(
                Decoding::Native {
                    chunk_samples: 12_800,
                },
                true,
                false,
            ),
            Conditions::new(
                Decoding::ChunkedFile {
                    chunk_samples: 8_000,
                },
                false,
                false,
            ),
            Conditions {
                replay: Replay::Realtime,
                ..manifest.engines[0].conditions
            },
            Conditions {
                previews: true,
                ..manifest.engines[0].conditions
            },
        ] {
            let mut row = original.clone();
            row["conditions"] = serde_json::to_value(conditions).unwrap();
            fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
            manifest.results_sha256 = fingerprint(&path).unwrap();
            fs::write(manifest_path(&path), serde_json::to_vec(&manifest).unwrap()).unwrap();
            assert!(Artifact::load(&path).is_err(), "{conditions:?}");
        }
        fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
        manifest.results_sha256 = fingerprint(&path).unwrap();
        manifest.schema = 6;
        fs::write(manifest_path(&path), serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(Artifact::load(&path).is_err());
    }

    #[test]
    fn changed_identity_missing_models_and_failures_prevent_acceptance() {
        let directory = tempfile::tempdir().unwrap();
        let path = artifact(directory.path(), "baseline", 1, &[1, 2]);
        let baseline = Artifact::load(&path).unwrap();
        let mut candidate = Artifact::load(&path).unwrap();
        candidate.manifest.scorer_sha256.push_str("-different");
        assert!(candidate.compare(&baseline, 0.0).is_err());
        let mut candidate = Artifact::load(&path).unwrap();
        candidate.manifest.language = "auto".to_owned();
        assert!(candidate.compare(&baseline, 0.0).is_err());
        let mut candidate = Artifact::load(&path).unwrap();
        candidate.manifest.engines[0].conditions.replay = Replay::Realtime;
        assert!(candidate.compare(&baseline, 0.0).is_err());
        let mut candidate = Artifact::load(&path).unwrap();
        candidate.manifest.provenance.debug_assertions =
            !candidate.manifest.provenance.debug_assertions;
        assert!(candidate.compare(&baseline, 0.0).is_err());
        let mut candidate = Artifact::load(&path).unwrap();
        candidate.manifest.engines[0].device = Some("cpu".to_owned());
        assert!(candidate.compare(&baseline, 0.0).is_err());
        let mut candidate = Artifact::load(&path).unwrap();
        candidate
            .manifest
            .cases
            .insert("0".to_owned(), "other audio".to_owned());
        assert!(candidate.compare(&baseline, 0.0).is_err());
        let mut candidate = Artifact::load(&path).unwrap();
        candidate.manifest.engines[0].model_sha256 = None;
        assert!(candidate.compare(&candidate, 0.0).is_err());
        let mut candidate = Artifact::load(&path).unwrap();
        candidate
            .clusters
            .get_mut("test")
            .unwrap()
            .get_mut("0")
            .unwrap()
            .failures = 1;
        assert_eq!(
            candidate.compare(&baseline, 0.0).unwrap()[0].decision,
            Decision::Inconclusive
        );
        assert!(baseline.compare(&baseline, f64::NAN).is_err());
    }
}
