//! Sets where dictation recognizes speech: on this machine, or on the paired
//! server. Only `stt.runs_on` changes, so the local engine and its keys
//! stay for the way back. The running daemon swaps once it sees the write,
//! so nothing restarts.

use std::path::Path;

use aria_settings::RunsOn;

use crate::primitives::user_home;

use super::settings_file::{self, SettingsWrite};
use super::stt_engine::fetch_engine_models;
use super::{Outcome, Pairing, SetupError, SetupEvent, Step, lock_setup};

#[derive(Debug)]
pub struct SetSttRunsOnSummary {
    pub runs_on: RunsOn,
    pub models: Vec<(&'static str, Outcome)>,
    pub settings: SettingsWrite,
}

pub fn set_stt_runs_on(
    runs_on: RunsOn,
    transfer_meter: bool,
    observe: &mut dyn FnMut(SetupEvent) -> Result<(), SetupError>,
) -> Result<SetSttRunsOnSummary, SetupError> {
    let home = user_home::resolve().map_err(SetupError::Home)?;
    set_in(&home, runs_on, transfer_meter, observe)
}

/// Coming back fetches the local engine's missing models before the write,
/// so the daemon never swaps to an engine that can't load.
fn set_in(
    home: &Path,
    runs_on: RunsOn,
    transfer_meter: bool,
    observe: &mut dyn FnMut(SetupEvent) -> Result<(), SetupError>,
) -> Result<SetSttRunsOnSummary, SetupError> {
    let _lock = lock_setup(&aria_settings::default_path(home))?;
    let file = settings_file::read(home)?;
    settings_file::check_runs_on(home, &file, runs_on)?;
    let models = match runs_on {
        RunsOn::Server => {
            Pairing::read(home).check_paired(home)?;
            Vec::new()
        }
        RunsOn::Local => {
            let engine = aria_settings::config::stt_engine(&file.stt);
            fetch_engine_models(home, &file, engine, transfer_meter, observe)?
        }
    };

    observe(SetupEvent::Started(Step::Settings))?;
    let settings = settings_file::write_runs_on(home, runs_on)?;
    Ok(SetSttRunsOnSummary {
        runs_on,
        models,
        settings,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn home(settings: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".aria")).unwrap();
        fs::write(aria_settings::default_path(home.path()), settings).unwrap();
        home
    }

    fn set(
        home: &Path,
        runs_on: RunsOn,
    ) -> (Result<SetSttRunsOnSummary, SetupError>, Vec<SetupEvent>) {
        let mut events = Vec::new();
        let result = set_in(home, runs_on, false, &mut |event| {
            events.push(event);
            Ok(())
        });
        (result, events)
    }

    #[test]
    fn a_round_trip_to_the_server_keeps_the_local_engine_and_its_settings() {
        let local =
            "[stt]\nbackend = \"parakeet\"\nmodel = \"/models/parakeet\"\ndevice = \"gpu\"\n";
        let home = home(local);
        aria_daemon::pair_server_for_tests(home.path(), "https://box.test:9851");

        let (server, events) = set(home.path(), RunsOn::Server);

        assert_eq!(server.unwrap().settings, SettingsWrite::Written);
        assert!(matches!(
            events.as_slice(),
            [SetupEvent::Started(Step::Settings)]
        ));
        let file = settings_file::read(home.path()).unwrap();
        assert_eq!(file.choices().stt_runs_on, RunsOn::Server);

        let (back, events) = set(home.path(), RunsOn::Local);

        assert!(back.unwrap().models.is_empty(), "the model lives elsewhere");
        assert!(matches!(
            events.as_slice(),
            [SetupEvent::Started(Step::Settings)]
        ));
        let file = settings_file::read(home.path()).unwrap();
        assert_eq!(file.choices().stt_runs_on, RunsOn::Local);
        assert_eq!(file.stt.backend.as_deref(), Some("parakeet"));
        assert_eq!(file.stt.model.as_deref(), Some("/models/parakeet"));
        assert!(file.stt.device.is_some());
    }

    #[test]
    fn the_server_without_a_pairing_is_refused_before_anything_changes() {
        let settings = "[stt]\nbackend = \"moonshine\"\n";
        let home = home(settings);

        let (result, events) = set(home.path(), RunsOn::Server);

        assert!(matches!(result, Err(SetupError::ServerNotPaired)));
        assert!(events.is_empty());
        assert_eq!(
            fs::read_to_string(aria_settings::default_path(home.path())).unwrap(),
            settings
        );
    }

    /// The download itself needs the network, so the observer stops the run
    /// where it starts.
    #[test]
    fn coming_back_fetches_the_local_model_before_the_write() {
        let settings = "[stt]\nbackend = \"moonshine\"\nruns_on = \"server\"\n";
        let home = home(settings);
        let mut events = Vec::new();

        let result = set_in(home.path(), RunsOn::Local, false, &mut |event| {
            events.push(event);
            Err(SetupError::Invalid("stop before the download"))
        });

        assert!(result.is_err());
        assert!(matches!(
            events.as_slice(),
            [SetupEvent::Started(Step::Models)]
        ));
        assert_eq!(
            fs::read_to_string(aria_settings::default_path(home.path())).unwrap(),
            settings
        );
    }
}
