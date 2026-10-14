//! The user's PATH, read from their login shell.
//!
//! Aria.app's login agent starts the daemon with launchd's bare PATH. The
//! daemon runs coding agents and tools by name, and those usually live in
//! directories the shell's rc files add.

use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::ffi::OsStringExt;
use std::process::{ChildStdout, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::primitives::process::{ChildProcess, ChildProcessError};

/// A slow rc file delays every daemon start by up to this much.
const SHELL_TIMEOUT: Duration = Duration::from_secs(5);
const SHELL_POLL: Duration = Duration::from_millis(10);
/// Rc banners beyond this leave the PATH unread rather than growing memory.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const MARKER: &str = "__ARIA_PATH__";
/// What launchd gives a job whose plist sets no PATH.
const LAUNCHD_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

#[derive(Debug, thiserror::Error)]
pub enum LoginShellError {
    #[error("SHELL is not set")]
    NoShell,
    #[error("failed to start the login shell {shell:?}")]
    Spawn {
        shell: OsString,
        #[source]
        source: io::Error,
    },
    #[error("failed to read the login shell's output")]
    Read(#[source] io::Error),
    #[error("failed to wait for the login shell")]
    Wait(#[source] ChildProcessError),
    #[error("the login shell did not finish within {SHELL_TIMEOUT:?}")]
    Timeout,
    #[error("the login shell exited with {0}")]
    Exit(ExitStatus),
    #[error("the login shell printed no PATH in {printed} bytes of output")]
    NoPath { printed: usize },
}

/// Only a daemon launchd started with its bare PATH needs the login shell's.
/// One run from a terminal, or from the CLI's plist, already has the PATH
/// its user chose.
pub fn needs_login_shell_path(inherited: Option<&OsStr>) -> bool {
    inherited.is_none_or(|path| path == LAUNCHD_PATH)
}

/// Runs `$SHELL` as an interactive login shell and returns the PATH it ends
/// up with. Interactive, because many rc files set PATH only in `.zshrc`.
pub fn login_shell_path() -> Result<OsString, LoginShellError> {
    let shell = std::env::var_os("SHELL").ok_or(LoginShellError::NoShell)?;
    let mut command = Command::new(shell);
    command.args(["-i", "-l", "-c", &print_path()]);
    printed_path(command)
}

fn print_path() -> String {
    format!("printf '{MARKER}%s{MARKER}' \"$PATH\"")
}

fn printed_path(mut command: Command) -> Result<OsString, LoginShellError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut process =
        ChildProcess::spawn(&mut command).map_err(|source| LoginShellError::Spawn {
            shell: command.get_program().to_owned(),
            source,
        })?;
    // Taken before the wait: reaping the shell drops its pipe. Read while
    // waiting, so a chatty rc file never fills the pipe, and never blocking,
    // so a daemon an rc file left holding the pipe cannot stall the wait.
    let mut stdout = process
        .take_stdout()
        .ok_or(LoginShellError::NoPath { printed: 0 })?;
    rustix::io::ioctl_fionbio(&stdout, true)
        .map_err(|error| LoginShellError::Read(error.into()))?;
    // Bytes, not a string: rc files may print anything around the PATH.
    let mut output = Vec::new();
    let deadline = Instant::now() + SHELL_TIMEOUT;
    loop {
        drain(&mut stdout, &mut output)?;
        if let Some(status) = process.try_wait().map_err(LoginShellError::Wait)? {
            drain(&mut stdout, &mut output)?;
            if !status.success() {
                return Err(LoginShellError::Exit(status));
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err(LoginShellError::Timeout);
        }
        thread::sleep(SHELL_POLL);
    }
    marked_path(&output)
        .map(|path| OsString::from_vec(path.to_vec()))
        .ok_or(LoginShellError::NoPath {
            printed: output.len(),
        })
}

/// Reads what the pipe holds now, up to `MAX_OUTPUT_BYTES` in all.
fn drain(stdout: &mut ChildStdout, output: &mut Vec<u8>) -> Result<(), LoginShellError> {
    let mut chunk = [0; 8192];
    while output.len() < MAX_OUTPUT_BYTES {
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => output.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(LoginShellError::Read(error)),
        }
    }
    Ok(())
}

fn marked_path(output: &[u8]) -> Option<&[u8]> {
    let marker = MARKER.as_bytes();
    let start = find(output, marker)? + marker.len();
    let length = find(&output[start..], marker)?;
    (length > 0).then(|| &output[start..start + length])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_launchds_bare_path_needs_the_login_shell() {
        assert!(needs_login_shell_path(None));
        assert!(needs_login_shell_path(Some(OsStr::new(LAUNCHD_PATH))));
        assert!(!needs_login_shell_path(Some(OsStr::new(
            "/opt/homebrew/bin:/usr/bin:/bin"
        ))));
    }

    #[test]
    fn a_shell_hands_over_its_path() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &print_path()])
            .env("PATH", "/opt/test/bin:/usr/bin");
        assert_eq!(
            printed_path(command).unwrap(),
            OsString::from("/opt/test/bin:/usr/bin")
        );
    }

    #[test]
    fn only_a_marked_path_is_kept() {
        let mut banner = b"\xff\x1b[?1h welcome back\n".to_vec();
        banner.extend(format!("{MARKER}/opt/homebrew/bin{MARKER}bye\n").as_bytes());
        assert_eq!(marked_path(&banner), Some(b"/opt/homebrew/bin".as_slice()));
        assert_eq!(marked_path(b"no markers"), None);
        assert_eq!(marked_path(format!("{MARKER}/usr/bin").as_bytes()), None);
        assert_eq!(marked_path(format!("{MARKER}{MARKER}").as_bytes()), None);
    }
}
