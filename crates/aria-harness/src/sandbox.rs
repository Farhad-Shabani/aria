//! Test environment with a temporary home and runtime directory. Aria environment
//! variables are cleared, and unattended mode suppresses sound, banners, and
//! the Aria key, so no case reacts to the developer's keyboard.

use std::path::Path;
use std::process::Command;
#[cfg(target_os = "macos")]
use std::process::Stdio;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use aria_primitives::process::{ChildProcess, WaitOutcome};

use tempfile::TempDir;

const SETTINGS_DIRECTORY: &str = ".aria";
const SETTINGS_FILE: &str = "settings.toml";
const HOME_ENV: &str = "HOME";
const RUNTIME_DIR_ENV: &str = "ARIA_RUNTIME_DIR";
const RUNTIME_SUBDIRECTORY: &str = "runtime";
const SOCKET_FILE: &str = "daemon.sock";
const LOG_FILE: &str = "daemon.log";
const ARIA_ENV_PREFIX: &str = "ARIA_";
const LOG_FILTER_ENV: &str = "RUST_LOG";
const UNATTENDED_ENV: &str = "ARIA_UNATTENDED";
const UNATTENDED: &str = "1";
#[cfg(target_os = "macos")]
const KEYCHAIN_DIRECTORY: &str = "Library/Keychains";
#[cfg(target_os = "macos")]
const LOGIN_KEYCHAIN: &str = "login.keychain-db";
#[cfg(target_os = "macos")]
const KEYCHAIN_DEADLINE: Duration = Duration::from_secs(10);

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

    #[error("failed to create the fake login Keychain")]
    CreateKeychain(#[source] std::io::Error),

    #[error("`security create-keychain` did not finish: {outcome}")]
    KeychainCommand { outcome: String },

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

    /// `settings` is the case's `settings.toml`.
    pub fn create(settings: &str) -> Result<Self, SandboxError> {
        let home = tempfile::tempdir().map_err(SandboxError::CreateHome)?;
        write_settings(home.path(), settings)?;
        let sandbox = Self { home };
        #[cfg(target_os = "macos")]
        sandbox.create_login_keychain()?;
        Ok(sandbox)
    }

    /// The daemon keeps its storage key in the default Keychain, which macOS
    /// finds through `HOME`; a fake home has none. `security` runs with the
    /// fake home, so the new Keychain joins that home's search list, never
    /// the developer's.
    #[cfg(target_os = "macos")]
    fn create_login_keychain(&self) -> Result<(), SandboxError> {
        let directory = self.home.path().join(KEYCHAIN_DIRECTORY);
        std::fs::create_dir_all(&directory).map_err(SandboxError::CreateKeychain)?;
        let mut command = Command::new("/usr/bin/security");
        self.apply(&mut command);
        command
            .args(["create-keychain", "-p", ""])
            .arg(directory.join(LOGIN_KEYCHAIN))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process =
            ChildProcess::spawn(&mut command).map_err(SandboxError::CreateKeychain)?;
        let outcome = match process.wait_until(Instant::now() + KEYCHAIN_DEADLINE, || false) {
            Ok(WaitOutcome::Exited(status)) if status.success() => return Ok(()),
            Ok(WaitOutcome::Exited(status)) => status.to_string(),
            Ok(WaitOutcome::TimedOut | WaitOutcome::Cancelled) => "timed out".to_owned(),
            Err(error) => aria_primitives::error_chain(&error),
        };
        Err(SandboxError::KeychainCommand { outcome })
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

    /// Reruns the current test binary as the ignored test `case` inside this
    /// sandbox.
    pub fn test_command(&self, case: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("the test binary resolves"));
        self.apply(&mut command);
        command.args([case, "--exact", "--ignored"]);
        command
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
    std::fs::write(directory.join(SETTINGS_FILE), settings).map_err(SandboxError::WriteConfig)
}
