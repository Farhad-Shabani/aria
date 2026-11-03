//! What describes one daemon run, fixed at startup.

use std::path::{Path, PathBuf};

use aria_platform::delivery::DeliveryFactory;
use aria_platform::runtime::PreparedRuntimePaths;
#[cfg(not(test))]
use aria_settings::config::home_directory;
use aria_settings::config::{ConfigError, UNATTENDED_ENV, unattended};
use aria_voice::dictation::SoundMuting;

use super::config::HotkeyConfig;
use super::host::DaemonLifetime;

const CODEX_HOME_ENV: &str = "CODEX_HOME";

pub(super) struct Context<'runtime> {
    pub(super) paths: &'runtime PreparedRuntimePaths,
    pub(super) settings_path: PathBuf,
    pub(super) home: PathBuf,
    pub(super) codex_home: PathBuf,
    pub(super) delivery: DeliveryFactory,
    pub(super) lifetime: DaemonLifetime,
    pub(super) attended: bool,
}

impl<'runtime> Context<'runtime> {
    /// Reads the process environment once, here, so everything downstream
    /// takes plain values.
    pub(super) fn detect(
        paths: &'runtime PreparedRuntimePaths,
        lifetime: DaemonLifetime,
    ) -> Result<Self, ConfigError> {
        #[cfg(not(test))]
        let home = home_directory()?;
        // A unit-test daemon must never expire or delete the user's history.
        #[cfg(test)]
        let home = paths.spool_dir().to_path_buf();
        // Nor may it play cues or show banners on the developer's machine.
        let attended = !cfg!(test) && attended(unattended()?.as_deref());
        let delivery = DeliveryFactory::detect(&home);
        Ok(Self {
            paths,
            settings_path: aria_settings::default_path(&home),
            codex_home: codex_home(&home),
            home,
            delivery,
            lifetime,
            attended,
        })
    }

    pub(super) fn sound_muting(&self) -> SoundMuting {
        if self.attended {
            SoundMuting::Allowed
        } else {
            SoundMuting::Never
        }
    }

    /// Nobody is at the keyboard of an unattended run, such as a test
    /// sandbox, so a key press there belongs to someone else.
    pub(super) fn hotkey(&self) -> HotkeyConfig {
        if self.attended {
            HotkeyConfig::Enabled
        } else {
            HotkeyConfig::Disabled
        }
    }
}

fn attended(unattended: Option<&str>) -> bool {
    match unattended {
        None | Some("0" | "false" | "") => true,
        Some("1" | "true") => false,
        Some(value) => {
            tracing::warn!(
                value,
                "unrecognized {UNATTENDED_ENV} value; the run counts as attended"
            );
            true
        }
    }
}

/// Where the Codex CLI keeps its login, as it finds it itself.
fn codex_home(home: &Path) -> PathBuf {
    std::env::var_os(CODEX_HOME_ENV).map_or_else(|| home.join(".codex"), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_flag_makes_a_run_unattended() {
        assert!(attended(None));
        assert!(attended(Some("")));
        assert!(attended(Some("0")));
        assert!(attended(Some("false")));
        assert!(!attended(Some("1")));
        assert!(!attended(Some("true")));
        assert!(attended(Some("yes")), "an unrecognized value is attended");
    }

    #[test]
    fn only_an_attended_run_arms_the_hotkey() {
        let directory = tempfile::TempDir::new().expect("the test directory should be created");
        let paths =
            aria_platform::runtime::RuntimePaths::in_directory(directory.path().to_path_buf())
                .prepare()
                .expect("the runtime paths should prepare");
        let mut context =
            Context::detect(&paths, DaemonLifetime::Persistent).expect("a test context");

        context.attended = false;
        assert_eq!(context.hotkey(), HotkeyConfig::Disabled);
        context.attended = true;
        assert_eq!(context.hotkey(), HotkeyConfig::Enabled);
    }
}
