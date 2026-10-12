//! The Linux voice program: `espeak-ng`, or its older name `espeak`.

use std::path::{Path, PathBuf};
use std::process::Command;

use aria_kit::synthesis::Voice;

use super::MissingProgram;

// Fixed absolute candidates rather than a PATH search, so what runs is not up
// to the caller's environment.
const CANDIDATES: [&str; 2] = ["/usr/bin/espeak-ng", "/usr/bin/espeak"];

pub(super) fn program() -> Result<PathBuf, MissingProgram> {
    let candidates: Vec<&Path> = CANDIDATES.iter().map(Path::new).collect();
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_path_buf())
        .ok_or_else(|| MissingProgram::new(&candidates))
}

pub(super) fn command(program: &Path, voice: Option<&Voice>, words_per_minute: u32) -> Command {
    let mut command = Command::new(program);
    command
        .arg("--stdin")
        .arg("-s")
        .arg(words_per_minute.to_string());
    if let Some(voice) = voice {
        command.arg("-v").arg(voice.as_str());
    }
    command
}
