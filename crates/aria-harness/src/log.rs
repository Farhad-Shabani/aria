//! Reading the daemon log's observable timing contract.
//!
//! The daemon reports each pipeline stage and the whole execution with an
//! `elapsed_ms` field. These lines are a public contract of the daemon,
//! like its stderr messages.

const STAGE_COMPLETED: &str = "dictation stage completed";
const EXECUTION_FINISHED: &str = "dictation execution finished";
const DAEMON_READY: &str = "daemon ready";
const ELAPSED_FIELD: &str = "elapsed_ms=";
const AFTER_RECORDING_FIELD: &str = "after_recording_ms=";

/// Whether the daemon has announced it can serve. The socket is bound before
/// the model is loaded, so its existence says only that a connection will be
/// accepted, not answered.
pub(crate) fn is_ready(log: &str) -> bool {
    log.contains(DAEMON_READY)
}

/// The pipeline stages in execution order.
pub const STAGES: [&str; 3] = ["recording", "transcription", "delivery"];

/// `elapsed_ms` values from completed-stage lines for one stage.
pub fn stage_samples(log: &str, stage: &str) -> Vec<u64> {
    let stage_field = format!("stage=\"{stage}\"");
    log.lines()
        .filter(|line| line.contains(STAGE_COMPLETED) && line.contains(&stage_field))
        .filter_map(|line| field(line, ELAPSED_FIELD))
        .collect()
}

/// How long the speaker waited after they stopped talking: per finished
/// execution, the `after_recording_ms` of its last completed stage that logs
/// one.
/// Complete delivery ends on its delivery stage; streaming has typed every
/// segment by the end of transcription and logs no delivery stage.
pub fn tail_samples(log: &str) -> Vec<u64> {
    let mut tails = Vec::new();
    let mut last = None;
    for line in log.lines() {
        if line.contains(STAGE_COMPLETED) {
            if let Some(tail) = field(line, AFTER_RECORDING_FIELD) {
                last = Some(tail);
            }
        } else if line.contains(EXECUTION_FINISHED) {
            tails.extend(last.take());
        }
    }
    tails
}

/// `elapsed_ms` values from finished-execution lines.
pub fn execution_samples(log: &str) -> Vec<u64> {
    log.lines()
        .filter(|line| line.contains(EXECUTION_FINISHED))
        .filter_map(|line| field(line, ELAPSED_FIELD))
        .collect()
}

/// Every distinct stage name the log mentions, in first-seen order. Lets a
/// reader notice stages it does not know about instead of skipping them.
pub fn stage_names(log: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in log.lines() {
        let Some((_, after)) = line.split_once("stage=\"") else {
            continue;
        };
        let Some((name, _)) = after.split_once('"') else {
            continue;
        };
        if !names.iter().any(|known| known == name) {
            names.push(name.to_owned());
        }
    }
    names
}

fn field(line: &str, name: &str) -> Option<u64> {
    let (_, after) = line.split_once(name)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_is_each_executions_last_completed_stage() {
        let log = "\
INFO dictation stage completed stage=\"recording\" status=\"completed\" elapsed_ms=9000
INFO dictation stage completed stage=\"transcription\" status=\"completed\" elapsed_ms=9300 after_recording_ms=300
INFO dictation execution finished status=\"completed\" elapsed_ms=9310
INFO dictation stage completed stage=\"transcription\" status=\"completed\" elapsed_ms=9200 after_recording_ms=150
INFO dictation stage completed stage=\"delivery\" status=\"completed\" elapsed_ms=40 after_recording_ms=190
INFO dictation execution finished status=\"completed\" elapsed_ms=9250
INFO dictation execution finished status=\"failed\" elapsed_ms=10
";
        assert_eq!(tail_samples(log), [300, 190]);
    }
}
