//! What describes one daemon run, fixed at startup.

use std::path::PathBuf;

use crate::config::{ConfigError, Environment, UNATTENDED_ENV};
use crate::preferences::PreferenceStore;
use crate::runtime::PreparedRuntimePaths;

use super::host::DaemonLifetime;

pub(super) struct Context<'runtime> {
    pub(super) paths: &'runtime PreparedRuntimePaths,
    pub(super) preferences_path: PathBuf,
    pub(super) config_environment: Environment,
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
        let config_environment = Environment::capture()?;
        // A unit-test daemon must never expire or delete the user's history.
        #[cfg(test)]
        let config_environment = Environment::in_home(paths.spool_dir().to_path_buf());
        // Nor may it play cues or show banners on the developer's machine.
        let attended = !cfg!(test) && attended(config_environment.unattended());
        Ok(Self {
            paths,
            preferences_path: PreferenceStore::default_path(config_environment.home()),
            config_environment,
            lifetime,
            attended,
        })
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
}
