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

/// `after_recording_ms` values from completed-delivery lines: how long the
/// speaker waited after they stopped talking.
pub fn tail_samples(log: &str) -> Vec<u64> {
    let stage_field = "stage=\"delivery\"";
    log.lines()
        .filter(|line| line.contains(STAGE_COMPLETED) && line.contains(stage_field))
        .filter_map(|line| field(line, AFTER_RECORDING_FIELD))
        .collect()
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
