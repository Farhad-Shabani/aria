use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, ensure};
use aria_harness::artifact::{fingerprint, write_json, write_jsonl};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::{PrimitiveDateTime, macros::format_description};

use super::{metadata_path, text};

pub(super) fn prepare(dataset: &Path, output: &Path) -> Result<()> {
    let (source, source_hash) = read_source(dataset)?;
    let entries = source.as_array().context("dataset must be a JSON array")?;
    ensure!(!entries.is_empty(), "dataset must be nonempty");
    let mut cases = Vec::new();
    let mut provenance = Vec::new();
    let mut seen = BTreeSet::new();
    for entry in entries {
        let id = text(&entry["question_id"])?;
        ensure!(seen.insert(id), "duplicate question_id: {id}");
        let sessions = entry["haystack_sessions"]
            .as_array()
            .context("sessions must be an array")?;
        let dates = entry["haystack_dates"]
            .as_array()
            .context("dates must be an array")?;
        let ids = entry["haystack_session_ids"]
            .as_array()
            .context("session IDs must be an array")?;
        ensure!(
            sessions.len() == dates.len() && dates.len() == ids.len(),
            "unaligned sessions: {id}"
        );
        let mut session_ids = BTreeSet::new();
        let mut ordered = Vec::new();
        for (index, date) in dates.iter().enumerate() {
            ensure!(
                session_ids.insert(text(&ids[index])?),
                "duplicate session IDs: {id}"
            );
            ordered.push((
                PrimitiveDateTime::parse(
                    text(date)?,
                    format_description!(
                        "[year]/[month padding:none]/[day padding:none] ([weekday repr:short case_sensitive:false]) [hour padding:none]:[minute padding:none]"
                    ),
                )?,
                index,
            ));
        }
        ordered.sort_by_key(|(date, _)| *date);
        let mut windows = Vec::new();
        let mut session_metadata = Vec::new();
        for (_, index) in ordered {
            let turns = sessions[index]
                .as_array()
                .context("session must be an array")?;
            let mut user_turns = Vec::new();
            let mut excluded = 0;
            for turn in turns {
                let content = turn["content"]
                    .as_str()
                    .context("turn content must be text")?;
                match turn["role"].as_str() {
                    Some("user") => user_turns.push(content),
                    Some("assistant") => excluded += 1,
                    _ => anyhow::bail!("unsupported role: {}", turn["role"]),
                }
            }
            session_metadata.push(json!({
                "session_id": ids[index], "date": dates[index], "source_index": index,
                "user_turns": user_turns.len(), "excluded_assistant_turns": excluded,
            }));
            windows.push(user_turns);
        }
        let date = text(&entry["question_date"])?;
        let answer = match &entry["answer"] {
            Value::String(_) => text(&entry["answer"])?.to_owned(),
            Value::Number(answer) => answer.to_string(),
            _ => anyhow::bail!("invalid answer: {id}"),
        };
        cases.push(json!({
            "id": id, "category": text(&entry["question_type"])?, "windows": windows,
            "expected": [], "fact_scoring": false,
            "questions": [{
                "text": format!("Current question date: {date}\n\n{}", text(&entry["question"])?),
                "answers": [answer], "abstain": id.contains("_abs"), "score_locally": false,
            }],
        }));
        provenance
            .push(json!({"question_id": id, "question_date": date, "sessions": session_metadata}));
    }
    write_json(
        output,
        &json!({
            "metadata": {
                "format": "conversational-memory-qa", "source_file": dataset.file_name().context("dataset needs a filename")?.to_string_lossy(),
                "source_sha256": source_hash, "instance_count": cases.len(),
                "adaptation": {
                    "selection": "All input instances, question types and sessions; no sampling or evidence filtering.",
                    "ordering": "Sessions sorted ascending by parsed timestamp; ties preserve input order.",
                    "turns": "User content preserved verbatim; assistant turns excluded to match Aria memory ingestion.",
                    "labels": "has_answer and answer_session_ids are not read; gold answers are scoring metadata only.",
                    "timestamps": "Session dates retained only in provenance; production ingestion does not preserve historical timestamps or present them to extraction. Question date is supplied with the question.",
                    "scoring": "No local fact or exact-answer scoring. Export answers for the semantic judge.",
                    "comparability": "User-only ingestion and omitted session timestamps limit comparison with full-history systems, especially assistant and temporal questions. No question categories are dropped.",
                },
                "instances": provenance,
            },
            "extraction": cases, "lexicon": [],
        }),
    )?;
    Ok(())
}

pub(super) fn export(
    report: &Path,
    output: &Path,
    repetition: usize,
    with_memory: bool,
    allow_extraction_failures: bool,
) -> Result<()> {
    let (source, report_hash) = read_source(report)?;
    ensure!(source["schema"] == 2, "expected benchmark report schema 2");
    ensure!(source["mode"] == "live", "export requires a live report");
    let corpus = &source["corpus"];
    ensure!(
        corpus["metadata"]["format"] == "conversational-memory-qa",
        "report is not a prepared conversational QA corpus"
    );
    let cases = corpus["extraction"]
        .as_array()
        .context("expected corpus extraction cases")?;
    let expected = cases
        .iter()
        .map(|case| text(&case["id"]))
        .collect::<Result<Vec<_>>>()?;
    let expected_ids = expected.iter().copied().collect::<BTreeSet<_>>();
    ensure!(!expected.is_empty(), "empty corpus");
    ensure!(
        expected.len() == expected_ids.len(),
        "duplicate corpus question IDs"
    );
    let runs = source["runs"].as_array().context("expected report runs")?;
    let run = repetition
        .checked_sub(1)
        .and_then(|index| runs.get(index))
        .context("requested repetition has not completed")?;
    ensure!(run["recall_errors"] == 0, "run contains recall errors");
    ensure!(
        allow_extraction_failures || run["failed_cases"] == 0,
        "run contains extraction errors; use --allow-extraction-failures to retain every case"
    );
    let rows = run["rows"].as_array().context("expected result rows")?;
    let mut records = BTreeMap::new();
    let mut failed_ids = BTreeSet::new();
    for row in rows {
        let id = text(&row["case"]["id"])?;
        ensure!(!records.contains_key(id), "duplicate result: {id}");
        let failed = row["failed"]
            .as_bool()
            .context("invalid extraction status")?;
        ensure!(
            allow_extraction_failures || !failed,
            "failed extraction: {id}"
        );
        if failed {
            failed_ids.insert(id);
        }
        let recall = row["recall"].as_array().context("expected recall array")?;
        ensure!(recall.len() == 1, "expected one question: {id}");
        let answers = recall[0]["rows"]
            .as_array()
            .context("expected answer rows")?;
        let selected = answers
            .iter()
            .filter(|answer| answer["with_memory"].as_bool() == Some(with_memory))
            .collect::<Vec<_>>();
        ensure!(
            selected.len() == 1,
            "missing or duplicate selected answer: {id}"
        );
        ensure!(selected[0]["error"].is_null(), "answer error: {id}");
        records.insert(
            id,
            json!({"question_id": id, "hypothesis": text(&selected[0]["reply"])?}),
        );
    }
    ensure!(
        records.keys().copied().eq(expected_ids.iter().copied()),
        "result question IDs differ from input corpus"
    );
    ensure!(
        run["failed_cases"].as_u64() == Some(failed_ids.len() as u64),
        "extraction failure count differs from rows"
    );
    let records = expected
        .into_iter()
        .map(|id| records.remove(id).expect("validated question ID"))
        .collect::<Vec<_>>();
    let sidecar = metadata_path(output);
    ensure!(
        !output.exists() && !sidecar.exists(),
        "output already exists"
    );
    write_jsonl(output, &records)?;
    write_json(
        &sidecar,
        &json!({
            "report_sha256": report_hash, "repetition": repetition,
            "condition": if with_memory { "after" } else { "before" },
            "allow_extraction_failures": allow_extraction_failures,
            "extraction_failures": {"count": failed_ids.len(), "question_ids": failed_ids},
            "hypotheses_sha256": fingerprint(output)?,
        }),
    )?;
    Ok(())
}

fn read_source(path: &Path) -> Result<(Value, String)> {
    let bytes = std::fs::read(path).with_context(|| path.display().to_string())?;
    let value = serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid JSON: {}", path.display()))?;
    Ok((value, format!("{:x}", Sha256::digest(&bytes))))
}

#[cfg(test)]
mod tests {
    use super::super::read_json;
    use super::*;
    use aria_primitives::process::{ChildProcess, WaitOutcome};
    use std::fs;
    use std::io::Write;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn provenance_hashes_parsed_bytes_when_the_source_is_replaced() {
        for exporting in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let source = directory.path().join("source.json");
            let replacement = directory.path().join("replacement.json");
            let output = directory.path().join("output.json");
            let bytes =
                serde_json::to_vec_pretty(&if exporting { report() } else { dataset() }).unwrap();
            let expected_hash = format!("{:x}", Sha256::digest(&bytes));
            fs::write(&replacement, b"{}").unwrap();
            let mut fifo = ChildProcess::spawn(Command::new("mkfifo").arg(&source)).unwrap();
            assert!(matches!(
                fifo.wait_until(Instant::now() + Duration::from_secs(5), || false).unwrap(),
                WaitOutcome::Exited(status) if status.success()
            ));
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    let mut pipe = fs::OpenOptions::new().write(true).open(&source).unwrap();
                    pipe.write_all(&bytes).unwrap();
                    fs::rename(&replacement, &source).unwrap();
                });
                let result = if exporting {
                    export(&source, &output, 1, true, false)
                } else {
                    prepare(&source, &output)
                };
                writer.join().unwrap();
                result.unwrap();
            });
            let actual_hash = if exporting {
                read_json(&metadata_path(&output)).unwrap()["report_sha256"].clone()
            } else {
                read_json(&output).unwrap()["metadata"]["source_sha256"].clone()
            };
            assert_eq!(actual_hash, expected_hash);
        }
    }

    fn dataset() -> Value {
        json!([{
            "question_id": "q_abs_suffix", "question_type": "single-session-assistant",
            "question": "Where?", "answer": 3, "question_date": "2024/03/03 (Sun) 12:00",
            "haystack_session_ids": ["later", "earlier", "tie"],
            "haystack_dates": ["2024/03/02 (Sat) 12:00", "2024/03/01 (Fri) 12:00", "2024/03/01 (Fri) 12:00"],
            "haystack_sessions": [
                [{"role": "user", "content": "  Unlabelled user content.  "}],
                [{"role": "assistant", "content": "Assistant evidence.", "has_answer": true}],
                [{"role": "user", "content": "Tie."}],
            ], "answer_session_ids": ["earlier"],
        }])
    }

    fn prepared(dataset: &Value) -> Result<Value> {
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("source.json");
        let output = directory.path().join("corpus.json");
        write_json(&source, dataset)?;
        prepare(&source, &output)?;
        read_json(&output)
    }

    fn report() -> Value {
        let corpus = json!({
            "metadata": {"format": "conversational-memory-qa"},
            "extraction": [{"id": "q_abs_suffix"}],
        });
        json!({"schema": 2, "mode": "live", "corpus": corpus,
            "runs": [{"failed_cases": 0, "recall_errors": 0, "rows": [{
                "case": corpus["extraction"][0], "failed": false,
                "recall": [{"rows": [
                    {"with_memory": false, "reply": "Unknown.", "error": null},
                    {"with_memory": true, "reply": "Three.", "error": null},
                ]}],
            }]}],
        })
    }

    #[test]
    fn preparation_preserves_cohort_order_and_text_without_evidence_leakage() {
        let mut source = dataset();
        let corpus = prepared(&source).unwrap();
        let case = &corpus["extraction"][0];
        assert_eq!(
            case["windows"],
            json!([[], ["Tie."], ["  Unlabelled user content.  "]])
        );
        assert_eq!(case["questions"][0]["answers"], json!(["3"]));
        assert_eq!(case["questions"][0]["abstain"], true);
        assert_eq!(case["questions"][0]["score_locally"], false);
        assert_eq!(case["fact_scoring"], false);
        assert!(
            !case["questions"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Assistant evidence")
        );
        let sessions = &corpus["metadata"]["instances"][0]["sessions"];
        assert_eq!(sessions[0]["session_id"], "earlier");
        assert_eq!(sessions[0]["excluded_assistant_turns"], 1);
        source[0]["answer_session_ids"] = json!(["later"]);
        source[0]["haystack_sessions"][0][0]["has_answer"] = json!(true);
        source[0]["haystack_dates"][1] = json!("2024/3/1 (mon) 12:0");
        assert_eq!(
            prepared(&source).unwrap()["extraction"],
            corpus["extraction"]
        );
    }

    #[test]
    fn preparation_rejects_malformed_histories_and_answers() {
        let source = dataset();
        assert!(prepared(&json!([source[0], source[0]])).is_err());
        for (field, value) in [
            ("haystack_dates", json!([])),
            (
                "haystack_dates",
                json!([
                    "2024/02/30 (Fri) 12:00",
                    "2024/03/01 (Fri) 12:00",
                    "2024/03/01 (Fri) 12:00"
                ]),
            ),
            ("haystack_session_ids", json!(["same", "same", "tie"])),
            ("answer", json!(true)),
        ] {
            let mut invalid = source.clone();
            invalid[0][field] = value;
            assert!(prepared(&invalid).is_err(), "{field}");
        }
    }

    #[test]
    fn export_selects_repetition_and_condition_and_binds_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("report.json");
        let mut value = report();
        let mut second = value["runs"][0].clone();
        second["rows"][0]["recall"][0]["rows"][1]["reply"] = json!("Four.");
        value["runs"].as_array_mut().unwrap().push(second);
        write_json(&source, &value).unwrap();
        for (index, condition, expected) in [
            (1, false, "Unknown."),
            (1, true, "Three."),
            (2, true, "Four."),
        ] {
            let output = directory.path().join(format!("{index}-{condition}.jsonl"));
            export(&source, &output, index, condition, false).unwrap();
            let rows = super::super::read_jsonl(&output).unwrap();
            assert_eq!(rows[0]["hypothesis"], expected);
            let metadata = read_json(&metadata_path(&output)).unwrap();
            assert_eq!(metadata["report_sha256"], fingerprint(&source).unwrap());
            assert_eq!(metadata["hypotheses_sha256"], fingerprint(&output).unwrap());
            assert_eq!(metadata["repetition"], index);
            assert_eq!(
                metadata["condition"],
                if condition { "after" } else { "before" }
            );
            assert!(export(&source, &output, index, condition, false).is_err());
        }
        let orphan = directory.path().join("orphan.jsonl");
        write_json(&metadata_path(&orphan), &json!({})).unwrap();
        assert!(export(&source, &orphan, 1, true, false).is_err());
        assert!(!orphan.exists());
    }

    #[test]
    fn export_retains_opted_in_failures_but_rejects_invalid_results() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("report.json");
        let output = directory.path().join("predictions.jsonl");
        let mut value = report();
        let mut second = value["runs"][0]["rows"][0].clone();
        second["case"]["id"] = json!("second");
        value["corpus"]["extraction"]
            .as_array_mut()
            .unwrap()
            .push(second["case"].clone());
        value["runs"][0]["rows"]
            .as_array_mut()
            .unwrap()
            .push(second);
        value["runs"][0]["failed_cases"] = json!(1);
        value["runs"][0]["rows"][0]["failed"] = json!(true);
        fs::write(&source, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(export(&source, &output, 1, true, false).is_err());
        export(&source, &output, 1, true, true).unwrap();
        assert_eq!(super::super::read_jsonl(&output).unwrap().len(), 2);
        assert_eq!(
            read_json(&metadata_path(&output)).unwrap()["extraction_failures"],
            json!({"count": 1, "question_ids": ["q_abs_suffix"]})
        );
        for index in 0..8 {
            let mut invalid = value.clone();
            match index {
                0 => invalid["runs"][0]["recall_errors"] = json!(1),
                1 => invalid["runs"][0]["failed_cases"] = json!(2),
                2 => {
                    invalid["runs"][0]["rows"][0]["recall"][0]["rows"][1]["error"] =
                        json!("timeout");
                }
                3 => invalid["runs"][0]["rows"] = json!([]),
                4 => invalid["runs"][0]["rows"][1]["case"]["id"] = json!("q_abs_suffix"),
                5 => invalid["runs"][0]["rows"][0]["failed"] = json!(1),
                6 => invalid["runs"][0]["rows"][0]["recall"][0]["rows"] = json!([]),
                _ => invalid["mode"] = json!("offline"),
            }
            fs::write(&source, serde_json::to_vec(&invalid).unwrap()).unwrap();
            let rejected = directory.path().join(format!("rejected-{index}.jsonl"));
            assert!(
                export(&source, &rejected, 1, true, true).is_err(),
                "case {index}"
            );
            assert!(!rejected.exists());
        }
        fs::write(&source, serde_json::to_vec(&value).unwrap()).unwrap();
        for repetition in [0, 2] {
            assert!(
                export(
                    &source,
                    &directory.path().join("missing.jsonl"),
                    repetition,
                    true,
                    true
                )
                .is_err()
            );
        }
    }
}
