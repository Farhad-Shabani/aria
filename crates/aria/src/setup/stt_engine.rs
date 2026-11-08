//! Sets dictation's speech-to-text engine in place: the engine's own
//! missing models, then the `stt` keys that name it. The running daemon
//! swaps the engine once it sees the write, so nothing restarts.

use std::path::Path;

use aria_settings::CheckedSettings;

use crate::primitives::user_home;

use super::backends::{Host, Stt, choices};
use super::settings_file::{self, SettingsWrite};
use super::{ModelState, Outcome, SetupError, SetupEvent, Step, fetch, lock_setup, model_states};

#[derive(Debug)]
pub struct SetSttEngineSummary {
    pub engine: &'static str,
    pub models: Vec<(&'static str, Outcome)>,
    pub settings: SettingsWrite,
}

pub fn set_stt_engine(
    engine: &str,
    transfer_meter: bool,
    observe: &mut dyn FnMut(SetupEvent) -> Result<(), SetupError>,
) -> Result<SetSttEngineSummary, SetupError> {
    let home = user_home::resolve().map_err(SetupError::Home)?;
    set_in(&home, &Host::probe(), engine, transfer_meter, observe)
}

/// What setting `engine` downloads: its own models not on disk yet.
/// Anything else missing stays a step of setup itself.
pub(super) fn downloads<'a>(
    models: &'a [ModelState],
    engine: &'a str,
) -> impl Iterator<Item = &'a ModelState> {
    models
        .iter()
        .filter(move |model| model.engine == Some(engine) && model.files.needs_download())
}

/// Fetches `engine`'s own models not on disk yet. `STEP_MODELS` starts only
/// when there is something to fetch.
pub(super) fn fetch_engine_models(
    home: &Path,
    file: &CheckedSettings,
    engine: &str,
    transfer_meter: bool,
    observe: &mut dyn FnMut(SetupEvent) -> Result<(), SetupError>,
) -> Result<Vec<(&'static str, Outcome)>, SetupError> {
    let models = model_states(home, file)?;
    let wanted: Vec<&ModelState> = downloads(&models, engine).collect();
    if !wanted.is_empty() {
        observe(SetupEvent::Started(Step::Models))?;
    }
    fetch(
        &aria_settings::config::models_dir(home),
        wanted.into_iter(),
        |_| true,
        transfer_meter,
        observe,
    )
}

fn set_in(
    home: &Path,
    host: &Host,
    engine: &str,
    transfer_meter: bool,
    observe: &mut dyn FnMut(SetupEvent) -> Result<(), SetupError>,
) -> Result<SetSttEngineSummary, SetupError> {
    let _lock = lock_setup(&aria_settings::default_path(home))?;
    let file = settings_file::read(home)?;
    let choice = choices::<Stt>(file.stt.backend.as_deref(), host)
        .into_iter()
        .find(|choice| choice.key == engine)
        .ok_or_else(|| SetupError::UnknownEngine {
            name: engine.to_owned(),
        })?;
    if !choice.settable() {
        return Err(SetupError::NotSettable { name: choice.key });
    }
    settings_file::check_stt(home, &file, choice.key)?;
    let models = fetch_engine_models(home, &file, choice.key, transfer_meter, observe)?;

    observe(SetupEvent::Started(Step::Settings))?;
    let settings = settings_file::write_stt(home, choice.key)?;
    Ok(SetSttEngineSummary {
        engine: choice.key,
        models,
        settings,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use aria_settings::config::backend;
    use aria_settings::{Apply, Key};

    use super::super::assets;
    use super::super::backends::Availability;
    use super::*;

    fn home(settings: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".aria")).unwrap();
        fs::write(aria_settings::default_path(home.path()), settings).unwrap();
        home
    }

    fn with_moonshine_on_disk(home: &Path) {
        let models = aria_settings::config::models_dir(home);
        for (relative, _) in assets::MOONSHINE.files() {
            let path = models.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"").unwrap();
        }
    }

    fn set_engine(
        home: &Path,
        engine: &str,
    ) -> (Result<SetSttEngineSummary, SetupError>, Vec<SetupEvent>) {
        let mut events = Vec::new();
        let host = Host {
            macos: true,
            system_voice: Availability::Ready,
        };
        let result = set_in(home, &host, engine, false, &mut |event| {
            events.push(event);
            Ok(())
        });
        (result, events)
    }

    #[test]
    fn setting_an_engine_writes_only_the_stt_keys_and_keeps_every_other_choice() {
        let home = home(
            "[stt]\nbackend = \"whisper\"\nmodel = \"~/whisper\"\n\n[tts]\nbackend = \"kokoro\"\n\n[wake]\nenabled = true\n",
        );
        with_moonshine_on_disk(home.path());

        let (result, _) = set_engine(home.path(), backend::MOONSHINE);

        assert_eq!(result.unwrap().settings, SettingsWrite::Written);
        let file = settings_file::read(home.path()).unwrap();
        assert_eq!(file.stt.backend.as_deref(), Some(backend::MOONSHINE));
        assert_eq!(file.stt.model, None, "the old engine's model path goes");
        assert_eq!(file.tts.backend.as_deref(), Some("kokoro"));
        assert_eq!(file.wake.enabled, Some(true));
    }

    #[test]
    fn every_key_setting_an_engine_writes_applies_without_a_restart() {
        for key in [
            Key::SttBackend,
            Key::SttModel,
            Key::SttDevice,
            Key::SttInterpreter,
        ] {
            assert_eq!(key.apply(), Apply::Live, "{}", key.path());
        }
    }

    #[test]
    fn setting_an_engine_whose_model_is_on_disk_fetches_nothing() {
        let home = home("[stt]\nbackend = \"whisper\"\n");
        with_moonshine_on_disk(home.path());

        let (result, events) = set_engine(home.path(), backend::MOONSHINE);

        assert!(result.unwrap().models.is_empty());
        assert!(matches!(
            events.as_slice(),
            [SetupEvent::Started(Step::Settings)]
        ));
    }

    #[test]
    fn an_engine_that_cant_be_set_in_place_is_refused_before_anything_changes() {
        let settings = "[stt]\nbackend = \"whisper\"\n";
        let home = home(settings);

        let (fetches_its_own, events) = set_engine(home.path(), backend::PARAKEET_MLX);
        let (server, _) = set_engine(home.path(), backend::SERVER);

        assert!(matches!(
            fetches_its_own,
            Err(SetupError::NotSettable { .. })
        ));
        assert!(matches!(server, Err(SetupError::UnknownEngine { .. })));
        assert!(events.is_empty());
        assert_eq!(
            fs::read_to_string(aria_settings::default_path(home.path())).unwrap(),
            settings
        );
    }

    #[test]
    fn only_the_engines_own_missing_models_download() {
        let mac = home("");
        with_moonshine_on_disk(mac.path());
        let file = settings_file::read(mac.path()).unwrap();
        let models = model_states(mac.path(), &file).unwrap();

        let names = |engine| {
            downloads(&models, engine)
                .map(|model| model.name)
                .collect::<Vec<_>>()
        };

        assert!(names(backend::MOONSHINE).is_empty());
        assert_eq!(names(backend::NEMOTRON), ["nemotron"]);

        let elsewhere = home("[stt]\nbackend = \"nemotron\"\nmodel = \"/elsewhere/nemotron\"\n");
        let file = settings_file::read(elsewhere.path()).unwrap();
        let models = model_states(elsewhere.path(), &file).unwrap();
        assert_eq!(downloads(&models, backend::NEMOTRON).count(), 0);
    }
}
