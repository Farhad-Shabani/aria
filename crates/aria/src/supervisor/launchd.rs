//! launchd: a LaunchAgent in the user's gui domain.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};
use std::{fs, io};

use super::{Job, SupervisorError, UnitSpec, run_command};

const LABEL: &str = "io.github.farhad-shabani.aria.daemon";
pub(super) const UNIT_FILE: &str = "io.github.farhad-shabani.aria.daemon.plist";
const LAUNCHCTL: &str = "/bin/launchctl";
/// What `launchctl` exits with for a label the domain does not know.
const NOT_FOUND: i32 = 113;
/// `bootout` returns at once and the job stays listed until its process
/// exits, which takes the daemon's shutdown grace.
const UNLOAD_TIMEOUT: Duration = Duration::from_secs(10);
const UNLOAD_POLL: Duration = Duration::from_millis(100);
/// The wake agent the Python `setup.sh` registers.
const LEGACY_LABEL: &str = "com.user.aria-wake";
const LEGACY_UNIT_FILE: &str = "com.user.aria-wake.plist";
/// What `bootout` exits with for a job that is not loaded.
const NO_SUCH_PROCESS: i32 = 3;

pub(super) fn unit_directory(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
}

pub(super) fn render(spec: &UnitSpec) -> String {
    let exe = xml_escape(spec.exe.as_os_str());
    let log = xml_escape(spec.home.join("Library/Logs/aria-daemon.log").as_os_str());
    let mut environment = String::new();
    for (name, value) in &spec.environment {
        environment.push_str(&format!(
            "        <key>{name}</key>\n        <string>{}</string>\n",
            xml_escape(value)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>
{environment}    </dict>
</dict>
</plist>
"#
    )
}

pub(super) fn load(unit: &Path) -> Result<(), SupervisorError> {
    let mut command = launchctl();
    command.arg("bootstrap").arg(domain()).arg(unit);
    run_command(command).map(drop)
}

/// Boots the job out and waits until launchd has forgotten it.
pub(super) fn unload() -> Result<(), SupervisorError> {
    if job()? == Job::Absent {
        return Ok(());
    }
    let mut command = launchctl();
    command.arg("bootout").arg(target());
    run_command(command)?;
    await_absent(&target())
}

/// Waits until launchd has forgotten a label, so the socket or microphone
/// its process held is free when this returns.
fn await_absent(target: &str) -> Result<(), SupervisorError> {
    let deadline = Instant::now() + UNLOAD_TIMEOUT;
    while job_at(target)? != Job::Absent {
        if Instant::now() >= deadline {
            return Err(SupervisorError::UnloadTimeout {
                timeout: UNLOAD_TIMEOUT,
            });
        }
        thread::sleep(UNLOAD_POLL);
    }
    Ok(())
}

pub(super) fn job() -> Result<Job, SupervisorError> {
    job_at(&target())
}

fn job_at(target: &str) -> Result<Job, SupervisorError> {
    Ok(print(target)?.map_or(Job::Absent, |report| parse_job(&report)))
}

/// Whether the loaded job came from a plist other than `unit`: Aria.app's
/// login agent, which launchd reports as submitted by smd.
pub(super) fn app_owns_job(unit: &Path) -> Result<bool, SupervisorError> {
    Ok(print(&target())?.is_some_and(|report| loaded_elsewhere(&report, unit)))
}

/// `launchctl print` for a label; `None` when the domain does not know it.
fn print(target: &str) -> Result<Option<String>, SupervisorError> {
    let mut command = launchctl();
    command.arg("print").arg(target);
    match run_command(command) {
        Ok(report) => Ok(Some(report)),
        Err(SupervisorError::Command { status, .. }) if status.code() == Some(NOT_FOUND) => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Boots the legacy wake agent out and deletes its plist, so it stops
/// competing for the microphone. Answers the plist it removed.
pub(super) fn remove_legacy_wake_agent(home: &Path) -> Result<Option<PathBuf>, SupervisorError> {
    let unit = unit_directory(home).join(LEGACY_UNIT_FILE);
    if !unit.exists() {
        return Ok(None);
    }
    let legacy_target = format!("{}/{LEGACY_LABEL}", domain());
    let mut command = launchctl();
    command.arg("bootout").arg(&legacy_target);
    match run_command(command) {
        Ok(_) => {}
        Err(SupervisorError::Command { status, .. })
            if status.code() == Some(NO_SUCH_PROCESS) || status.code() == Some(NOT_FOUND) => {}
        Err(error) => return Err(error),
    }
    // `bootout` returns before the process exits, and the new daemon must
    // not race the old one for the microphone.
    await_absent(&legacy_target)?;
    match fs::remove_file(&unit) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(SupervisorError::RemoveUnit { path: unit, source }),
    }
    Ok(Some(unit))
}

pub(super) fn start() -> Result<(), SupervisorError> {
    let mut command = launchctl();
    command.arg("kickstart").arg(target());
    run_command(command).map(drop)
}

pub(super) fn stop() -> Result<(), SupervisorError> {
    let mut command = launchctl();
    command.args(["kill", "SIGTERM"]).arg(target());
    run_command(command).map(drop)
}

/// `launchctl print` lists `pid = N` only while the job has a process.
fn parse_job(report: &str) -> Job {
    report
        .lines()
        .filter_map(|line| line.trim().strip_prefix("pid = "))
        .find_map(|pid| pid.trim().parse().ok())
        .map_or(Job::Idle, Job::Running)
}

/// The job's own `path = ` line comes before any nested section's.
fn loaded_elsewhere(report: &str, unit: &Path) -> bool {
    report
        .lines()
        .find_map(|line| line.trim().strip_prefix("path = "))
        .is_some_and(|path| Path::new(path) != unit)
}

fn launchctl() -> Command {
    Command::new(LAUNCHCTL)
}

fn domain() -> String {
    format!("gui/{}", rustix::process::getuid().as_raw())
}

fn target() -> String {
    format!("{}/{LABEL}", domain())
}

fn xml_escape(value: &OsStr) -> String {
    value
        .to_string_lossy()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    #[test]
    fn the_unit_file_carries_the_label() {
        assert_eq!(UNIT_FILE, format!("{LABEL}.plist"));
    }

    #[test]
    fn the_plist_runs_the_binary_with_the_installing_environment() {
        let spec = UnitSpec {
            exe: PathBuf::from("/opt/aria & co/aria"),
            home: PathBuf::from("/Users/tester"),
            environment: vec![
                ("PATH", OsString::from("/opt/homebrew/bin:/usr/bin")),
                ("ARIA_RUNTIME_DIR", OsString::from("/tmp/<aria>")),
            ],
        };

        let plist = render(&spec);

        assert!(plist.contains("<string>io.github.farhad-shabani.aria.daemon</string>"));
        assert!(
            plist.contains(
                "<string>/opt/aria &amp; co/aria</string>\n        <string>daemon</string>"
            )
        );
        assert!(plist.contains("<key>SuccessfulExit</key>\n        <false/>"));
        assert!(plist.contains("<string>/Users/tester/Library/Logs/aria-daemon.log</string>"));
        assert!(
            plist.contains("<key>PATH</key>\n        <string>/opt/homebrew/bin:/usr/bin</string>")
        );
        assert!(
            plist.contains(
                "<key>ARIA_RUNTIME_DIR</key>\n        <string>/tmp/&lt;aria&gt;</string>"
            )
        );
    }

    #[test]
    fn an_unset_variable_is_left_out_of_the_plist() {
        let spec = UnitSpec {
            exe: PathBuf::from("/opt/aria"),
            home: PathBuf::from("/Users/tester"),
            environment: Vec::new(),
        };

        let plist = render(&spec);

        assert!(!plist.contains("<key>PATH</key>"));
        assert!(plist.contains("<dict>\n    </dict>"));
    }

    #[test]
    fn a_job_with_a_process_reports_its_pid() {
        let report = "gui/501/io.github.farhad-shabani.aria.daemon = {\n\tstate = running\n\tpid = 4242\n}\n";
        assert_eq!(parse_job(report), Job::Running(4242));
        assert_eq!(parse_job("\tstate = not running\n"), Job::Idle);
    }

    #[test]
    fn only_a_job_loaded_from_another_plist_belongs_to_the_app() {
        let unit = Path::new(
            "/Users/tester/Library/LaunchAgents/io.github.farhad-shabani.aria.daemon.plist",
        );
        let app = "gui/501/io.github.farhad-shabani.aria.daemon = {\n\tpath = (submitted by smd.348)\n\tendpoints = {\n\t\tpath = /tmp/other\n\t}\n}\n";
        let ours = format!(
            "gui/501/io.github.farhad-shabani.aria.daemon = {{\n\tpath = {}\n}}\n",
            unit.display()
        );
        assert!(loaded_elsewhere(app, unit));
        assert!(!loaded_elsewhere(&ours, unit));
        assert!(!loaded_elsewhere("\tstate = running\n", unit));
    }
}
