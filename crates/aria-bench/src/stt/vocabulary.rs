use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::hint::black_box;
use std::ops::Range;
use std::path::Path;
use std::time::Instant;

use aria_harness::artifact::{Provenance, fingerprint, fingerprint_value, write_json};
use aria_harness::stats::bootstrap;
use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{SegmentCorrector, Writing};
use aria_voice::vocabulary::{PersonalRules, Term, Vocabulary};
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sentence_ended_before: Vec<Option<bool>>,
    #[serde(default)]
    pub personal_terms: Vec<String>,
    #[serde(default)]
    pub personal_pairs: Vec<(String, String)>,
    #[serde(default)]
    pub project_terms: Vec<String>,
    /// Folder names a named slash glues to: "crates forward slash aria" →
    /// `crates/aria`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_folders: Vec<String>,
    /// Commands unless the case says prose, where "and forward slash or" →
    /// `and/or`.
    #[serde(default, skip_serializing_if = "CaseWriting::is_commands")]
    pub writing: CaseWriting,
    #[serde(default)]
    pub targets: Vec<Target>,
    pub legacy_whole: Option<String>,
    pub legacy_segments: Option<Vec<String>>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaseWriting {
    #[default]
    Commands,
    Prose,
}

impl CaseWriting {
    fn is_commands(&self) -> bool {
        *self == Self::Commands
    }

    fn writing(self) -> Writing {
        match self {
            Self::Commands => Writing::Commands,
            Self::Prose => Writing::Prose,
        }
    }
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
            if !case.sentence_ended_before.is_empty()
                && case.segments.as_ref().map(Vec::len) != Some(case.sentence_ended_before.len())
            {
                return Err(format!("sentence readings do not cover segments: {}", case.id).into());
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

fn corrected_stream(
    inputs: &[String],
    sentence_ended_before: &[Option<bool>],
    vocabulary: &Vocabulary,
    writing: Writing,
) -> (String, Vec<String>) {
    let mut corrector = SegmentCorrector::new(vocabulary).with_writing(writing);
    let mut segments: Vec<String> = inputs
        .iter()
        .enumerate()
        .filter_map(|(index, input)| {
            corrector.push(input, sentence_ended_before.get(index).copied().flatten())
        })
        .map(|typed| typed.text)
        .collect();
    let end = corrector.finish();
    segments.extend(
        end.words
            .into_iter()
            .chain(end.mark)
            .map(|typed| typed.text),
    );
    (end.text, segments)
}

pub fn evaluate(suite: &Suite, output: &Path) -> Result<()> {
    suite.validate()?;
    let mut rows = Vec::new();
    let mut totals: BTreeMap<String, Counts> = BTreeMap::new();
    let mut groups: BTreeMap<String, BTreeMap<String, Counts>> = BTreeMap::new();
    for case in &suite.cases {
        let rules = PersonalRules::from_stored(
            case.personal_terms.clone(),
            case.personal_pairs.clone(),
            &CancelToken::never(),
        )?;
        let personal = Vocabulary::compile(&[], rules.clone())?;
        let terms: Vec<_> = case
            .project_terms
            .iter()
            .map(|canonical| Term {
                canonical: canonical.clone(),
                priority: 0,
            })
            .collect();
        let project = Vocabulary::compile(&terms, rules)?
            .with_path_names(&case.project_folders, usize::MAX)?;
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
                let (text, corrected) = match matcher {
                    Some(matcher) if streaming => corrected_stream(
                        &inputs,
                        &case.sentence_ended_before,
                        matcher,
                        case.writing.writing(),
                    ),
                    Some(matcher) => {
                        let input = &inputs[0];
                        let text = matcher
                            .correct_spoken(input, case.writing.writing())
                            .map_or_else(|| input.clone(), |corrected| corrected.text);
                        (text.clone(), vec![text])
                    }
                    None => (
                        baseline.clone(),
                        legacy.clone().unwrap_or_else(|| inputs.clone()),
                    ),
                };
                let correction_ns = started.elapsed().as_nanos();
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
                let readings = if streaming {
                    case.sentence_ended_before.as_slice()
                } else {
                    &[]
                };
                rows.push(json!({"id": case.id, "group": case.group, "category": case.category, "mode": mode, "condition": name, "baseline_kind": baseline_kind, "input": inputs, "sentence_ended_before": readings, "baseline": baseline, "output": text, "output_segments": corrected, "error": case.error, "correction_ns": correction_ns, "metrics": counts.report()}));
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

/// What dictation writes for a personal case whose expected text holds a
/// spoken number or symbol. Memory expects the personal words' effect alone.
const DICTATED: &[(&str, &str)] = &[(
    "adversarial_numeric_sentence",
    "There are 12 boxes and one spare.",
)];

pub fn fixtures() -> Result<Suite> {
    let mut suite: Suite = serde_json::from_str(include_str!("vocabulary-cases.json"))?;
    let memory: Value = serde_json::from_str(aria_assistant::memory::EVAL_CASES)?;
    for value in memory["lexicon"]
        .as_array()
        .ok_or("missing personal lexicon cases")?
    {
        let id = value["id"].as_str().ok_or("missing lexicon id")?;
        let input = value["input"]
            .as_str()
            .ok_or("missing lexicon input")?
            .to_owned();
        let expected = value["expected"].as_str().ok_or("missing expected text")?;
        let written = DICTATED
            .iter()
            .find(|(case, _)| *case == id)
            .map_or(expected, |(_, written)| written)
            .to_owned();
        suite.cases.push(Case {
            id: format!("personal/{id}"),
            group: format!("personal/{id}"),
            category: "personal_regression".into(),
            spoken_reference: expected.to_owned(),
            written_reference: written,
            whole_input: Some(input.clone()),
            segments: (!input.trim().is_empty()).then(|| vec![input]),
            sentence_ended_before: Vec::new(),
            personal_terms: serde_json::from_value(value["terms"].clone())?,
            personal_pairs: serde_json::from_value(
                value.get("pairs").cloned().unwrap_or(json!([])),
            )?,
            project_terms: Vec::new(),
            project_folders: Vec::new(),
            writing: CaseWriting::Commands,
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
            || row["case"]["expected"] != case.spoken_reference
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
    #[derive(Deserialize)]
    struct Segment {
        text: String,
        sentence_ended_before: Option<bool>,
    }
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
        let observed = Vec::<Segment>::deserialize(&row["segments"])?;
        let (segments, readings): (Vec<_>, Vec<_>) = observed
            .into_iter()
            .map(|segment| (segment.text, segment.sentence_ended_before))
            .unzip();
        case.sentence_ended_before = readings;
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

    fn evaluated(suite: &Suite) -> Value {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("result.json");
        evaluate(suite, &path).unwrap();
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

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
    fn empty_and_overlapping_cases_are_rejected() {
        let mut suite = fixtures().unwrap();
        let duplicate = suite.cases[0].targets[0].clone();
        suite.cases[0].targets.push(duplicate);
        assert!(suite.validate().is_err());
        suite.cases.clear();
        assert!(suite.validate().is_err());
    }

    #[test]
    fn replay_uses_only_learned_pairs_for_listed_words() {
        let mut suite = fixtures().unwrap();
        suite
            .cases
            .retain(|case| case.id == "personal/learned_punctuation");
        let case = &mut suite.cases[0];
        case.personal_terms.clear();
        case.legacy_whole = Some(case.written_reference.clone());
        case.legacy_segments = Some(vec![case.written_reference.clone()]);
        let report = evaluated(&suite);
        for row in report["rows"].as_array().unwrap() {
            let expected = if row["condition"] == "baseline" {
                "Hello, Lumenforge!"
            } else {
                "Hello, lumen forge!"
            };
            assert_eq!(row["output"], expected);
        }
    }

    #[test]
    fn a_personal_fixture_is_scored_against_what_dictation_writes() {
        let mut suite = fixtures().unwrap();
        suite
            .cases
            .retain(|case| case.id == "personal/adversarial_numeric_sentence");
        let case = &suite.cases[0];
        assert_eq!(
            case.spoken_reference,
            "There are twelve boxes and one spare."
        );
        assert_eq!(case.written_reference, "There are 12 boxes and one spare.");
        let report = evaluated(&suite);
        for row in report["rows"].as_array().unwrap() {
            assert_eq!(row["metrics"]["counts"]["introduced_errors"], 0, "{row}");
        }
    }

    #[test]
    fn a_personal_reference_is_what_dictation_writes_without_vocabulary() {
        let suite = fixtures().unwrap();
        for case in &suite.cases {
            if case.category != "personal_regression" {
                continue;
            }
            let dictated = Vocabulary::default()
                .correct_spoken(&case.spoken_reference, Writing::Commands)
                .map_or_else(|| case.spoken_reference.clone(), |corrected| corrected.text);
            assert_eq!(case.written_reference, dictated, "{}", case.id);
        }
    }

    #[test]
    fn streaming_replay_spaces_spoken_symbols_and_uses_the_decoders_sentence_reading() {
        let inputs = ["open src.", "forward slash main.", "then stop."].map(str::to_owned);
        let (typed, segments) = corrected_stream(
            &inputs,
            &[None, Some(false), Some(true)],
            &Vocabulary::default(),
            Writing::Commands,
        );
        assert_eq!(typed, "open src/main. then stop.");
        assert_eq!(segments, ["open src", "/main", ". then stop", "."]);
    }

    #[test]
    fn a_project_folder_glues_a_slash_only_in_the_project() {
        let mut case = fixtures().unwrap().cases.remove(0);
        case.spoken_reference = "open crates forward slash aria".into();
        case.written_reference = "open crates/aria".into();
        case.whole_input = Some(case.spoken_reference.clone());
        case.segments = Some(vec![case.spoken_reference.clone()]);
        case.project_terms.clear();
        case.project_folders = vec!["crates".into()];
        case.targets.clear();
        let report = evaluated(&Suite {
            schema: 1,
            evidence: "test".into(),
            cases: vec![case],
        });
        let output = |condition: &str| {
            report["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["condition"] == condition && row["mode"] == "whole_input")
                .unwrap()["output"]
                .clone()
        };
        assert_eq!(output("project"), "open crates/aria");
        assert_eq!(output("personal"), "open crates /aria");
    }

    #[test]
    fn a_prose_case_is_corrected_as_prose() {
        let mut case = fixtures().unwrap().cases.remove(0);
        case.spoken_reference = "read and forward slash or write".into();
        case.written_reference = "read and/or write".into();
        case.whole_input = Some(case.spoken_reference.clone());
        case.segments = Some(vec!["read and forward slash".into(), "or write".into()]);
        case.project_terms.clear();
        case.targets.clear();
        case.writing = CaseWriting::Prose;
        let report = evaluated(&Suite {
            schema: 1,
            evidence: "test".into(),
            cases: vec![case],
        });
        let outputs: Vec<_> = report["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["condition"] == "personal")
            .map(|row| row["output"].clone())
            .collect();
        assert_eq!(outputs, ["read and/or write", "read and/or write"]);
    }

    #[test]
    fn evaluation_scores_whole_and_segment_inputs_separately() {
        let mut suite = fixtures().unwrap();
        let case = suite
            .cases
            .iter()
            .find(|case| case.id == "three_segments")
            .unwrap()
            .clone();
        suite.cases = vec![case];
        let report = evaluated(&suite);
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
        assert_eq!(streaming["output"], "aria-stt");
        assert_eq!(streaming["metrics"]["counts"]["supported_recovered"], 1);
        assert_eq!(streaming["metrics"]["counts"]["baseline_target_errors"], 1);
    }
}
