//! The five choices setup owns inside `~/.aria/config.yaml`.

use std::fs;
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::config::{self, FileConfig};

use super::SetupError;

const HEADER: &str = "# Written by `aria setup`. Hand edits are kept; a rerun that changes an\n\
                      # answer rewrites this file and leaves the old one at config.yaml.bak.\n";
/// The mode a hand-written config would have. The temporary file starts at
/// 0600, which would silently tighten what the user already chose.
const CONFIG_FILE_MODE: u32 = 0o644;

/// The five choices as the file carries them, whether or not setup knows
/// them. A name it does not know still belongs to the user.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Existing {
    pub stt: Option<String>,
    pub tts: Option<String>,
    pub chat: Option<String>,
    pub wake: Option<bool>,
    pub hotkey: Option<bool>,
}

/// The answers, as config.yaml spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selections {
    pub stt: String,
    pub tts: String,
    pub chat: String,
    pub wake: bool,
    pub hotkey: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWrite {
    Unchanged,
    Written { backup: Option<PathBuf> },
}

impl Existing {
    pub(super) fn read(path: &Path) -> Result<Self, SetupError> {
        let file = read(path)?;
        Ok(Self {
            stt: file.transcription.backend,
            tts: file.tts.backend,
            chat: file.chat.agent,
            wake: file.wake.enabled,
            hotkey: file.hotkey.enabled,
        })
    }
}

impl Selections {
    /// Rewrites the file with these five keys set and everything else as it
    /// was. Comments do not survive, so an unchanged result is not written,
    /// and the first rewrite of a hand-written file keeps it beside.
    pub(super) fn write(&self, path: &Path) -> Result<ConfigWrite, SetupError> {
        // A symlinked config belongs to whatever it points at; renaming onto
        // the link would leave that file stale and untouched.
        let path = &fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let mut file = read(path)?;
        file.transcription.backend = Some(self.stt.clone());
        file.tts.backend = Some(self.tts.clone());
        file.chat.agent = Some(self.chat.clone());
        file.wake.enabled = Some(self.wake);
        file.hotkey.enabled = Some(self.hotkey);
        let text = format!(
            "{HEADER}{}",
            serde_saphyr::to_string(&file).map_err(|source| SetupError::ConfigWrite {
                path: path.to_path_buf(),
                source,
            })?
        );

        let previous = fs::read_to_string(path).ok();
        if previous.as_deref() == Some(text.as_str()) {
            return Ok(ConfigWrite::Unchanged);
        }
        // Only the file setup did not write is worth keeping: a second
        // backup of its own output would bury the user's original.
        let backup = previous
            .filter(|previous| !previous.starts_with(HEADER))
            .map(|_| path.with_extension("yaml.bak"));
        if let Some(backup) = &backup {
            fs::copy(path, backup).map_err(|source| io_error("back up", backup, source))?;
        }
        replace(path, &text)?;
        Ok(ConfigWrite::Written { backup })
    }
}

fn read(path: &Path) -> Result<FileConfig, SetupError> {
    config::read_file(path).map_err(|source| SetupError::Config {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes through a sibling temporary file, so a crash leaves the old
/// config or the new one, never half of either. The rename is durable only
/// once the directory itself is synced.
fn replace(path: &Path, text: &str) -> Result<(), SetupError> {
    let directory = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(directory).map_err(|source| io_error("create", directory, source))?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)
        .map_err(|source| io_error("create a temporary for", path, source))?;
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(CONFIG_FILE_MODE))
        .and_then(|()| temporary.write_all(text.as_bytes()))
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| io_error("write", path, source))?;
    temporary
        .persist(path)
        .map_err(|error| io_error("replace", path, error.error))?;
    fs::File::open(directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error("flush", directory, source))
}

fn io_error(action: &'static str, path: &Path, source: std::io::Error) -> SetupError {
    SetupError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selections() -> Selections {
        Selections {
            stt: "parakeet".to_owned(),
            tts: "kokoro".to_owned(),
            chat: "codex".to_owned(),
            wake: true,
            hotkey: false,
        }
    }

    #[test]
    fn what_is_written_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".aria/config.yaml");

        let first = selections().write(&path).unwrap();
        let read = Existing::read(&path).unwrap();

        assert_eq!(first, ConfigWrite::Written { backup: None });
        assert_eq!(
            read,
            Existing {
                stt: Some("parakeet".to_owned()),
                tts: Some("kokoro".to_owned()),
                chat: Some("codex".to_owned()),
                wake: Some(true),
                hotkey: Some(false),
            }
        );
    }

    #[test]
    fn an_unchanged_rerun_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        selections().write(&path).unwrap();
        let written = fs::metadata(&path).unwrap().modified().unwrap();

        let again = selections().write(&path).unwrap();

        assert_eq!(again, ConfigWrite::Unchanged);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), written);
        assert!(!path.with_extension("yaml.bak").exists());
    }

    #[test]
    fn a_change_keeps_the_old_text_and_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let hand_written = "tts:\n  backend: system\n  rate: 1.5\n";
        fs::write(&path, hand_written).unwrap();

        let write = selections().write(&path).unwrap();

        // The reported path is the resolved one, which on macOS means
        // /private/var rather than the /var symlink a tempdir hands out.
        let backup = fs::canonicalize(&path).unwrap().with_extension("yaml.bak");
        assert_eq!(
            write,
            ConfigWrite::Written {
                backup: Some(backup.clone())
            }
        );
        assert_eq!(fs::read_to_string(backup).unwrap(), hand_written);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("rate: 1.5"), "{text}");
        assert!(text.contains("backend: kokoro"), "{text}");
        assert!(!text.contains("{}"), "empty sections are left out: {text}");
    }

    #[test]
    fn a_later_change_keeps_the_hand_written_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let hand_written = "# mine\ntts:\n  backend: system\n";
        fs::write(&path, hand_written).unwrap();
        selections().write(&path).unwrap();

        let mut second = selections();
        second.wake = false;
        second.write(&path).unwrap();

        assert_eq!(
            fs::read_to_string(path.with_extension("yaml.bak")).unwrap(),
            hand_written,
            "setup's own output must not bury the user's original"
        );
    }

    #[test]
    fn a_rewrite_keeps_the_mode_a_hand_written_file_has() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");

        selections().write(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, CONFIG_FILE_MODE, "{mode:o}");
    }

    #[test]
    fn a_symlinked_config_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/config.yaml");
        fs::create_dir_all(real.parent().unwrap()).unwrap();
        fs::write(&real, "tts:\n  backend: system\n").unwrap();
        let link = dir.path().join("config.yaml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        selections().write(&link).unwrap();

        assert!(link.symlink_metadata().unwrap().is_symlink(), "link stands");
        assert!(
            fs::read_to_string(&real)
                .unwrap()
                .contains("backend: kokoro"),
            "the real file is the one written"
        );
    }

    #[test]
    fn an_unknown_value_is_read_as_it_stands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "transcription:\n  backend: futuristic\n").unwrap();

        assert_eq!(
            Existing::read(&path).unwrap().stt,
            Some("futuristic".to_owned())
        );
    }
}
