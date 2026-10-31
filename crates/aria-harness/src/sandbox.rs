//! Test environment with a temporary home and runtime directory. Aria environment
//! variables are cleared, unattended mode suppresses sound and banners, and the
//! Aria key is off so no case taps the developer's keyboard.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const SETTINGS_DIRECTORY: &str = ".aria";
const SETTINGS_FILE: &str = "settings.toml";
/// Comes last, so the line numbers in a case's errors are the case's own.
const ARIA_KEY_OFF: &str = "\n[shortcuts]\naria_key = \"off\"\n";
const HOME_ENV: &str = "HOME";
const RUNTIME_DIR_ENV: &str = "ARIA_RUNTIME_DIR";
const RUNTIME_SUBDIRECTORY: &str = "runtime";
const SOCKET_FILE: &str = "daemon.sock";
const LOG_FILE: &str = "daemon.log";
const ARIA_ENV_PREFIX: &str = "ARIA_";
const LOG_FILTER_ENV: &str = "RUST_LOG";
const UNATTENDED_ENV: &str = "ARIA_UNATTENDED";
const UNATTENDED: &str = "1";

pub struct Sandbox {
    home: TempDir,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("failed to create the fake home")]
    CreateHome(#[source] std::io::Error),

    #[error("failed to create the fake settings directory")]
    CreateConfigDirectory(#[source] std::io::Error),

    #[error("failed to write the settings")]
    WriteConfig(#[source] std::io::Error),

    #[error("settings value contains control characters: {value:?}")]
    UnquotableValue { value: String },
}

/// Quotes a TOML basic string, preserving special characters and rejecting
/// control characters.
pub fn toml_string(value: &str) -> Result<String, SandboxError> {
    if value.chars().any(char::is_control) {
        return Err(SandboxError::UnquotableValue {
            value: value.to_owned(),
        });
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("\"{escaped}\""))
}

impl Sandbox {
    pub fn home_path(&self) -> &Path {
        self.home.path()
    }

    /// `settings` is the case's `settings.toml`, without the Aria key line.
    pub fn create(settings: &str) -> Result<Self, SandboxError> {
        let home = tempfile::tempdir().map_err(SandboxError::CreateHome)?;
        write_settings(home.path(), settings)?;
        Ok(Self { home })
    }

    /// Where the daemon binds, which it does before loading its model.
    pub fn socket_path(&self) -> std::path::PathBuf {
        self.home
            .path()
            .join(RUNTIME_SUBDIRECTORY)
            .join(SOCKET_FILE)
    }

    /// Where the daemon writes its structured log.
    pub fn log_path(&self) -> std::path::PathBuf {
        self.home.path().join(RUNTIME_SUBDIRECTORY).join(LOG_FILE)
    }

    /// Scopes `command` to this sandbox.
    pub fn apply(&self, command: &mut Command) {
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with(ARIA_ENV_PREFIX) {
                command.env_remove(&name);
            }
        }
        command
            .env(HOME_ENV, self.home.path())
            .env(RUNTIME_DIR_ENV, self.home.path().join(RUNTIME_SUBDIRECTORY))
            .env(UNATTENDED_ENV, UNATTENDED)
            .env_remove(LOG_FILTER_ENV);
    }
}

fn write_settings(home: &Path, settings: &str) -> Result<(), SandboxError> {
    let directory = home.join(SETTINGS_DIRECTORY);
    std::fs::create_dir(&directory).map_err(SandboxError::CreateConfigDirectory)?;
    std::fs::write(
        directory.join(SETTINGS_FILE),
        format!("{settings}{ARIA_KEY_OFF}"),
    )
    .map_err(SandboxError::WriteConfig)
}
