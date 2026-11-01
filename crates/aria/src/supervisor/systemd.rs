//! systemd: a user unit under `~/.config/systemd/user`.
//!
//! The unit belongs to the graphical session: delivery needs its display,
//! and the desktop sets the display variables in the user manager before
//! `graphical-session.target` starts. It stops at logout with the session.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{fs, io};

use super::{Job, SupervisorError, UnitSpec, run_command};
use crate::delivery::APP_ID;

const UNIT: &str = "aria-daemon.service";
pub(super) const UNIT_FILE: &str = UNIT;
const SYSTEMCTL: &str = "/usr/bin/systemctl";
/// The wake unit the Python `setup.sh` registers.
const LEGACY_UNIT: &str = "aria-wake.service";
/// What `systemctl` exits with for a unit it does not know.
const NO_SUCH_UNIT: i32 = 5;

pub(super) fn unit_directory(home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
}

pub(super) fn render(spec: &UnitSpec) -> String {
    let mut environment = String::new();
    for (name, value) in &spec.environment {
        environment.push_str(&format!("Environment=\"{name}={}\"\n", quote(value)));
    }
    format!(
        "[Unit]\nDescription=Aria daemon\nPartOf=graphical-session.target\nAfter=graphical-session.target\n\n[Service]\nExecStart=\"{}\" daemon\nRestart=on-failure\n{environment}\n[Install]\nWantedBy=graphical-session.target\n",
        quote(spec.exe.as_os_str())
    )
}

/// The portal uses this hidden desktop entry to name Aria in its permission dialog.
pub(super) fn companion_path(spec: &UnitSpec) -> Option<PathBuf> {
    let data_home = spec
        .environment
        .iter()
        .find(|(name, _)| *name == "XDG_DATA_HOME")
        .map(|(_, value)| PathBuf::from(value))
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| spec.home.join(".local/share"));
    Some(
        data_home
            .join("applications")
            .join(format!("{APP_ID}.desktop")),
    )
}

pub(super) fn render_companion(spec: &UnitSpec) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=Aria\nComment=Local voice dictation\n\
         Exec={} daemon\nNoDisplay=true\n",
        exec_argument(spec.exe.as_os_str())
    )
}

/// Desktop entries unescape the value before parsing the quoted Exec argument,
/// so backslashes need a second escape.
fn exec_argument(value: &OsStr) -> String {
    let mut quoted = String::from("\"");
    for character in value.to_string_lossy().chars() {
        match character {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(character);
            }
            '%' => quoted.push_str("%%"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\")
}

pub(super) fn load(_unit: &Path) -> Result<(), SupervisorError> {
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", UNIT])
}

pub(super) fn unload() -> Result<(), SupervisorError> {
    if job()? == Job::Absent {
        return Ok(());
    }
    systemctl(&["disable", "--now", UNIT])
}

/// `is-enabled` exits non-zero for an unknown unit; a failed call means
/// not registered. `show` then answers `0` for a unit without a process.
pub(super) fn job() -> Result<Job, SupervisorError> {
    match systemctl(&["is-enabled", "--quiet", UNIT]) {
        Ok(()) => {}
        Err(SupervisorError::Command { .. }) => return Ok(Job::Absent),
        Err(error) => return Err(error),
    }
    let mut command = Command::new(SYSTEMCTL);
    command.args(["--user", "show", "--property", "MainPID", "--value", UNIT]);
    Ok(parse_main_pid(&run_command(command)?))
}

/// Only `aria service install` writes this unit, so no other owner exists.
pub(super) fn app_owns_job(_unit: &Path) -> Result<bool, SupervisorError> {
    Ok(false)
}

/// Stops the legacy wake unit and deletes it, so it stops competing for
/// the microphone. Answers the unit file it removed.
pub(super) fn remove_legacy_wake_agent(home: &Path) -> Result<Option<PathBuf>, SupervisorError> {
    let unit = unit_directory(home).join(LEGACY_UNIT);
    if !unit.exists() {
        return Ok(None);
    }
    match systemctl(&["disable", "--now", LEGACY_UNIT]) {
        // A unit systemd never loaded has nothing to stop. Any other
        // failure means it may still be recording, so the unit stays.
        Ok(()) => {}
        Err(SupervisorError::Command { status, .. }) if status.code() == Some(NO_SUCH_UNIT) => {}
        Err(error) => return Err(error),
    }
    match fs::remove_file(&unit) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(SupervisorError::RemoveUnit { path: unit, source }),
    }
    systemctl(&["daemon-reload"])?;
    Ok(Some(unit))
}

pub(super) fn start() -> Result<(), SupervisorError> {
    systemctl(&["start", UNIT])
}

pub(super) fn stop() -> Result<(), SupervisorError> {
    systemctl(&["stop", UNIT])
}

fn parse_main_pid(value: &str) -> Job {
    match value.trim().parse() {
        Ok(0) | Err(_) => Job::Idle,
        Ok(pid) => Job::Running(pid),
    }
}

fn systemctl(args: &[&str]) -> Result<(), SupervisorError> {
    let mut command = Command::new(SYSTEMCTL);
    command.arg("--user").args(args);
    run_command(command).map(drop)
}

/// Inside a double-quoted unit value systemd C-unescapes backslashes and
/// expands `%` specifiers. Newlines are refused before rendering.
fn quote(value: &OsStr) -> String {
    value
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn the_unit_runs_the_binary_with_the_installing_environment() {
        let spec = UnitSpec {
            exe: PathBuf::from("/opt/aria 100%/ar\"ia"),
            home: PathBuf::from("/home/tester"),
            environment: vec![
                ("PATH", OsString::from("/usr/local/bin:/usr/bin")),
                ("ARIA_RUNTIME_DIR", OsString::from("/run/user/1000/a\\b")),
                ("XDG_DATA_HOME", OsString::from("/home/tester/data")),
            ],
        };

        assert_eq!(
            render(&spec),
            "[Unit]\nDescription=Aria daemon\n\
             PartOf=graphical-session.target\nAfter=graphical-session.target\n\n[Service]\n\
             ExecStart=\"/opt/aria 100%%/ar\\\"ia\" daemon\nRestart=on-failure\n\
             Environment=\"PATH=/usr/local/bin:/usr/bin\"\n\
             Environment=\"ARIA_RUNTIME_DIR=/run/user/1000/a\\\\b\"\n\
             Environment=\"XDG_DATA_HOME=/home/tester/data\"\n\n\
             [Install]\nWantedBy=graphical-session.target\n"
        );
    }

    #[test]
    fn the_desktop_entry_uses_an_absolute_xdg_data_home() {
        for (value, directory) in [
            (None, "/home/tester/.local/share"),
            (Some(""), "/home/tester/.local/share"),
            (Some("relative/data"), "/home/tester/.local/share"),
            (Some("/home/tester/custom data"), "/home/tester/custom data"),
        ] {
            let spec = UnitSpec {
                exe: PathBuf::from("/opt/aria"),
                home: PathBuf::from("/home/tester"),
                environment: value
                    .map(|value| ("XDG_DATA_HOME", OsString::from(value)))
                    .into_iter()
                    .collect(),
            };

            assert_eq!(
                companion_path(&spec),
                Some(Path::new(directory).join(format!("applications/{APP_ID}.desktop"))),
                "XDG_DATA_HOME={value:?}"
            );
        }
    }

    #[test]
    fn the_desktop_entry_quotes_the_binary_for_exec() {
        let spec = UnitSpec {
            exe: PathBuf::from("/opt/a\"b $c%d\\e/aria"),
            home: PathBuf::from("/home/tester"),
            environment: Vec::new(),
        };

        assert_eq!(
            companion_path(&spec),
            Some(PathBuf::from(
                "/home/tester/.local/share/applications/io.github.farhad_shabani.aria.desktop"
            ))
        );
        let entry = render_companion(&spec);
        assert!(
            entry.contains("Exec=\"/opt/a\\\\\"b \\\\$c%%d\\\\\\\\e/aria\" daemon\n"),
            "{entry}"
        );
        assert!(entry.contains("NoDisplay=true\n"));
    }

    #[test]
    fn a_main_pid_of_zero_means_no_process() {
        assert_eq!(parse_main_pid("0\n"), Job::Idle);
        assert_eq!(parse_main_pid("4242\n"), Job::Running(4242));
        assert_eq!(parse_main_pid(""), Job::Idle);
    }
}
