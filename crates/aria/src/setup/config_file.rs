//! What Aria writes inside `~/.aria/config.yaml`: setup's five choices, and
//! the phone pairing switch.

use std::fs;
use std::fs::Permissions;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::config::{self, FileConfig, ServerEndpoint, backend};

use super::SetupError;

const HEADER: &str = "# Written by Aria. Hand edits are kept, but comments are not.\n\
                      # A hand-written original is kept at config.yaml.bak.\n";
/// Every header Aria has written starts this way, so a file from an older
/// release isn't taken for a hand-written one.
const WRITTEN_BY_ARIA: &str = "# Written by ";
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
    pub(super) fn of(file: &FileConfig) -> Self {
        Self {
            stt: file.transcription.backend.clone(),
            tts: file.tts.backend.clone(),
            chat: file.chat.agent.clone(),
            wake: file.wake.enabled,
            hotkey: file.hotkey.enabled,
        }
    }
}

pub(super) struct PreparedConfig {
    path: PathBuf,
    previous: Option<String>,
    /// `None` when the rewrite matches the file.
    staged: Option<NamedTempFile>,
}

impl Selections {
    pub(super) fn prepare(
        &self,
        path: &Path,
        paired_server: Option<&ServerEndpoint>,
    ) -> Result<PreparedConfig, SetupError> {
        prepare_rewrite(path, |file| self.apply_to(file, paired_server))
    }

    /// Writes the answers into `file`. Switching a section's engine clears its
    /// engine-specific keys, so the new engine never gets the old model path.
    pub(super) fn apply_to(&self, file: &mut FileConfig, paired_server: Option<&ServerEndpoint>) {
        if config::transcription_engine(file) != self.stt {
            file.transcription.model = None;
            file.transcription.device = None;
            file.transcription.interpreter = None;
            file.transcription.server = None;
        }
        if config::tts_engine(file) != self.tts {
            file.tts.model = None;
            file.tts.voices = None;
            file.tts.tokenizer = None;
        }
        if self.stt == backend::SERVER
            && let Some(endpoint) = paired_server
        {
            file.transcription.server = Some(endpoint.to_string());
        }
        file.transcription.backend = Some(self.stt.clone());
        file.tts.backend = Some(self.tts.clone());
        file.chat.agent = Some(self.chat.clone());
        file.wake.enabled = Some(self.wake);
        file.hotkey.enabled = Some(self.hotkey);
    }
}

impl PreparedConfig {
    pub(super) fn commit(self) -> Result<ConfigWrite, SetupError> {
        let Self {
            path,
            previous,
            staged,
        } = self;
        let Some(staged) = staged else {
            return Ok(ConfigWrite::Unchanged);
        };
        // Only a file Aria did not write is worth keeping: a second backup of
        // Aria's own output would bury the user's original.
        let backup = previous
            .filter(|previous| !previous.starts_with(WRITTEN_BY_ARIA))
            .map(|_| path.with_extension("yaml.bak"));
        if let Some(backup) = &backup {
            fs::copy(&path, backup).map_err(|source| io_error("back up", backup, source))?;
        }
        staged
            .persist(&path)
            .map_err(|error| io_error("replace", &path, error.error))?;
        // The rename is durable only once the directory itself is synced.
        let directory = path.parent().unwrap_or(Path::new("."));
        fs::File::open(directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| io_error("flush", directory, source))?;
        Ok(ConfigWrite::Written { backup })
    }
}

pub(super) fn set_phone_pairing(path: &Path, enabled: bool) -> Result<ConfigWrite, SetupError> {
    // Off is the default, so a hand-written file that never mentions phone
    // pairing is left alone, comments and all.
    if read(path)?.phone.enabled.unwrap_or(false) == enabled {
        return Ok(ConfigWrite::Unchanged);
    }
    prepare_rewrite(path, |file| file.phone.enabled = Some(enabled))?.commit()
}

/// Comments don't survive a rewrite. So an unchanged result isn't staged,
/// and a hand-written file is first copied to config.yaml.bak.
fn prepare_rewrite(
    path: &Path,
    edit: impl FnOnce(&mut FileConfig),
) -> Result<PreparedConfig, SetupError> {
    // A symlinked config belongs to whatever it points at; renaming onto
    // the link would leave that file stale and untouched.
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut file = read(&path)?;
    edit(&mut file);
    let text = format!(
        "{HEADER}{}",
        serde_saphyr::to_string(&file).map_err(|source| SetupError::ConfigWrite {
            path: path.clone(),
            source,
        })?
    );
    let previous = fs::read_to_string(&path).ok();
    let staged = if previous.as_deref() == Some(text.as_str()) {
        None
    } else {
        Some(stage(&path, &text)?)
    };
    Ok(PreparedConfig {
        path,
        previous,
        staged,
    })
}

pub(super) fn read(path: &Path) -> Result<FileConfig, SetupError> {
    config::read_file(path).map_err(|source| SetupError::Config {
        path: path.to_path_buf(),
        source,
    })
}

/// Writes to a sibling temporary file, so a crash leaves the old config or
/// the new one, never half of either.
fn stage(path: &Path, text: &str) -> Result<NamedTempFile, SetupError> {
    let directory = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(directory).map_err(|source| io_error("create", directory, source))?;
    let mut temporary = NamedTempFile::new_in(directory)
        .map_err(|source| io_error("create a temporary for", path, source))?;
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(CONFIG_FILE_MODE))
        .and_then(|()| temporary.write_all(text.as_bytes()))
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| io_error("write", path, source))?;
    Ok(temporary)
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

        let first = selections().prepare(&path, None).unwrap().commit().unwrap();
        let read = Existing::of(&read(&path).unwrap());

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
    fn server_selection_takes_the_paired_endpoint_and_local_selection_clears_it() {
        let mut file: FileConfig = serde_saphyr::from_str(
            "transcription: {backend: server, server: https://example.test:9851}",
        )
        .unwrap();
        let mut chosen = selections();
        chosen.stt = "server".to_owned();
        let paired = ServerEndpoint::parse("https://paired.test:9851").unwrap();
        chosen.apply_to(&mut file, Some(&paired));
        assert_eq!(
            file.transcription.server.as_deref(),
            Some("https://paired.test:9851")
        );
        chosen.stt = "parakeet".to_owned();
        chosen.apply_to(&mut file, Some(&paired));
        assert_eq!(file.transcription.server, None);
    }

    #[test]
    fn a_new_engine_drops_the_old_engine_paths() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(
            &path,
            "transcription: {backend: whisper, model: ~/w.bin}\n\
             tts: {backend: kokoro, model: ~/k.onnx}\n",
        )
        .unwrap();

        selections().prepare(&path, None).unwrap().commit().unwrap();
        let file = read(&path).unwrap();

        assert_eq!(file.transcription.model, None, "parakeet replaced whisper");
        assert_eq!(file.tts.model.as_deref(), Some("~/k.onnx"), "kokoro stayed");
    }

    /// No backend key means the default engine, so picking it is no switch.
    #[test]
    fn the_default_engine_keeps_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "transcription: {model: ~/tdt}\n").unwrap();

        let default_engine = Selections {
            stt: config::backend::DEFAULT.to_owned(),
            ..selections()
        };
        default_engine
            .prepare(&path, None)
            .unwrap()
            .commit()
            .unwrap();

        assert_eq!(
            read(&path).unwrap().transcription.model.as_deref(),
            Some("~/tdt")
        );
    }

    #[test]
    fn an_unchanged_rerun_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        selections().prepare(&path, None).unwrap().commit().unwrap();
        let written = fs::metadata(&path).unwrap().modified().unwrap();

        let again = selections().prepare(&path, None).unwrap().commit().unwrap();

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

        let write = selections().prepare(&path, None).unwrap().commit().unwrap();

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
        selections().prepare(&path, None).unwrap().commit().unwrap();

        let mut second = selections();
        second.wake = false;
        second.prepare(&path, None).unwrap().commit().unwrap();

        assert_eq!(
            fs::read_to_string(path.with_extension("yaml.bak")).unwrap(),
            hand_written,
            "setup's own output must not bury the user's original"
        );
    }

    #[test]
    fn a_file_an_older_release_wrote_keeps_the_hand_written_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let hand_written = "# mine\ntts:\n  backend: system\n";
        fs::write(path.with_extension("yaml.bak"), hand_written).unwrap();
        fs::write(
            &path,
            "# Written by `aria setup`. Hand edits are kept; a rerun that changes an\n\
             # answer rewrites this file and leaves the old one at config.yaml.bak.\n\
             tts:\n  backend: system\n",
        )
        .unwrap();

        set_phone_pairing(&path, true).unwrap();

        assert_eq!(
            fs::read_to_string(path.with_extension("yaml.bak")).unwrap(),
            hand_written
        );
    }

    #[test]
    fn a_rewrite_keeps_the_mode_a_hand_written_file_has() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");

        selections().prepare(&path, None).unwrap().commit().unwrap();

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

        selections().prepare(&link, None).unwrap().commit().unwrap();

        assert!(link.symlink_metadata().unwrap().is_symlink(), "link stands");
        assert!(
            fs::read_to_string(&real)
                .unwrap()
                .contains("backend: kokoro"),
            "the real file is the one written"
        );
    }

    #[test]
    fn enabling_phone_pairing_fills_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "").unwrap();

        set_phone_pairing(&path, true).unwrap();

        assert_eq!(read(&path).unwrap().phone.enabled, Some(true));
    }

    #[test]
    fn turning_phone_pairing_off_when_unset_touches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        let hand_written = "# mine\ntts:\n  backend: system\n";
        fs::write(&path, hand_written).unwrap();

        assert_eq!(
            set_phone_pairing(&path, false).unwrap(),
            ConfigWrite::Unchanged
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), hand_written);
    }

    #[test]
    fn turning_phone_pairing_off_keeps_its_port() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "phone:\n  enabled: true\n  port: 9999\n").unwrap();

        set_phone_pairing(&path, false).unwrap();

        let file = read(&path).unwrap();
        assert_eq!(file.phone.enabled, Some(false));
        assert_eq!(file.phone.port, Some(9999));
    }

    #[test]
    fn enabling_phone_pairing_keeps_its_port_and_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "phone:\n  port: 9999\ntts:\n  rate: 1.5\n").unwrap();

        set_phone_pairing(&path, true).unwrap();

        let file = read(&path).unwrap();
        assert_eq!(file.phone.enabled, Some(true));
        assert_eq!(file.phone.port, Some(9999));
        assert!(fs::read_to_string(&path).unwrap().contains("rate: 1.5"));
    }

    #[test]
    fn an_unknown_value_is_read_as_it_stands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        fs::write(&path, "transcription:\n  backend: futuristic\n").unwrap();

        assert_eq!(
            Existing::of(&read(&path).unwrap()).stt,
            Some("futuristic".to_owned())
        );
    }
}
