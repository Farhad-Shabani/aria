use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::hint::black_box;
use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use aria::vocabulary::{PersonalRules, Term, Vocabulary};
use aria_harness::artifact::{Provenance, fingerprint, fingerprint_value, write_json};
use aria_harness::stats::bootstrap;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub written_words: Range<usize>,
    pub spoken_words: Range<usize>,
    pub context_available: bool,
    pub supported_whole: bool,
    pub supported_streaming: bool,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub id: String,
    pub group: String,
    pub category: String,
    pub spoken_reference: String,
    pub written_reference: String,
    pub whole_input: Option<String>,
    pub segments: Option<Vec<String>>,
    #[serde(default)]
    pub personal_terms: Vec<String>,
    #[serde(default)]
    pub personal_pairs: Vec<(String, String)>,
    #[serde(default)]
    pub project_terms: Vec<String>,
    #[serde(default)]
    pub targets: Vec<Target>,
    pub legacy_whole: Option<String>,
    pub legacy_segments: Option<Vec<String>>,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub schema: u32,
    pub evidence: String,
    pub cases: Vec<Case>,
}

impl Suite {
    pub fn load(path: &Path) -> Result<Self> {
        let suite: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        suite.validate()?;
        Ok(suite)
    }

    fn validate(&self) -> Result<()> {
        if self.schema != 1 || self.cases.is_empty() || self.evidence.trim().is_empty() {
            return Err(
                "expected a nonempty schema 1 vocabulary suite with evidence description".into(),
            );
        }
        let mut ids = BTreeSet::new();
        for case in &self.cases {
            if case.id.is_empty() || case.group.is_empty() || !ids.insert(&case.id) {
                return Err(
                    format!("missing identity/group or duplicate case: {}", case.id).into(),
                );
            }
            if case.whole_input.is_none() && case.segments.is_none() {
                return Err(format!("no whole or segment observation: {}", case.id).into());
            }
            let spoken = crate::stt::words(&case.spoken_reference).len();
            let written = case.written_reference.split_whitespace().count();
            let mut previous_written = 0;
            let mut previous_spoken = 0;
            for target in &case.targets {
                if target.written_words.start < previous_written
                    || target.spoken_words.start < previous_spoken
                    || target.written_words.is_empty()
                    || target.spoken_words.is_empty()
                    || target.written_words.end > written
                    || target.spoken_words.end > spoken
                    || (!target.context_available
                        && (target.supported_whole || target.supported_streaming))
                {
                    return Err(format!("invalid or overlapping target spans: {}", case.id).into());
                }
                previous_written = target.written_words.end;
                previous_spoken = target.spoken_words.end;
            }
            if let Some(segments) = &case.segments
                && (segments.is_empty()
                    || (case.error.is_none() && segments.iter().any(|text| text.trim().is_empty())))
            {
                return Err(format!("empty finalized segment: {}", case.id).into());
            }
            if let Some(legacy) = &case.legacy_segments
                && case.segments.as_ref().map(Vec::len) != Some(legacy.len())
            {
                return Err(format!("legacy segment coverage differs: {}", case.id).into());
            }
        }
        Ok(())
    }
}

#[derive(Default, Clone, Copy, Serialize)]
struct Counts {
    cases: usize,
    failures: usize,
    exact_cases: usize,
    changed_cases: usize,
    changed_exact_cases: usize,
    harmed_cases: usize,
    introduced_errors: usize,
    introduced_error_cases: usize,
    introduced_incorrect_vocabulary_spans: usize,
    targets: usize,
    correct_targets: usize,
    baseline_target_errors: usize,
    recovered_targets: usize,
    lost_targets: usize,
    supported_errors: usize,
    supported_recovered: usize,
    available_errors: usize,
    available_recovered: usize,
    target_words: usize,
    target_errors: usize,
    background_words: usize,
    background_errors: usize,
    baseline_errors: usize,
}

impl Counts {
    fn add(&mut self, other: &Self) {
        self.cases += other.cases;
        self.failures += other.failures;
        self.exact_cases += other.exact_cases;
        self.changed_cases += other.changed_cases;
        self.changed_exact_cases += other.changed_exact_cases;
        self.harmed_cases += other.harmed_cases;
        self.introduced_errors += other.introduced_errors;
        self.introduced_error_cases += other.introduced_error_cases;
        self.introduced_incorrect_vocabulary_spans += other.introduced_incorrect_vocabulary_spans;
        self.targets += other.targets;
        self.correct_targets += other.correct_targets;
        self.baseline_target_errors += other.baseline_target_errors;
        self.recovered_targets += other.recovered_targets;
        self.lost_targets += other.lost_targets;
        self.supported_errors += other.supported_errors;
        self.supported_recovered += other.supported_recovered;
        self.available_errors += other.available_errors;
        self.available_recovered += other.available_recovered;
        self.target_words += other.target_words;
        self.target_errors += other.target_errors;
        self.background_words += other.background_words;
        self.background_errors += other.background_errors;
        self.baseline_errors += other.baseline_errors;
    }

    fn report(&self) -> Value {
        json!({
            "counts": self,
            "exact_case_accuracy": ratio(self.exact_cases, self.cases),
            "changed_case_exact_precision": ratio(self.changed_exact_cases, self.changed_cases),
            "canonical_target_recall": ratio(self.correct_targets, self.targets),
            "overall_recovery": ratio(self.recovered_targets, self.baseline_target_errors),
            "supported_recovery": ratio(self.supported_recovered, self.supported_errors),
            "available_recovery": ratio(self.available_recovered, self.available_errors),
            "spoken_reference_wer": ratio(self.target_errors + self.background_errors, self.target_words + self.background_words),
            "spoken_target_wer": ratio(self.target_errors, self.target_words),
            "spoken_background_wer": ratio(self.background_errors, self.background_words),
        })
    }
}

fn ratio(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn errors(alignment: crate::stt::Alignment) -> usize {
    alignment.substitutions + alignment.deletions + alignment.insertions
}

fn score(case: &Case, baseline: &str, output: &str, streaming: bool) -> Counts {
    let before = crate::stt::score::exact_alignment(&case.written_reference, baseline);
    let after = crate::stt::score::exact_alignment(&case.written_reference, output);
    let introduced_errors = after.introduced_errors(&before);
    let baseline_vocabulary = incorrect_vocabulary_spans(baseline, &before, &case.project_terms);
    let candidate_vocabulary = incorrect_vocabulary_spans(output, &after, &case.project_terms);
    let introduced_incorrect_vocabulary_spans = candidate_vocabulary
        .iter()
        .map(|(span, count)| count.saturating_sub(*baseline_vocabulary.get(span).unwrap_or(&0)))
        .sum();
    let mut mask = vec![false; crate::stt::words(&case.spoken_reference).len()];
    for target in &case.targets {
        mask[target.spoken_words.clone()].fill(true);
    }
    let (target, background) =
        crate::stt::score::partitioned_errors(&case.spoken_reference, output, &mask);
    let mut counts = Counts {
        cases: 1,
        failures: usize::from(case.error.is_some()),
        exact_cases: usize::from(output == case.written_reference),
        changed_cases: usize::from(output != baseline),
        changed_exact_cases: usize::from(output != baseline && output == case.written_reference),
        harmed_cases: usize::from(baseline == case.written_reference && output != baseline),
        introduced_errors,
        introduced_error_cases: usize::from(introduced_errors != 0),
        introduced_incorrect_vocabulary_spans,
        targets: case.targets.len(),
        target_words: mask.iter().filter(|value| **value).count(),
        target_errors: errors(target),
        background_words: mask.iter().filter(|value| !**value).count(),
        background_errors: errors(background),
        baseline_errors: errors(crate::stt::word_errors(&case.spoken_reference, baseline)),
        ..Counts::default()
    };
    for target in &case.targets {
        let was_correct = before.reference_span_matches(target.written_words.clone());
        let is_correct = after.reference_span_matches(target.written_words.clone());
        let recovered = !was_correct && is_correct;
        counts.correct_targets += usize::from(is_correct);
        counts.baseline_target_errors += usize::from(!was_correct);
        counts.recovered_targets += usize::from(recovered);
        counts.lost_targets += usize::from(was_correct && !is_correct);
        if target.context_available {
            counts.available_errors += usize::from(!was_correct);
            counts.available_recovered += usize::from(recovered);
        }
        if if streaming {
            target.supported_streaming
        } else {
            target.supported_whole
        } {
            counts.supported_errors += usize::from(!was_correct);
            counts.supported_recovered += usize::from(recovered);
        }
    }
    counts
}

fn incorrect_vocabulary_spans(
    text: &str,
    alignment: &crate::stt::score::ExactAlignment,
    terms: &[String],
) -> BTreeMap<(String, usize), usize> {
    let mut offset = 0;
    let words: Vec<_> = text
        .split_whitespace()
        .map(|word| {
            offset += text[offset..].find(word).expect("word belongs to text");
            let start = offset;
            offset += word.len();
            start..offset
        })
        .collect();
    let is_word = |character: char| character.is_alphanumeric() || character == '_';
    let mut spans = Vec::new();
    for term in terms.iter().collect::<BTreeSet<_>>() {
        for (start, _) in text.match_indices(term.as_str()) {
            let end = start + term.len();
            if text[..start].chars().next_back().is_some_and(is_word)
                || text[end..].chars().next().is_some_and(is_word)
            {
                continue;
            }
            spans.push((start, end, term));
        }
    }
    spans.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
    let mut covered = 0;
    let mut incorrect = BTreeMap::new();
    for (start, end, term) in spans {
        if start < covered {
            continue;
        }
        covered = end;
        let first = words.partition_point(|word| word.end <= start);
        let last = words.partition_point(|word| word.start < end);
        if first < last && !alignment.hypothesis_span_matches(first..last) {
            *incorrect
                .entry((term.clone(), alignment.anchor(first)))
                .or_default() += 1;
        }
    }
    incorrect
}

pub fn evaluate(suite: &Suite, output: &Path) -> Result<()> {
    suite.validate()?;
    let mut rows = Vec::new();
    let mut totals: BTreeMap<String, Counts> = BTreeMap::new();
    let mut groups: BTreeMap<String, BTreeMap<String, Counts>> = BTreeMap::new();
    for case in &suite.cases {
        let rules = PersonalRules::new(case.personal_terms.clone(), case.personal_pairs.clone());
        let personal = Vocabulary::compile(&[], rules.clone())?;
        let terms: Vec<_> = case
            .project_terms
            .iter()
            .map(|canonical| Term {
                canonical: canonical.clone(),
                priority: 0,
            })
            .collect();
        let project = Vocabulary::compile(&terms, rules)?;
        for streaming in [false, true] {
            if !streaming && case.whole_input.is_none() {
                continue;
            }
            let inputs = if streaming {
                let Some(segments) = &case.segments else {
                    continue;
                };
                segments.clone()
            } else {
                vec![case.whole_input.clone().ok_or("whole input absent")?]
            };
            let legacy = if streaming {
                case.legacy_segments.clone()
            } else {
                case.legacy_whole.clone().map(|text| vec![text])
            };
            let baseline = legacy.as_ref().unwrap_or(&inputs).join(" ");
            let baseline_kind = if legacy.is_some() {
                "frozen_personal"
            } else {
                "uncorrected_input"
            };
            for (name, matcher) in [
                ("baseline", None),
                ("personal", Some(&personal)),
                ("project", Some(&project)),
            ] {
                let started = Instant::now();
                let corrected: Vec<_> = if let Some(matcher) = matcher {
                    inputs
                        .iter()
                        .map(|input| matcher.correct(input).unwrap_or_else(|| input.clone()))
                        .collect()
                } else {
                    legacy.clone().unwrap_or_else(|| inputs.clone())
                };
                let correction_ns = started.elapsed().as_nanos();
                let text = corrected.join(" ");
                let counts = score(case, &baseline, &text, streaming);
                let mode = if streaming {
                    "streaming_segments"
                } else {
                    "whole_input"
                };
                let key = format!("{mode}/{baseline_kind}/{name}");
                totals.entry(key.clone()).or_default().add(&counts);
                groups
                    .entry(key)
                    .or_default()
                    .entry(case.group.clone())
                    .or_default()
                    .add(&counts);
                rows.push(json!({"id": case.id, "group": case.group, "category": case.category, "mode": mode, "condition": name, "baseline_kind": baseline_kind, "input": inputs, "baseline": baseline, "output": text, "output_segments": corrected, "error": case.error, "correction_ns": correction_ns, "metrics": counts.report()}));
            }
        }
    }
    let summaries: BTreeMap<_, _> = totals.iter().map(|(key, counts)| {
        let clusters: Vec<_> = groups[key].values().copied().collect();
        let difference = |sample: &[Counts]| {
            let mut sum = Counts::default();
            for row in sample { sum.add(row); }
            (sum.target_errors + sum.background_errors) as f64 / (sum.target_words + sum.background_words) as f64
                - sum.baseline_errors as f64 / (sum.target_words + sum.background_words) as f64
        };
        let interval = if counts.failures == 0 && clusters.iter().all(|row| row.target_words + row.background_words > 0) { bootstrap(&clusters, difference) } else { None };
        (key, json!({"metrics": counts.report(), "groups": clusters.len(), "paired_spoken_wer_delta_95_interval": interval}))
    }).collect();
    write_json(
        output,
        &json!({
            "schema": 2, "kind": "vocabulary_replay", "provenance": Provenance::capture()?,
            "suite_sha256": fingerprint_value(suite)?, "scorer_sha256": fingerprint_value(&(super::analysis::scorer_fingerprint()?, include_str!("vocabulary.rs")))?,
            "evidence": suite.evidence, "measurement": "in-memory text replay; not real-time capture or delivery latency",
            "scoring": "canonical spans use exact whitespace-token alignment; insertion errors belong to following reference word (last word at EOF); changed-case precision is not edit-level precision",
            "summaries": summaries, "rows": rows,
        }),
    )?;
    Ok(())
}

pub fn fixtures() -> Result<Suite> {
    let mut suite: Suite = serde_json::from_str(include_str!("vocabulary-cases.json"))?;
    let memory: Value =
        serde_json::from_str(include_str!("../../../aria/src/memory/eval_cases.json"))?;
    for value in memory["lexicon"]
        .as_array()
        .ok_or("missing personal lexicon cases")?
    {
        let id = value["id"].as_str().ok_or("missing lexicon id")?;
        let input = value["input"]
            .as_str()
            .ok_or("missing lexicon input")?
            .to_owned();
        suite.cases.push(Case {
            id: format!("personal/{id}"),
            group: format!("personal/{id}"),
            category: "personal_regression".into(),
            spoken_reference: value["expected"]
                .as_str()
                .ok_or("missing expected text")?
                .into(),
            written_reference: value["expected"]
                .as_str()
                .ok_or("missing expected text")?
                .into(),
            whole_input: Some(input.clone()),
            segments: (!input.trim().is_empty()).then(|| vec![input]),
            personal_terms: serde_json::from_value(value["terms"].clone())?,
            personal_pairs: serde_json::from_value(
                value.get("pairs").cloned().unwrap_or(json!([])),
            )?,
            project_terms: Vec::new(),
            targets: Vec::new(),
            legacy_whole: None,
            legacy_segments: None,
            error: None,
        });
    }
    suite.validate()?;
    Ok(suite)
}

pub fn apply_legacy_report(suite: &mut Suite, path: &Path) -> Result<()> {
    let report: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let rows = report["lexicon"]["rows"]
        .as_array()
        .ok_or("expected a memory evaluation lexicon report")?;
    let mut seen = BTreeSet::new();
    for row in rows {
        let id = row["case"]["id"]
            .as_str()
            .ok_or("legacy row has no case id")?;
        if !seen.insert(id) {
            return Err("duplicate legacy case".into());
        }
        let case = suite
            .cases
            .iter_mut()
            .find(|case| case.id == format!("personal/{id}"))
            .ok_or("legacy case absent from suite")?;
        if row["case"]["input"].as_str() != case.whole_input.as_deref()
            || row["case"]["expected"] != case.written_reference
            || row["case"]["terms"] != json!(case.personal_terms)
            || row["case"].get("pairs").cloned().unwrap_or(json!([])) != json!(case.personal_pairs)
        {
            return Err("legacy inputs or rules differ".into());
        }
        let text = row["output"]
            .as_str()
            .ok_or("legacy row has no output")?
            .to_owned();
        case.legacy_whole = Some(text.clone());
        case.legacy_segments = case.segments.as_ref().map(|_| vec![text]);
    }
    if seen.len()
        != suite
            .cases
            .iter()
            .filter(|case| case.category == "personal_regression")
            .count()
    {
        return Err("legacy report does not cover all personal cases".into());
    }
    suite.evidence.push_str(&format!(
        "; frozen personal output report sha256={}",
        fingerprint(path)?
    ));
    Ok(())
}

pub fn replay(path: &Path, annotations: &Suite, backend: &str, output: &Path) -> Result<()> {
    super::analysis::Artifact::load(path)?;
    annotations.validate()?;
    let mut cases = Vec::new();
    let mut seen = BTreeSet::new();
    for line in std::fs::read_to_string(path)?.lines() {
        let row: Value = serde_json::from_str(line)?;
        if row["backend"]["name"] != backend {
            continue;
        }
        let id = row["utterance"].as_str().ok_or("trace has no utterance")?;
        let mut case = annotations
            .cases
            .iter()
            .find(|case| case.id == id)
            .ok_or("trace has no vocabulary annotation")?
            .clone();
        if row["reference"] != case.spoken_reference {
            return Err(format!("reference differs for {id}").into());
        }
        seen.insert(id.to_owned());
        case.id = format!("{id}/pass-{}", row["pass"]);
        case.error = row["error"].as_str().map(str::to_owned);
        case.legacy_whole = None;
        case.legacy_segments = None;
        let hypothesis = row["hypothesis"]
            .as_str()
            .ok_or("trace has no hypothesis")?
            .to_owned();
        let segments: Vec<String> = row["segments"]
            .as_array()
            .ok_or("trace has no segment list")?
            .iter()
            .map(|segment| {
                segment["text"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or("invalid segment text")
            })
            .collect::<std::result::Result<_, _>>()?;
        if row["conditions"]["decoding"]["mode"] == "whole"
            || row["conditions"]["decoding"]["mode"] == "chunked_file"
        {
            if !segments.is_empty() {
                return Err("whole trace unexpectedly contains streaming segments".into());
            }
            case.whole_input = Some(hypothesis);
            case.segments = None;
        } else {
            if case.error.is_none() && segments.join(" ") != hypothesis {
                return Err(format!("segments do not reproduce trace: {id}").into());
            }
            case.whole_input = None;
            case.segments = Some(if segments.is_empty() {
                vec![String::new()]
            } else {
                segments
            });
        }
        cases.push(case);
    }
    if seen.len() != annotations.cases.len() {
        return Err("trace/annotation coverage differs, or backend is absent".into());
    }
    let suite = Suite {
        schema: 1,
        evidence: format!(
            "Frozen ASR text replay: artifact sha256={}, annotations sha256={}; {}",
            fingerprint(path)?,
            fingerprint_value(annotations)?,
            annotations.evidence
        ),
        cases,
    };
    evaluate(&suite, output)
}

pub fn microbench(output: &Path) -> Result<()> {
    let mut rows = Vec::new();
    for count in [100, 1_000, 10_000, 100_000] {
        let terms: Vec<_> = (0..count)
            .map(|index| Term {
                canonical: format!("package-{index}-stt"),
                priority: 0,
            })
            .collect();
        let started = Instant::now();
        let compiled = Vocabulary::compile(&terms, PersonalRules::default());
        let build_ns = started.elapsed().as_nanos();
        let matcher = match compiled {
            Ok(matcher) => matcher,
            Err(error) => {
                rows.push(json!({"terms": count, "status": "compile_failed", "error": error.to_string(), "build_ns": build_ns}));
                continue;
            }
        };
        for (name, input) in [
            (
                "no_match",
                "ordinary speech without any technical names".to_owned(),
            ),
            (
                "shared_prefix_miss",
                "package dash unavailable dash stt".to_owned(),
            ),
            ("match", format!("package dash {} dash stt", count - 1)),
            (
                "dense",
                format!(
                    "package dash 0 dash stt and package dash {} dash stt",
                    count - 1
                ),
            ),
        ] {
            for _ in 0..20 {
                black_box(matcher.correct(black_box(&input)));
            }
            let mut samples = Vec::with_capacity(1_000);
            let mut changed = 0;
            for _ in 0..1_000 {
                let started = Instant::now();
                changed += usize::from(black_box(matcher.correct(black_box(&input))).is_some());
                samples.push(started.elapsed().as_nanos() as u64);
            }
            let mut ordered = samples.clone();
            ordered.sort_unstable();
            rows.push(json!({"terms": count, "case": name, "build_ns": build_ns, "estimated_snapshot_bytes": matcher.estimated_bytes(), "warmups": 20, "changed": changed, "samples_ns": samples, "p50_ns": ordered[499], "p95_ns": ordered[949], "p99_ns": ordered[989]}));
        }
    }
    write_json(
        output,
        &json!({"schema": 1, "kind": "vocabulary_microbench", "provenance": Provenance::capture()?, "measurement": "single-process warmed matcher calls; not streaming or end-to-end latency; estimated bytes are not RSS", "rows": rows}),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn introduced_harm_is_visible_beside_recovered_targets_and_old_errors() {
        let mut case = fixtures().unwrap().cases.remove(0);
        case.written_reference = "open aria-stt and to dash do".into();
        case.spoken_reference = "open aria dash stt and to dash do".into();
        case.project_terms = vec!["aria-stt".into(), "to-do".into()];
        case.targets[0].written_words = 1..2;
        case.targets[0].spoken_words = 1..4;
        let counts = score(
            &case,
            "open aria dash stt and to dash do",
            "open aria-stt and to-do",
            false,
        );
        assert_eq!(counts.recovered_targets, 1);
        assert_eq!(counts.introduced_error_cases, 1);
        assert_eq!(counts.introduced_incorrect_vocabulary_spans, 1);
        assert!(counts.introduced_errors > 0);

        case.written_reference = "open a dash b, please".into();
        case.spoken_reference.clone_from(&case.written_reference);
        case.targets.clear();
        case.project_terms = vec!["a-b".into()];
        let counts = score(&case, "open a dash b, now", "open a-b, now", false);
        assert_eq!(counts.introduced_error_cases, 1);
        assert_eq!(counts.introduced_incorrect_vocabulary_spans, 1);
        let unchanged = score(&case, "open a-b, now", "open a-b, now", false);
        assert_eq!(unchanged.introduced_errors, 0);
        assert_eq!(unchanged.introduced_incorrect_vocabulary_spans, 0);
    }

    #[test]
    fn repeated_distractors_preserve_occurrence_counts() {
        let mut case = fixtures().unwrap().cases.remove(0);
        case.written_reference = "open aria-stt".into();
        case.spoken_reference.clone_from(&case.written_reference);
        case.targets.clear();
        case.project_terms = vec!["aria-stt".into(), "aria-stt".into()];
        let counts = score(
            &case,
            "open aria-stt aria-stt",
            "open aria-stt aria-stt aria-stt",
            false,
        );
        assert_eq!(counts.introduced_errors, 1);
        assert_eq!(counts.introduced_incorrect_vocabulary_spans, 1);
    }

    #[test]
    fn a_canonical_span_cannot_contain_extra_words() {
        let mut case = fixtures().unwrap().cases.remove(0);
        case.written_reference = "New York".into();
        case.spoken_reference = "New York".into();
        case.targets[0].written_words = 0..2;
        case.targets[0].spoken_words = 0..2;
        assert_eq!(
            score(&case, "New old York", "New old York", false).correct_targets,
            0
        );
    }

    #[test]
    fn recovery_keeps_unsupported_cases_and_detects_harm() {
        let suite = fixtures().unwrap();
        let case = &suite.cases[0];
        let good = score(
            case,
            case.whole_input.as_deref().unwrap(),
            &case.written_reference,
            false,
        );
        assert_eq!((good.recovered_targets, good.supported_recovered), (1, 1));
        let mut unsupported = case.clone();
        unsupported.targets[0].supported_streaming = false;
        let split = score(
            &unsupported,
            case.whole_input.as_deref().unwrap(),
            &case.written_reference,
            true,
        );
        assert_eq!((split.recovered_targets, split.supported_recovered), (1, 0));
        let bad = score(
            case,
            &case.written_reference,
            "look in another project",
            false,
        );
        assert_eq!((bad.harmed_cases, bad.lost_targets), (1, 1));
    }

    #[test]
    fn canonical_spelling_is_not_hidden_by_wer_normalization() {
        assert_eq!(crate::stt::words("aria-stt"), crate::stt::words("aria stt"));
        assert!(
            !crate::stt::score::exact_alignment("Cargo.toml", "cargo toml")
                .reference_span_matches(0..1)
        );
        let (target, background) = crate::stt::score::partitioned_errors(
            "open aria now",
            "open wrong aria now",
            &[false, true, false],
        );
        assert_eq!(target.insertions, 1);
        assert_eq!(errors(background), 0);
    }

    #[test]
    fn empty_and_overlapping_cases_are_rejected() {
        let mut suite = fixtures().unwrap();
        let duplicate = suite.cases[0].targets[0].clone();
        suite.cases[0].targets.push(duplicate);
        assert!(suite.validate().is_err());
        suite.cases.clear();
        assert!(suite.validate().is_err());
    }

    #[test]
    fn replay_keeps_whole_and_segment_results_separate() {
        let mut suite = fixtures().unwrap();
        let case = suite
            .cases
            .iter()
            .find(|case| case.id == "three_segments")
            .unwrap()
            .clone();
        suite.cases = vec![case];
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("result.json");
        evaluate(&suite, &path).unwrap();
        let report: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let rows = report["rows"].as_array().unwrap();
        let whole = rows
            .iter()
            .find(|row| row["condition"] == "project" && row["mode"] == "whole_input")
            .unwrap();
        let streaming = rows
            .iter()
            .find(|row| row["condition"] == "project" && row["mode"] == "streaming_segments")
            .unwrap();
        assert_eq!(whole["output"], "aria-stt");
        assert_eq!(streaming["output"], "aria dash stt");
        assert_eq!(streaming["metrics"]["counts"]["baseline_target_errors"], 1);
        assert!(evaluate(&suite, &path).is_err());
    }
}
