use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use aria_harness::artifact::{fingerprint, write_json};
use aria_harness::stats::paired_mean_ci95;
use serde_json::{Value, json};

use super::{index_rows, metadata_path, read_json, read_jsonl, text};

pub(super) fn run(references: &Path, before: &Path, after: &Path, output: &Path) -> Result<()> {
    let metadata = [
        read_json(&metadata_path(before))?,
        read_json(&metadata_path(after))?,
    ];
    let failures = compare_metadata(&metadata[0], &metadata[1], &fingerprint(references)?)?;
    let references_value = read_json(references)?;
    let references_rows = references_value
        .as_array()
        .context("references must be an array")?;
    let mut report = compare(references_rows, &read_jsonl(before)?, &read_jsonl(after)?)?;
    let ids = index_rows(references_rows, "reference")?;
    for id in failures["question_ids"]
        .as_array()
        .context("missing failure IDs")?
    {
        ensure!(ids.contains_key(text(id)?), "unknown extraction failure ID");
    }
    let failed = failures["count"]
        .as_u64()
        .context("invalid failure count")?;
    report["reliability"] = json!({
        "questions": references_rows.len(),
        "extraction_failed_cases": failed,
        "extraction_succeeded_cases": references_rows.len() as u64 - failed,
        "extraction_failed_question_ids": failures["question_ids"],
        "failed_cases_retained_in_scores": true,
    });
    report["judge_metadata"] = json!(metadata);
    report["inputs"] = json!({});
    for (name, path) in [
        ("references", references),
        ("before", before),
        ("after", after),
    ] {
        report["inputs"][name] = json!({"path": path, "sha256": fingerprint(path)?});
    }
    write_json(output, &report)?;
    Ok(())
}

fn history_clusters(references: &[Value]) -> Result<Vec<Vec<usize>>> {
    fn root(parents: &mut [usize], mut index: usize) -> usize {
        while parents[index] != index {
            parents[index] = parents[parents[index]];
            index = parents[index];
        }
        index
    }
    let mut parents: Vec<_> = (0..references.len()).collect();
    let mut owners = HashMap::new();
    for (index, row) in references.iter().enumerate() {
        let sessions = row["haystack_session_ids"]
            .as_array()
            .context("haystack_session_ids must be an array")?;
        for session in sessions {
            let session = text(session)?;
            if let Some(&owner) = owners.get(session) {
                let parent = root(&mut parents, index);
                parents[parent] = root(&mut parents, owner);
            } else {
                owners.insert(session, index);
            }
        }
    }
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut positions = HashMap::new();
    for index in 0..references.len() {
        let parent = root(&mut parents, index);
        let position = *positions.entry(parent).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[position].push(index);
    }
    Ok(groups)
}

fn counts(pairs: &[(bool, bool)]) -> Value {
    let accuracy = |after: bool| {
        (!pairs.is_empty()).then(|| {
            pairs
                .iter()
                .filter(|pair| if after { pair.1 } else { pair.0 })
                .count() as f64
                / pairs.len() as f64
        })
    };
    json!({
        "questions": pairs.len(),
        "improved": pairs.iter().filter(|&&(before, after)| !before && after).count(),
        "harmed": pairs.iter().filter(|&&(before, after)| before && !after).count(),
        "both_correct": pairs.iter().filter(|&&(before, after)| before && after).count(),
        "neither": pairs.iter().filter(|&&(before, after)| !before && !after).count(),
        "accuracy_before": accuracy(false), "accuracy_after": accuracy(true),
    })
}

fn compare(references: &[Value], before: &[Value], after: &[Value]) -> Result<Value> {
    let reference_index = index_rows(references, "reference")?;
    let before = index_rows(before, "before")?;
    let after = index_rows(after, "after")?;
    ensure!(
        reference_index.keys().eq(before.keys()) && before.keys().eq(after.keys()),
        "question IDs differ"
    );
    let mut models = BTreeSet::new();
    let mut pairs = Vec::new();
    let mut categories: BTreeMap<&str, Vec<(bool, bool)>> = BTreeMap::new();
    let mut abstention = Vec::new();
    let mut answerable = Vec::new();
    for reference in references {
        let id = text(&reference["question_id"])?;
        let mut labels = [false; 2];
        for (index, rows) in [&before, &after].into_iter().enumerate() {
            let row = rows[id];
            ensure!(
                row["error"].is_null(),
                "judge error: {id}: {}",
                row["error"]
            );
            labels[index] = row["autoeval_label"]["label"]
                .as_bool()
                .with_context(|| format!("judge label must be boolean: {id}"))?;
            models.insert(text(&row["autoeval_label"]["model"])?);
        }
        let pair = (labels[0], labels[1]);
        pairs.push(pair);
        categories
            .entry(text(&reference["question_type"])?)
            .or_default()
            .push(pair);
        if id.contains("_abs") {
            abstention.push(pair);
        } else {
            answerable.push(pair);
        }
    }
    ensure!(
        models.len() == 1,
        "all before/after rows must use the same judge model"
    );
    let mut cluster_rows = Vec::new();
    let mut total_delta = 0.0;
    for group in history_clusters(references)? {
        let delta = group
            .iter()
            .map(|&index| i32::from(pairs[index].1) - i32::from(pairs[index].0))
            .sum::<i32>() as f64
            / group.len() as f64;
        total_delta += delta;
        let ids: Vec<_> = group
            .iter()
            .map(|&index| &references[index]["question_id"])
            .collect();
        cluster_rows.push(json!({"question_ids": ids, "paired_delta": delta}));
    }
    let mean = total_delta / cluster_rows.len() as f64;
    let categories: BTreeMap<_, _> = categories
        .into_iter()
        .map(|(category, trials)| (category, counts(&trials)))
        .collect();
    Ok(json!({
        "schema": 1, "judge_model": models.first(), "overall": counts(&pairs),
        "answerable": counts(&answerable), "abstention": counts(&abstention), "categories": categories,
        "cluster_weighted": {
            "history_clusters": cluster_rows.len(), "mean_paired_delta": mean,
            "paired_delta_ci95": paired_mean_ci95(mean, cluster_rows.len()), "clusters": cluster_rows,
            "method": "Equal cluster weights; fixed-sample 95% Hoeffding interval for paired differences in [-1,1]. Questions sharing history session IDs are merged transitively.",
            "assumptions": "The interval assumes independent, representative history clusters. Session-ID clustering does not establish independence or population representativeness. This is a challenge-set score, not evidence of population performance.",
        }
    }))
}

fn compare_metadata(before: &Value, after: &Value, reference_hash: &str) -> Result<Value> {
    for field in [
        "model",
        "cli_version",
        "source_sha256",
        "dataset_sha256",
        "system_prompt",
    ] {
        ensure!(
            before.get(field).is_some() && before.get(field) == after.get(field),
            "judge metadata differs: {field}"
        );
    }
    ensure!(
        before["dataset_sha256"] == reference_hash,
        "judge reference hash differs from comparison reference"
    );
    let mut predictions = Vec::new();
    for (metadata, condition) in [(before, "before"), (after, "after")] {
        let prediction = metadata
            .get("prediction_metadata")
            .filter(|value| value.is_object())
            .context("comparison requires prediction provenance")?;
        ensure!(
            prediction["condition"] == condition,
            "expected {condition} predictions"
        );
        ensure!(
            metadata.get("hypotheses_sha256").is_some()
                && prediction.get("hypotheses_sha256") == metadata.get("hypotheses_sha256"),
            "prediction hash differs from judge input"
        );
        let hash = prediction["report_sha256"]
            .as_str()
            .context("invalid prediction report hash")?;
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid prediction report hash"
        );
        ensure!(
            prediction["repetition"]
                .as_u64()
                .is_some_and(|value| value > 0),
            "invalid prediction repetition"
        );
        let failures = prediction
            .get("extraction_failures")
            .filter(|value| value.is_object())
            .context("missing extraction failure metadata")?;
        let ids = failures["question_ids"]
            .as_array()
            .context("invalid extraction failure metadata")?;
        let mut unique = BTreeSet::new();
        for id in ids {
            let id = id
                .as_str()
                .filter(|id| !id.is_empty())
                .context("invalid extraction failure ID")?;
            ensure!(unique.insert(id), "duplicate extraction failure ID");
        }
        ensure!(
            failures["count"].as_u64() == Some(ids.len() as u64),
            "invalid extraction failure metadata"
        );
        ensure!(
            ids.is_empty() || prediction["allow_extraction_failures"] == true,
            "extraction failures require explicit export opt-in"
        );
        predictions.push(prediction);
    }
    for field in ["report_sha256", "repetition", "extraction_failures"] {
        ensure!(
            predictions[0][field] == predictions[1][field],
            "prediction provenance differs: {field}"
        );
    }
    Ok(predictions[0]["extraction_failures"].clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(id: &str, sessions: &[&str]) -> Value {
        json!({"question_id": id, "question_type": "identity", "haystack_session_ids": sessions})
    }

    fn judged(id: &str, label: bool) -> Value {
        json!({"question_id": id, "autoeval_label": {"model": "opus", "label": label}, "error": null})
    }

    #[test]
    fn transitive_clusters_preserve_input_order_and_abstentions() {
        let references = [
            reference("z", &["one"]),
            reference("b", &["two"]),
            reference("a", &["one", "two"]),
            reference("d_abs_tail", &["three"]),
        ];
        let before: Vec<_> = references
            .iter()
            .map(|row| judged(row["question_id"].as_str().unwrap(), false))
            .collect();
        let after: Vec<_> = references
            .iter()
            .map(|row| judged(row["question_id"].as_str().unwrap(), true))
            .collect();
        let report = compare(&references, &before, &after).unwrap();
        assert_eq!(report["cluster_weighted"]["history_clusters"], 2);
        assert_eq!(
            report["cluster_weighted"]["clusters"][0]["question_ids"],
            json!(["z", "b", "a"])
        );
        assert_eq!(report["overall"]["improved"], 4);
        assert_eq!(report["abstention"]["questions"], 1);
        assert_eq!(report["answerable"]["questions"], 3);
    }

    #[test]
    fn clusters_have_equal_weight_and_empty_categories_have_no_accuracy() {
        let references = [
            reference("a", &["shared"]),
            reference("b", &["shared"]),
            reference("c", &[]),
        ];
        let before = [judged("a", false), judged("b", false), judged("c", true)];
        let after = [judged("a", true), judged("b", true), judged("c", false)];
        let report = compare(&references, &before, &after).unwrap();
        assert_eq!(report["cluster_weighted"]["mean_paired_delta"], 0.0);
        assert_eq!(report["overall"]["improved"], 2);
        assert_eq!(report["overall"]["harmed"], 1);
        assert!(report["abstention"]["accuracy_before"].is_null());
    }

    #[test]
    fn rejects_unpaired_invalid_and_failed_judgments() {
        let references = [reference("a", &[])];
        let baseline = vec![judged("a", true)];
        for candidate in [
            vec![],
            vec![judged("a", true); 2],
            vec![judged("other", true)],
        ] {
            assert!(compare(&references, &baseline, &candidate).is_err());
        }
        for (field, value) in [
            ("error", json!("timeout")),
            ("autoeval_label", json!({"model": "sonnet", "label": true})),
            ("autoeval_label", json!({"model": "opus", "label": 1})),
        ] {
            let mut candidate = baseline.clone();
            candidate[0][field] = value;
            assert!(compare(&references, &baseline, &candidate).is_err());
        }
        assert!(
            compare(
                &[references[0].clone(), references[0].clone()],
                &baseline,
                &baseline
            )
            .is_err()
        );
    }

    #[test]
    fn provenance_requires_matching_runs_judges_and_explicit_failure_opt_in() {
        let before = json!({
            "model": "opus", "cli_version": "1", "source_sha256": "rubric", "dataset_sha256": "dataset", "system_prompt": "grade", "hypotheses_sha256": "before",
            "prediction_metadata": {"report_sha256": "a".repeat(64), "repetition": 1, "condition": "before", "hypotheses_sha256": "before", "allow_extraction_failures": true, "extraction_failures": {"count": 1, "question_ids": ["a"]}}
        });
        let mut after = before.clone();
        after["hypotheses_sha256"] = json!("after");
        after["prediction_metadata"]["condition"] = json!("after");
        after["prediction_metadata"]["hypotheses_sha256"] = json!("after");
        assert_eq!(
            compare_metadata(&before, &after, "dataset").unwrap(),
            json!({"count": 1, "question_ids": ["a"]})
        );
        for (field, value) in [
            ("report_sha256", json!("b".repeat(64))),
            ("report_sha256", json!("G".repeat(64))),
            ("repetition", json!(2)),
            ("repetition", json!(true)),
            ("repetition", json!(0)),
            ("condition", json!("before")),
            ("hypotheses_sha256", json!("mismatch")),
            (
                "extraction_failures",
                json!({"count": 1, "question_ids": ["b"]}),
            ),
            (
                "extraction_failures",
                json!({"count": 2, "question_ids": ["a"]}),
            ),
            (
                "extraction_failures",
                json!({"count": 2, "question_ids": ["a", "a"]}),
            ),
            ("allow_extraction_failures", json!(false)),
        ] {
            let mut changed = after.clone();
            changed["prediction_metadata"][field] = value;
            assert!(
                compare_metadata(&before, &changed, "dataset").is_err(),
                "{field}"
            );
        }
        for field in [
            "model",
            "cli_version",
            "source_sha256",
            "dataset_sha256",
            "system_prompt",
            "prediction_metadata",
        ] {
            let mut changed = after.clone();
            changed.as_object_mut().unwrap().remove(field);
            assert!(
                compare_metadata(&before, &changed, "dataset").is_err(),
                "{field}"
            );
        }
        assert!(compare_metadata(&before, &after, "other dataset").is_err());
    }
}
