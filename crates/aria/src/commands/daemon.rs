use crate::primitives::error_chain;
use anyhow::Context;
use aria_daemon::Daemon;

use super::logging::init_daemon_logging;

pub(super) fn run() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let login_path = adopt_login_shell_path();
    let daemon = Daemon::open().context("failed to prepare daemon runtime paths")?;

    init_daemon_logging(daemon.log_path())?;
    tracing::info!(socket = %daemon.socket_path().display(), "starting Aria daemon");
    #[cfg(target_os = "macos")]
    if let Err(error) = login_path {
        tracing::warn!(
            error = %error_chain(error.as_ref()),
            "kept the inherited PATH; assistants and tools outside it will not start"
        );
    }

    // The Mac app's service drops stderr, so the error must reach the log.
    if let Err(error) = daemon.run() {
        match error.settings_position() {
            // The message can quote the file; stderr still gets all of it.
            Some(Some(position)) => tracing::error!(
                line = position.line,
                column = position.column,
                "Aria daemon terminated: settings.toml is invalid"
            ),
            Some(None) => tracing::error!("Aria daemon terminated: settings.toml is unusable"),
            None => {
                tracing::error!(error = %error_chain(&error), "Aria daemon terminated with an error");
            }
        }
        return Err(error).context("Aria daemon terminated with an error");
    }
    Ok(())
}

/// Restarts the daemon in place with the PATH a terminal would have, so it
/// finds `claude` and `codex` however launchd started it. Returns
/// only when it could not; the restarted daemon's PATH is no longer bare.
#[cfg(target_os = "macos")]
fn adopt_login_shell_path() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::process::CommandExt;

    if !aria_daemon::needs_login_shell_path(std::env::var_os("PATH").as_deref()) {
        return Ok(());
    }
    let path = aria_daemon::login_shell_path()?;
    // Same pid, so launchd still tracks the daemon.
    let error = std::process::Command::new(std::env::current_exe()?)
        .args(std::env::args_os().skip(1))
        .env("PATH", path)
        .exec();
    Err(error.into())
}
