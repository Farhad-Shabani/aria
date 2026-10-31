//! Running the scenario catalog against a compiled binary, one test per case.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use aria_primitives::error_chain;
use libtest_mimic::{Arguments, Conclusion, Failed, Trial};

use crate::catalog::{self, Case, CatalogError, ClientStep, ResolvedSettings, Startup};
use crate::daemon::{self, ServiceStart};
use crate::sandbox::Sandbox;

const LOG_TAIL_LINES: usize = 40;
const PRIVATE_FILE_MODE: u32 = 0o644;

/// Runs every case in `cases_directory` against `binary` as its own test,
/// under the command-line arguments `cargo test` passed. A test is named
/// after its case file, so `cargo test --test acceptance <name>` runs one.
/// Cases name files in `fixtures_directory` as `${FIXTURES}`.
pub fn run_catalog(
    binary: &Path,
    cases_directory: &Path,
    fixtures_directory: &Path,
) -> Result<Conclusion, CatalogError> {
    let fixtures = private_copy(fixtures_directory)?;
    let trials = catalog::discover(cases_directory)?
        .into_iter()
        .map(|path| trial(binary, path, fixtures.path()))
        .collect();
    Ok(libtest_mimic::run(&Arguments::from_args(), trials))
}

/// The daemon refuses an input file other users can write, and a checkout
/// under umask 002 leaves every fixture group-writable.
fn private_copy(directory: &Path) -> Result<tempfile::TempDir, CatalogError> {
    let failed = |source| CatalogError::CopyFixtures {
        path: directory.to_path_buf(),
        source,
    };
    let copy = tempfile::tempdir().map_err(failed)?;
    for entry in std::fs::read_dir(directory).map_err(failed)? {
        let entry = entry.map_err(failed)?;
        if !entry.file_type().map_err(failed)?.is_file() {
            continue;
        }
        let target = copy.path().join(entry.file_name());
        std::fs::copy(entry.path(), &target).map_err(failed)?;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(PRIVATE_FILE_MODE))
            .map_err(failed)?;
    }
    Ok(copy)
}

fn trial(binary: &Path, path: PathBuf, fixtures_directory: &Path) -> Trial {
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let case = match Case::load(&path) {
        Ok(case) => case,
        Err(error) => return Trial::test(name, move || Err(error_chain(&error).into())),
    };
    if let Some(platform) = case.platform.filter(|platform| !platform.is_current()) {
        return skipped(name, format!("{} only", platform.name()));
    }
    let settings = match case.resolve_settings(fixtures_directory) {
        Ok(ResolvedSettings::Ready(settings)) => settings,
        Ok(ResolvedSettings::MissingEnvironment(variable)) => {
            return skipped(name, format!("needs {variable}"));
        }
        Err(error) => return Trial::test(name, move || Err(error_chain(&error).into())),
    };
    let binary = binary.to_path_buf();
    Trial::test(name, move || run_case(&binary, &case, &settings))
}

/// Ignored, with the reason shown beside the name. Under `--ignored` the case
/// fails with that reason instead of passing without running.
fn skipped(name: String, reason: String) -> Trial {
    Trial::test(name, {
        let reason = reason.clone();
        move || Err(reason.into())
    })
    .with_kind(reason)
    .with_ignored_flag(true)
}

fn run_case(binary: &Path, case: &Case, settings: &str) -> Result<(), Failed> {
    let sandbox = Sandbox::create(settings).map_err(|error| error_chain(&error))?;
    let outcome = run_steps(binary, &sandbox, case.expect.steps_before_daemon()).and_then(|()| {
        match case.expect.daemon_startup() {
            None => run_clients(binary, &sandbox, case),
            Some(Startup::Failure) => daemon::run_to_exit(binary, &sandbox)
                .map_err(|error| error_chain(&error))
                .and_then(|output| case.expect.check_daemon(&output)),
            Some(Startup::Success) => serve_clients(binary, &sandbox, case)
                .and_then(|output| case.expect.check_daemon(&output)),
        }
    });
    outcome.map_err(|failure| with_log_tail(&failure, &sandbox).into())
}

fn run_clients(binary: &Path, sandbox: &Sandbox, case: &Case) -> Result<(), String> {
    run_steps(binary, sandbox, case.expect.client_steps())
}

fn run_steps(binary: &Path, sandbox: &Sandbox, steps: &[ClientStep]) -> Result<(), String> {
    for client in steps {
        client.check(&run_client(binary, sandbox, &client.run)?)?;
    }
    Ok(())
}

/// A daemon that exits before it is ready fails the case, even with status 0.
/// Its client steps never ran.
fn serve_clients(binary: &Path, sandbox: &Sandbox, case: &Case) -> Result<Output, String> {
    let daemon =
        match daemon::start_service(binary, sandbox).map_err(|error| error_chain(&error))? {
            ServiceStart::Running(daemon) => daemon,
            ServiceStart::Exited(output) => {
                return Err(format!(
                    "`aria daemon` exited before it was ready\nexit: {}\nstderr:\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr),
                ));
            }
        };
    run_clients(binary, sandbox, case)?;
    daemon.stop().map_err(|error| error_chain(&error))
}

fn run_client(binary: &Path, sandbox: &Sandbox, run: &[String]) -> Result<Output, String> {
    let arguments: Vec<&str> = run.iter().map(String::as_str).collect();
    daemon::run_client(binary, &arguments, sandbox).map_err(|error| error_chain(&error))
}

fn with_log_tail(failure: &str, sandbox: &Sandbox) -> String {
    let Ok(log) = std::fs::read_to_string(sandbox.log_path()) else {
        return format!("{failure}\n(no daemon log)");
    };
    let lines: Vec<&str> = log.lines().collect();
    let tail = lines[lines.len().saturating_sub(LOG_TAIL_LINES)..].join("\n");
    format!("{failure}\ndaemon log, last {LOG_TAIL_LINES} lines:\n{tail}")
}
