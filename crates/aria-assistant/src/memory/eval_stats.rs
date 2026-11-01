use std::collections::BTreeMap;

use serde_json::{Value, json};

pub(super) fn summarize(runs: &[Value]) -> Value {
    let mut overall = Groups::default();
    let mut categories: BTreeMap<&str, Groups> = BTreeMap::new();
    let mut failed_extraction_runs = 0;
    for run in runs {
        for row in run["rows"].as_array().expect("extraction rows") {
            let id = row["case"]["id"].as_str().expect("conversation id");
            let category = row["case"]["category"].as_str().expect("category");
            failed_extraction_runs += usize::from(row["failed"] == true);
            for recall in row["recall"].as_array().expect("recall trials") {
                if recall["question"]["score_locally"] == false {
                    continue;
                }
                let calls = recall["rows"].as_array().expect("paired calls");
                assert_eq!(calls.len(), 2, "each recall needs two calls");
                let before = calls
                    .iter()
                    .find(|call| call["with_memory"] == false)
                    .expect("call without memory");
                let after = calls
                    .iter()
                    .find(|call| call["with_memory"] == true)
                    .expect("call with memory");
                let before_error = !before["error"].is_null();
                let after_error = !after["error"].is_null();
                let before_correct = before["correct"] == true && !before_error;
                let after_correct = after["correct"] == true && !after_error;
                let errors = usize::from(before_error) + usize::from(after_error);
                let abstain = recall["question"]["abstain"]
                    .as_bool()
                    .expect("abstention flag");
                for groups in [&mut overall, categories.entry(category).or_default()] {
                    groups
                        .overall
                        .record(id, before_correct, after_correct, errors);
                    let group = if abstain {
                        &mut groups.abstention
                    } else {
                        &mut groups.answerable
                    };
                    group.record(id, before_correct, after_correct, errors);
                }
            }
        }
    }
    json!({
        "method": "equal conversation weights; paired mean accuracy difference; fixed-sample 95% Hoeffding interval",
        "interval_assumptions": "Representative, independently sampled conversation cases; repetitions and questions do not increase independent n. A curated challenge corpus does not establish these assumptions. Intervals are marginal, not simultaneous across strata.",
        "failed_extraction_runs": failed_extraction_runs,
        "overall": overall.overall.report(),
        "answerable": overall.answerable.report(),
        "abstention": overall.abstention.report(),
        "categories": categories.into_iter().map(|(category, groups)| (category, groups.report())).collect::<BTreeMap<_, _>>(),
    })
}

#[derive(Default)]
struct Groups {
    overall: Group,
    answerable: Group,
    abstention: Group,
}

impl Groups {
    fn report(&self) -> Value {
        json!({"overall": self.overall.report(), "answerable": self.answerable.report(), "abstention": self.abstention.report()})
    }
}

#[derive(Default)]
struct Group {
    cases: BTreeMap<String, [usize; 3]>,
    improved: usize,
    harmed: usize,
    both_correct: usize,
    neither: usize,
    call_errors: usize,
}

impl Group {
    fn record(&mut self, id: &str, before: bool, after: bool, errors: usize) {
        let counts = self.cases.entry(id.to_owned()).or_default();
        counts[0] += usize::from(before);
        counts[1] += usize::from(after);
        counts[2] += 1;
        match (before, after) {
            (false, true) => self.improved += 1,
            (true, false) => self.harmed += 1,
            (true, true) => self.both_correct += 1,
            (false, false) => self.neither += 1,
        }
        self.call_errors += errors;
    }

    fn report(&self) -> Value {
        let n = self.cases.len();
        let mean = |index: usize| {
            (n > 0).then(|| {
                self.cases
                    .values()
                    .map(|counts| counts[index] as f64 / counts[2] as f64)
                    .sum::<f64>()
                    / n as f64
            })
        };
        let before = mean(0);
        let after = mean(1);
        let delta = before.zip(after).map(|(before, after)| after - before);
        let interval = delta.and_then(|delta| aria_harness::stats::paired_mean_ci95(delta, n));
        json!({
            "independent_cases": n,
            "trials": self.improved + self.harmed + self.both_correct + self.neither,
            "improved": self.improved,
            "harmed": self.harmed,
            "both_correct": self.both_correct,
            "neither": self.neither,
            "call_errors": self.call_errors,
            "mean_without_memory": before,
            "mean_with_memory": after,
            "paired_delta": delta,
            "paired_delta_ci95": interval,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation(id: &str, abstain: bool, before: bool, after: bool, reverse: bool) -> Value {
        let mut calls = vec![
            json!({"with_memory": false, "correct": before, "error": null}),
            json!({"with_memory": true, "correct": after, "error": null}),
        ];
        if reverse {
            calls.reverse();
        }
        json!({"case": {"id": id, "category": "test"}, "failed": false,
            "recall": [{"question": {"abstain": abstain}, "rows": calls}]})
    }

    #[test]
    fn paired_scores_weight_conversations_and_separate_abstention() {
        let improved = conversation("improved", false, false, true, true);
        let harmed = conversation("harmed", true, true, false, false);
        let runs = vec![
            json!({"rows": [improved, harmed]}),
            json!({"rows": [improved, improved]}),
        ];
        let report = summarize(&runs);
        assert_eq!(report["overall"]["independent_cases"], 2);
        assert_eq!(report["overall"]["trials"], 4);
        assert_eq!(report["overall"]["improved"], 3);
        assert_eq!(report["overall"]["harmed"], 1);
        assert_eq!(report["overall"]["paired_delta"], 0.0);
        assert_eq!(report["overall"]["mean_with_memory"], 0.5);
        assert_eq!(report["answerable"]["paired_delta"], 1.0);
        assert_eq!(report["abstention"]["paired_delta"], -1.0);
        assert_eq!(report["categories"]["test"]["overall"], report["overall"]);
    }

    #[test]
    fn intervals_use_distinct_cases_and_errors_count_as_incorrect() {
        let rows: Vec<_> = (0..100)
            .map(|index| conversation(&index.to_string(), false, false, true, false))
            .collect();
        let run = json!({"rows": rows});
        let once = summarize(std::slice::from_ref(&run));
        let repeated = summarize(&[run.clone(), run]);
        assert_eq!(
            once["overall"]["paired_delta_ci95"],
            repeated["overall"]["paired_delta_ci95"]
        );
        let lower = once["overall"]["paired_delta_ci95"][0].as_f64().unwrap();
        assert!((lower - (1.0 - (2.0 * 40.0_f64.ln() / 100.0).sqrt())).abs() < 1e-12);
        let mut broken = conversation("error", false, true, true, false);
        broken["recall"][0]["rows"][1]["error"] = json!("timeout");
        let result = summarize(&[json!({"rows": [broken]})]);
        assert_eq!(result["overall"]["call_errors"], 1);
        assert_eq!(result["overall"]["harmed"], 1);
        assert!(result["abstention"]["paired_delta_ci95"].is_null());
        assert!(summarize(&[])["overall"]["paired_delta"].is_null());
    }
}
