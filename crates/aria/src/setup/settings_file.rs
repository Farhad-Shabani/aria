//! What Aria writes into `~/.aria/settings.toml` for setup: its four choices,
//! each a key write that keeps the rest of the file as it is.

use std::path::Path;

use crate::config::{self, ServerEndpoint, backend, tts_backend};
use crate::settings::{
    self, AriaKeyName, Key, Patch, Refusal, SettingsError, SettingsFile, Written,
};

use super::SetupError;

/// The four choices as the file carries them, whether or not setup knows
/// them. A name it does not know still belongs to the user.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Existing {
    pub stt: Option<String>,
    pub tts: Option<String>,
    pub wake: Option<bool>,
    pub aria_key: Option<bool>,
}

/// The answers, as settings.toml spells them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selections {
    pub stt: String,
    pub tts: String,
    pub wake: bool,
    pub aria_key: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsWrite {
    Unchanged,
    Written,
}

impl Existing {
    pub(super) fn of(settings: &SettingsFile) -> Self {
        Self {
            stt: settings.stt.backend.clone(),
            tts: settings.tts.backend.clone(),
            wake: settings.wake.enabled,
            aria_key: settings
                .shortcuts
                .aria_key
                .map(|key| key != AriaKeyName::Off),
        }
    }
}

impl Selections {
    /// Checks the answers against the file without writing, so a refused
    /// write fails before any download.
    pub(super) fn check(
        &self,
        home: &Path,
        paired_server: Option<&ServerEndpoint>,
    ) -> Result<(), SetupError> {
        let current = read(home)?;
        let patch = self.patch(&current, paired_server)?;
        accepted(settings::check_patch(
            &settings::default_path(home),
            home,
            &patch,
        )?)?;
        Ok(())
    }

    pub(super) fn write(
        &self,
        home: &Path,
        paired_server: Option<&ServerEndpoint>,
    ) -> Result<SettingsWrite, SetupError> {
        let mut changed = false;
        let written = settings::modify(&settings::default_path(home), home, None, |current| {
            let patch = self.patch(current, paired_server)?;
            changed = !patch.is_empty();
            Ok::<_, SettingsError>(patch)
        })?;
        accepted(written)?;
        Ok(if changed {
            SettingsWrite::Written
        } else {
            SettingsWrite::Unchanged
        })
    }

    /// The keys whose values change. Switching transcription engines resets
    /// that engine's own keys, so the new engine never gets the old model
    /// path. Speech engines keep their files in their own tables.
    pub(super) fn patch(
        &self,
        current: &SettingsFile,
        paired_server: Option<&ServerEndpoint>,
    ) -> Result<Patch, SettingsError> {
        let mut patch = Patch::new();
        if config::transcription_engine(&current.stt) != self.stt {
            for key in [
                Key::SttModel,
                Key::SttDevice,
                Key::SttInterpreter,
                Key::SttServer,
            ] {
                patch.reset(key);
            }
        }
        if current.stt.backend.as_deref() != Some(self.stt.as_str()) {
            patch.set(Key::SttBackend, self.stt.as_str());
        }
        if self.stt == backend::SERVER
            && let Some(endpoint) = paired_server
            && current.stt.server.as_deref() != Some(endpoint.to_string().as_str())
        {
            patch.set(Key::SttServer, endpoint.to_string());
        }
        if current.tts.backend.as_deref() != Some(self.tts.as_str()) {
            patch.set(Key::TtsBackend, self.tts.as_str());
        }
        // The system engine takes no fallback.
        if self.tts == tts_backend::SYSTEM && current.tts.fallback.is_some() {
            patch.reset(Key::TtsFallback);
        }
        if current.wake.enabled != Some(self.wake) {
            patch.set(Key::WakeEnabled, self.wake);
        }
        let key_is_on = current.shortcuts.aria_key != Some(AriaKeyName::Off);
        match (self.aria_key, key_is_on) {
            (false, true) => {
                patch.set_encoded(Key::ShortcutsAriaKey, &AriaKeyName::Off)?;
            }
            (true, false) => {
                patch.set_encoded(Key::ShortcutsAriaKey, &AriaKeyName::RightOption)?;
            }
            (true, true) | (false, false) => {}
        }
        Ok(patch)
    }
}

pub(super) fn read(home: &Path) -> Result<SettingsFile, SetupError> {
    settings::load(home).map_err(SetupError::Settings)
}

fn accepted(written: Written) -> Result<(), SetupError> {
    match written {
        Written::Applied(_) | Written::AppliedRestartKey(_) => Ok(()),
        Written::Refused(Refusal::FileInvalid(error) | Refusal::Invalid(error)) => {
            Err(SetupError::SettingsRefused(error))
        }
        // Setup writes against the file as it stands, with no revision, so a
        // conflict here is the file moving under the write as well.
        Written::Refused(Refusal::ChangedRecently | Refusal::Conflict { .. }) => {
            Err(SetupError::SettingsChanging)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn selections() -> Selections {
        Selections {
            stt: "parakeet".to_owned(),
            tts: "kokoro".to_owned(),
            wake: true,
            aria_key: false,
        }
    }

    fn home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".aria")).unwrap();
        home
    }

    fn settings_path(home: &Path) -> std::path::PathBuf {
        settings::default_path(home)
    }

    #[test]
    fn what_is_written_reads_back() {
        let home = home();

        let first = selections().write(home.path(), None).unwrap();
        let read = Existing::of(&read(home.path()).unwrap());

        assert_eq!(first, SettingsWrite::Written);
        assert_eq!(
            read,
            Existing {
                stt: Some("parakeet".to_owned()),
                tts: Some("kokoro".to_owned()),
                wake: Some(true),
                aria_key: Some(false),
            }
        );
    }

    #[test]
    fn setup_writes_no_assistant() {
        let home = home();

        selections().write(home.path(), None).unwrap();

        let text = fs::read_to_string(settings_path(home.path())).unwrap();
        assert!(!text.contains("assistant"), "{text}");
    }

    #[test]
    fn a_rewrite_keeps_comments_and_other_keys() {
        let home = home();
        let hand_written = "# mine\n[assistant]\nid = \"codex\" # my pick\n";
        fs::write(settings_path(home.path()), hand_written).unwrap();

        selections().write(home.path(), None).unwrap();

        let text = fs::read_to_string(settings_path(home.path())).unwrap();
        assert!(text.starts_with(hand_written), "{text}");
        assert_eq!(
            read(home.path()).unwrap().assistant.id.as_deref(),
            Some("codex")
        );
    }

    #[test]
    fn server_selection_takes_the_paired_endpoint_and_local_selection_clears_it() {
        let current = SettingsFile::default();
        let mut chosen = selections();
        chosen.stt = "server".to_owned();
        let paired = ServerEndpoint::parse("https://paired.test:9851").unwrap();
        let keys: Vec<Key> = chosen
            .patch(&current, Some(&paired))
            .unwrap()
            .keys()
            .collect();
        assert!(keys.contains(&Key::SttServer), "{keys:?}");

        let home = home();
        fs::write(
            settings_path(home.path()),
            "[stt]\nbackend = \"server\"\nserver = \"https://paired.test:9851\"\n",
        )
        .unwrap();
        selections().write(home.path(), None).unwrap();
        assert_eq!(read(home.path()).unwrap().stt.server, None);
    }

    #[test]
    fn a_new_engine_drops_the_old_engine_paths() {
        let home = home();
        fs::write(
            settings_path(home.path()),
            "[stt]\nbackend = \"whisper\"\nmodel = \"~/w.bin\"\n\
             [tts]\nbackend = \"kokoro\"\n[tts.kokoro]\nmodel = \"~/k.onnx\"\n",
        )
        .unwrap();

        selections().write(home.path(), None).unwrap();
        let settings = read(home.path()).unwrap();

        assert_eq!(settings.stt.model, None, "parakeet replaced whisper");
        assert!(settings.tts.kokoro.model.is_some(), "kokoro stayed");
    }

    #[test]
    fn choosing_the_system_voice_drops_the_fallback() {
        let home = home();
        fs::write(
            settings_path(home.path()),
            "[tts]\nbackend = \"kokoro\"\nfallback = \"system\"\n",
        )
        .unwrap();
        let system = Selections {
            tts: "system".to_owned(),
            ..selections()
        };

        system.check(home.path(), None).unwrap();
        system.write(home.path(), None).unwrap();

        assert_eq!(read(home.path()).unwrap().tts.fallback, None);
    }

    /// No backend key means the default engine, so picking it is no switch.
    #[test]
    fn the_default_engine_keeps_its_path() {
        let home = home();
        fs::write(settings_path(home.path()), "[stt]\nmodel = \"~/tdt\"\n").unwrap();

        let default_engine = Selections {
            stt: config::backend::DEFAULT.to_owned(),
            ..selections()
        };
        default_engine.write(home.path(), None).unwrap();

        assert!(read(home.path()).unwrap().stt.model.is_some());
    }

    #[test]
    fn an_unchanged_rerun_touches_nothing() {
        let home = home();
        selections().write(home.path(), None).unwrap();
        let written = fs::metadata(settings_path(home.path()))
            .unwrap()
            .modified()
            .unwrap();

        let again = selections().write(home.path(), None).unwrap();

        assert_eq!(again, SettingsWrite::Unchanged);
        assert_eq!(
            fs::metadata(settings_path(home.path()))
                .unwrap()
                .modified()
                .unwrap(),
            written
        );
    }

    #[test]
    fn an_invalid_file_is_refused_and_left_alone() {
        let home = home();
        let broken = "[tts]\nbackend = \"futuristic\"\n";
        fs::write(settings_path(home.path()), broken).unwrap();

        assert!(matches!(
            selections().check(home.path(), None),
            Err(SetupError::Settings(_))
        ));
        assert_eq!(
            fs::read_to_string(settings_path(home.path())).unwrap(),
            broken
        );
    }

    #[test]
    fn turning_the_aria_key_back_on_names_the_default_key() {
        let current = read_text("[shortcuts]\naria_key = \"off\"\n");
        let on = Selections {
            aria_key: true,
            ..selections()
        };
        let keys: Vec<Key> = on.patch(&current, None).unwrap().keys().collect();
        assert!(keys.contains(&Key::ShortcutsAriaKey), "{keys:?}");

        let fn_key = read_text("[shortcuts]\naria_key = \"fn\"\n");
        let keys: Vec<Key> = on.patch(&fn_key, None).unwrap().keys().collect();
        assert!(
            !keys.contains(&Key::ShortcutsAriaKey),
            "a chosen key stays: {keys:?}"
        );
    }

    fn read_text(text: &str) -> SettingsFile {
        settings::Loaded::parse(text, Path::new("/Users/you"))
            .unwrap()
            .settings
    }
}
