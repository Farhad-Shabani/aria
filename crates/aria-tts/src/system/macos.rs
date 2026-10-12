//! The macOS voice program: `say`, part of every install.

use std::path::{Path, PathBuf};
use std::process::Command;

use aria_kit::synthesis::Voice;

use super::MissingProgram;

const SAY: &str = "/usr/bin/say";

pub(super) fn program() -> Result<PathBuf, MissingProgram> {
    let say = Path::new(SAY);
    if say.is_file() {
        Ok(say.to_path_buf())
    } else {
        Err(MissingProgram::new(&[say]))
    }
}

/// `say` reads the text from stdin when none is given as an argument.
pub(super) fn command(program: &Path, voice: Option<&Voice>, words_per_minute: u32) -> Command {
    let mut command = Command::new(program);
    command.arg("-r").arg(words_per_minute.to_string());
    if let Some(voice) = voice {
        command.arg("-v").arg(voice.as_str());
    }
    command
}
