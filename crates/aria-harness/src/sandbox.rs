//! Test environment with a temporary home and runtime directory. Aria environment
//! variables are cleared, and unattended mode suppresses sound and banners.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

const CONFIG_DIRECTORY: &str = ".aria";
const CONFIG_FILE: &str = "config.yaml";
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

    #[error("failed to create the fake config directory")]
    CreateConfigDirectory(#[source] std::io::Error),

    #[error("failed to write the config")]
    WriteConfig(#[source] std::io::Error),

    #[error("config value contains control characters: {value:?}")]
    UnquotableValue { value: String },
}

/// Quotes a YAML value, preserving special characters and rejecting control characters.
pub fn yaml_string(value: &str) -> Result<String, SandboxError> {
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

    pub fn create(config: &str) -> Result<Self, SandboxError> {
        let home = tempfile::tempdir().map_err(SandboxError::CreateHome)?;
        write_config(home.path(), config)?;
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

fn write_config(home: &Path, config: &str) -> Result<(), SandboxError> {
    let directory = home.join(CONFIG_DIRECTORY);
    std::fs::create_dir(&directory).map_err(SandboxError::CreateConfigDirectory)?;
    std::fs::write(directory.join(CONFIG_FILE), config).map_err(SandboxError::WriteConfig)
}
