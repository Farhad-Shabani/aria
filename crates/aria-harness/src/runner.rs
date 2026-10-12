//! Running the whole scenario catalog against a compiled binary.

use std::path::Path;

use crate::catalog::{self, Case, CatalogError, ResolvedConfig, Startup};
use crate::daemon::{self, DaemonProcessError, ServiceStart};
use crate::sandbox::{Sandbox, SandboxError};

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),

    #[error(transparent)]
    Sandbox(#[from] SandboxError),

    #[error("case {name:?}")]
    Daemon {
        name: String,
        #[source]
        source: DaemonProcessError,
    },
}

/// Runs each client step against a serving daemon, then stops it and hands
/// back the daemon's own output for the daemon expectations to judge.
///
/// A daemon that exits before it can serve returns its output straight
/// through: the steps have nothing to talk to, and the daemon expectations
/// are what should report that.
fn serve_clients(
    binary: &Path,
    sandbox: &Sandbox,
    case: &Case,
) -> Result<std::process::Output, DaemonProcessError> {
    let daemon = match daemon::start_service(binary, sandbox)? {
        ServiceStart::Running(daemon) => daemon,
        ServiceStart::Exited(output) => return Ok(output),
    };
    for (step, client) in case.expect.client_steps().iter().enumerate() {
        let arguments: Vec<&str> = client.run.iter().map(String::as_str).collect();
        let output = daemon::run_client(binary, &arguments, sandbox)?;
        case.expect.check_client(&case.name, step, &output);
    }
    daemon.stop()
}

/// Runs every case in `cases_directory` against `binary`.
pub fn run_catalog(binary: &Path, cases_directory: &Path) -> Result<(), HarnessError> {
    for path in catalog::discover(cases_directory)? {
        let case = Case::load(&path)?;
        if case.platform.is_some_and(|platform| !platform.is_current()) {
            eprintln!(
                "skipping case {:?}: it asserts behavior of another platform",
                case.name
            );
            continue;
        }
        let config = match case.resolve_config()? {
            ResolvedConfig::Ready(config) => config,
            ResolvedConfig::MissingEnvironment(variable) => {
                eprintln!("skipping case {:?}: {variable} is not set", case.name);
                continue;
            }
        };
        let sandbox = Sandbox::create(&config)?;
        let named = |source| HarnessError::Daemon {
            name: case.name.clone(),
            source,
        };
        let output = match case.expect.daemon_startup() {
            Startup::Failure => daemon::run_to_exit(binary, &sandbox).map_err(named)?,
            Startup::Success if case.expect.client_steps().is_empty() => {
                daemon::run_service(binary, &sandbox).map_err(named)?
            }
            Startup::Success => serve_clients(binary, &sandbox, &case).map_err(named)?,
        };
        case.expect.check(&case.name, &output);
    }
    Ok(())
}
