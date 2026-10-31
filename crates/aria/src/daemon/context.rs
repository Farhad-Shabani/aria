//! What describes one daemon run, fixed at startup.

use std::path::PathBuf;

#[cfg(not(test))]
use crate::config::home_directory;
use crate::config::{ConfigError, UNATTENDED_ENV, codex_home, unattended};
use crate::runtime::PreparedRuntimePaths;

use super::host::DaemonLifetime;

pub(super) struct Context<'runtime> {
    pub(super) paths: &'runtime PreparedRuntimePaths,
    pub(super) settings_path: PathBuf,
    pub(super) home: PathBuf,
    pub(super) codex_home: PathBuf,
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
        Ok(Self {
            paths,
            settings_path: crate::settings::default_path(&home),
            codex_home: codex_home(&home),
            home,
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
