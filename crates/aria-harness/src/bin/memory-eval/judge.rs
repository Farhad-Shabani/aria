use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use aria_harness::artifact::{fingerprint, write_json};
use aria_primitives::cancel::CancelToken;
use aria_primitives::process::{self, Bounds, Capture};
use serde::Deserialize;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use super::{index_rows, metadata_path, read_json, read_jsonl, text};

const SYSTEM_PROMPT: &str = "Evaluate the supplied response. Reply yes or no only.";
const OUTPUT_LIMIT: usize = 1_048_576;
const WORKERS: usize = 4;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rubric {
    tasks: BTreeMap<String, String>,
    abstention: String,
}

pub(super) fn run(
    references: &Path,
    hypotheses: &Path,
    rubric: &Path,
    output: &Path,
    model: &str,
) -> Result<()> {
    ensure!(
        !output.exists() && !metadata_path(output).exists(),
        "output already exists"
    );
    let predictions = prediction_metadata(hypotheses)?;
    let templates: Rubric = serde_json::from_value(read_json(rubric)?)?;
    let mut rows = prepare(
        &read_json(references)?,
        &read_jsonl(hypotheses)?,
        &templates,
    )?;
    let environment: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| key != "CLAUDECODE" && key != "CLAUDE_CODE")
        .collect();
    let mut command = Command::new("claude");
    command
        .arg("--version")
        .env_clear()
        .envs(environment.clone());
    let version = run_bounded(&mut command, "", Duration::from_secs(30), OUTPUT_LIMIT)?;
    ensure!(
        version.status.success(),
        "Claude version check failed: {}",
        String::from_utf8_lossy(&version.stderr)
    );
    let metadata = json!({
        "schema": 1,
        "started_at": time::OffsetDateTime::now_utc().format(&Rfc3339)?,
        "model": model,
        "cli_version": String::from_utf8_lossy(&version.stdout).trim(),
        "cases": rows.len(),
        "workers": WORKERS,
        "source_sha256": fingerprint(rubric)?,
        "dataset_sha256": fingerprint(references)?,
        "hypotheses_sha256": fingerprint(hypotheses)?,
        "rubric": rubric,
        "references": references,
        "hypotheses": hypotheses,
        "prediction_metadata": predictions,
        "system_prompt": SYSTEM_PROMPT,
        "comparability": "Uses reference task-specific prompts with a Claude judge; results do not establish equivalence with other judges or published scores.",
        "label_policy": "Exact case-insensitive yes/no only; invalid replies and failed calls produce null labels and a nonzero exit.",
    });
    let mut results = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(output)?;
    write_json(&metadata_path(output), &metadata)?;
    for batch in rows.chunks_mut(WORKERS) {
        thread::scope(|scope| {
            for row in batch.iter_mut() {
                let environment = &environment;
                scope.spawn(move || grade(row, model, environment));
            }
        });
        for row in batch {
            serde_json::to_writer(&mut results, &row)?;
            writeln!(results)?;
            eprintln!(
                "{}: {}",
                row["question_id"],
                row["error"]
                    .as_str()
                    .unwrap_or_else(|| row["judge_reply"].as_str().unwrap_or("failed"))
            );
        }
        results.flush()?;
    }
    let failures = rows.iter().filter(|row| !row["error"].is_null()).count();
    ensure!(
        failures == 0,
        "{failures} judge calls failed; inspect {}",
        output.display()
    );
    Ok(())
}

fn prepare(references: &Value, hypotheses: &[Value], rubric: &Rubric) -> Result<Vec<Value>> {
    let references = references
        .as_array()
        .context("references must be an array")?;
    let reference_index = index_rows(references, "reference")?;
    let hypothesis_index = index_rows(hypotheses, "hypothesis")?;
    ensure!(
        reference_index.keys().eq(hypothesis_index.keys()),
        "reference and hypothesis question IDs differ"
    );
    for template in rubric.tasks.values().chain([&rubric.abstention]) {
        render(template, "", "", "")?;
    }
    references
        .iter()
        .map(|row| {
            let id = text(&row["question_id"])?;
            let response = text(&hypothesis_index[id]["hypothesis"])?;
            let task = text(&row["question_type"])?;
            let task_template = rubric
                .tasks
                .get(task)
                .with_context(|| format!("missing rubric task: {task}"))?;
            let template = if id.contains("_abs") {
                &rubric.abstention
            } else {
                task_template
            };
            let answer = match &row["answer"] {
                Value::Number(number) => number.to_string(),
                value => text(value)?.to_owned(),
            };
            Ok(json!({
                "question_id": id,
                "hypothesis": response,
                "judge_prompt": render(template, text(&row["question"])?, &answer, response)?,
            }))
        })
        .collect()
}

fn render(template: &str, question: &str, answer: &str, response: &str) -> Result<String> {
    ensure!(
        !template.trim().is_empty(),
        "rubric template must not be empty"
    );
    let mut rendered = String::new();
    let mut chars = template.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                rendered.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                rendered.push('}');
            }
            '{' => {
                let mut placeholder = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(character) => placeholder.push(character),
                        None => bail!("unclosed rubric placeholder"),
                    }
                }
                rendered.push_str(match placeholder.as_str() {
                    "question" => question,
                    "answer" => answer,
                    "response" => response,
                    _ => bail!("unknown rubric placeholder: {placeholder}"),
                });
            }
            '}' => bail!("unmatched closing brace in rubric"),
            character => rendered.push(character),
        }
    }
    Ok(rendered)
}

fn prediction_metadata(path: &Path) -> Result<Value> {
    let sidecar = metadata_path(path);
    if !sidecar.try_exists()? {
        return Ok(Value::Null);
    }
    let metadata = read_json(&sidecar)?;
    ensure!(
        metadata.is_object(),
        "prediction metadata must be an object"
    );
    ensure!(
        metadata["hypotheses_sha256"].as_str() == Some(fingerprint(path)?.as_str()),
        "prediction metadata hash differs from hypotheses"
    );
    Ok(metadata)
}

fn parse_reply(stdout: &[u8]) -> Result<(String, bool)> {
    let payload: Value = serde_json::from_slice(stdout)?;
    ensure!(payload.is_object(), "Claude response must be a JSON object");
    ensure!(
        payload["is_error"] == false,
        "Claude returned an error or missing is_error flag"
    );
    let reply = text(&payload["result"])?.trim();
    ensure!(
        reply.eq_ignore_ascii_case("yes") || reply.eq_ignore_ascii_case("no"),
        "expected yes or no"
    );
    Ok((reply.to_owned(), reply.eq_ignore_ascii_case("yes")))
}

fn grade(row: &mut Value, model: &str, environment: &[(std::ffi::OsString, std::ffi::OsString)]) {
    row["judge_reply"] = Value::Null;
    row["error"] = Value::Null;
    row["autoeval_label"] = json!({"model": model, "label": null});
    let result = (|| -> Result<()> {
        let directory = tempfile::Builder::new()
            .prefix("aria-memory-judge-")
            .tempdir()?;
        let mut command = Command::new("claude");
        command
            .args([
                "-p",
                "--output-format",
                "json",
                "--restricted",
                "--tools",
                "",
                "--strict-mcp-config",
                "--system-prompt",
                SYSTEM_PROMPT,
                "--model",
                model,
            ])
            .current_dir(directory.path())
            .env_clear()
            .envs(environment.iter().cloned());
        let completion = run_bounded(
            &mut command,
            text(&row["judge_prompt"])?,
            Duration::from_secs(120),
            OUTPUT_LIMIT,
        )?;
        row["judge_raw_response"] = String::from_utf8_lossy(&completion.stdout)
            .into_owned()
            .into();
        ensure!(
            completion.status.success(),
            "Claude exited {}: {}",
            completion.status,
            String::from_utf8_lossy(&completion.stderr)
        );
        let (reply, label) = parse_reply(&completion.stdout)?;
        row["judge_reply"] = reply.into();
        row["autoeval_label"]["label"] = label.into();
        Ok(())
    })();
    if let Err(error) = result {
        row["error"] = format!("{error:#}").into();
    }
}

fn run_bounded(
    command: &mut Command,
    input: &str,
    timeout: Duration,
    limit: usize,
) -> Result<Output> {
    let bounds = Bounds {
        timeout,
        stdout: Capture::FailAbove(limit),
        stderr: Capture::FailAbove(limit),
    };
    Ok(process::run(
        command,
        input.as_bytes(),
        bounds,
        &CancelToken::never(),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rubric_substitutes_once_and_selects_task_or_abstention() {
        let rubric: Rubric = serde_json::from_value(json!({"tasks":{"temporal":"{question}|{answer}|{response}"},"abstention":"{{abs}} {question}|{answer}|{response}"})).unwrap();
        let references = json!([
            {"question_id":"q", "question_type":"temporal", "question":"{answer}", "answer":3},
            {"question_id":"q_abs_suffix", "question_type":"temporal", "question":"When?", "answer":"Yesterday"}
        ]);
        let hypotheses = vec![
            json!({"question_id":"q_abs_suffix", "hypothesis":"Unknown"}),
            json!({"question_id":"q", "hypothesis":"Today"}),
        ];
        let rows = prepare(&references, &hypotheses, &rubric).unwrap();
        assert_eq!(rows[0]["judge_prompt"], "{answer}|3|Today");
        assert_eq!(rows[1]["judge_prompt"], "{abs} When?|Yesterday|Unknown");
        for template in ["{unknown}", "{question", "question}", ""] {
            assert!(render(template, "Q", "A", "R").is_err());
        }
        for answer in [Value::Null, json!(true)] {
            let mut invalid = references.clone();
            invalid[0]["answer"] = answer;
            assert!(prepare(&invalid, &hypotheses, &rubric).is_err());
        }
        let mut missing_task = rubric;
        missing_task.tasks.clear();
        assert!(prepare(&references, &hypotheses, &missing_task).is_err());
    }

    #[test]
    fn matching_unique_ids_and_strict_labels_are_required() {
        let rubric: Rubric = serde_json::from_value(
            json!({"tasks":{"task":"{response}"},"abstention":"{response}"}),
        )
        .unwrap();
        let row = json!({"question_id":"q", "question_type":"task", "question":"Q", "answer":"A"});
        let hypothesis = json!({"question_id":"q", "hypothesis":"A"});
        for (references, hypotheses) in [
            (json!([row]), vec![]),
            (
                json!([row]),
                vec![json!({"question_id":"other", "hypothesis":"A"})],
            ),
            (json!([row, row]), vec![hypothesis.clone()]),
            (json!([row]), vec![hypothesis.clone(), hypothesis]),
        ] {
            assert!(prepare(&references, &hypotheses, &rubric).is_err());
        }
        for (reply, expected) in [(" yes ", true), ("NO", false)] {
            assert_eq!(
                parse_reply(
                    &serde_json::to_vec(&json!({"is_error":false,"result":reply})).unwrap()
                )
                .unwrap()
                .1,
                expected
            );
        }
        for payload in [
            json!({"is_error":true,"result":"yes"}),
            json!({"is_error":false,"result":"yesterday"}),
            json!({"is_error":false,"result":"yes, but no"}),
            json!({"is_error":false,"result":""}),
            json!({"result":"yes"}),
            json!([]),
        ] {
            assert!(parse_reply(&serde_json::to_vec(&payload).unwrap()).is_err());
        }
    }

    #[test]
    fn prediction_sidecar_is_optional_and_bound_to_input() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("predictions.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        assert_eq!(prediction_metadata(&path).unwrap(), Value::Null);
        let metadata = json!({"hypotheses_sha256": fingerprint(&path).unwrap()});
        write_json(&metadata_path(&path), &metadata).unwrap();
        assert_eq!(prediction_metadata(&path).unwrap(), metadata);
        std::fs::write(&path, "changed\n").unwrap();
        assert!(prediction_metadata(&path).is_err());
    }
}
