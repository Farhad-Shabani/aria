use std::path::Path;

use crate::primitives::error_chain;
use anyhow::Context;
use aria_daemon::open_log_writer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{EnvFilter, Layer};

const DEFAULT_LOG_FILTER: &str = concat!(
    // Some of these log personal text, and any LAN host can make rustls warn.
    "info,ort=warn,whisper_rs=warn,rustls=error,misaki_rs=off,language_tokenizer=off,",
    // These log heavily below warn. Under a blanket `RUST_LOG=debug`, they would
    // fill the size-capped log and push out the daemon's own lines.
    "h2=warn,hyper_util=warn,tonic=warn,tower=warn,",
    "globset=warn,ignore=warn,tokenizers=warn,candle_transformers=warn",
);

pub(super) fn init_daemon_logging(log_path: &Path) -> anyhow::Result<()> {
    let log = open_log_writer(log_path.to_path_buf()).context("failed to open the daemon log")?;
    let overrides = std::env::var("RUST_LOG").ok();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .compact()
        .with_ansi(false)
        .with_writer(log)
        .with_filter(log_filter(overrides.as_deref()));
    let terminal_layer = overrides.is_some().then(|| {
        tracing_subscriber::fmt::layer()
            .with_target(true)
            .compact()
            .with_writer(std::io::stderr)
            .with_filter(log_filter(overrides.as_deref()))
    });
    let subscriber = tracing_subscriber::registry()
        .with(file_layer)
        .with(terminal_layer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|error| anyhow::anyhow!("failed to initialize daemon logging: {error}"))?;
    log_panics();
    // HTTP traces can contain dictated text, even when application errors are redacted.
    tracing_log::LogTracer::builder()
        .ignore_crate("ureq")
        .ignore_crate("ureq_proto")
        .init()
        .map_err(|error| anyhow::anyhow!("failed to initialize daemon logging: {error}"))?;
    aria_stt::whisper::install_logging_hooks();
    // The filter reports a bad directive only on stderr, not in the daemon log.
    if let Some(error) = overrides
        .as_deref()
        .and_then(|overrides| EnvFilter::builder().parse(overrides).err())
    {
        tracing::warn!(
            error = %error_chain(&error),
            "RUST_LOG has a directive that does not parse; it is skipped"
        );
    }
    Ok(())
}

/// A detached daemon's stderr goes nowhere, so a panic has to reach the log.
/// Only its place is logged, because a panic message can quote personal content.
fn log_panics() {
    let print_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("unnamed"),
            location = panic.location().map(tracing::field::display),
            "a thread panicked"
        );
        print_panic(panic);
    }));
}

/// The daemon's log filter, for the file and the terminal alike. `RUST_LOG`
/// has to reach the file: the daemon runs detached, so its terminal goes
/// nowhere and a filter the file ignores is a switch that does nothing.
pub(super) fn log_filter(overrides: Option<&str>) -> EnvFilter {
    match overrides {
        Some(overrides) => EnvFilter::new(format!("{DEFAULT_LOG_FILTER},{overrides}")),
        None => EnvFilter::new(DEFAULT_LOG_FILTER),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_logging_excludes_http_payloads_under_trace_overrides() {
        use aria_primitives::process::{ChildProcess, WaitOutcome};
        use std::io::Read;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        const CHILD_LOG_PATH: &str = "ARIA_TEST_HTTP_LOG_PATH";
        if let Some(path) = std::env::var_os(CHILD_LOG_PATH) {
            init_daemon_logging(Path::new(&path)).unwrap();
            tracing_log::log::trace!(target: "ureq_proto::util", "private-response-marker");
            tracing_log::log::debug!(target: "ureq::body", "private-header-marker");
            tracing::info!(target: "aria", "application-marker");
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.log");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "commands::logging::tests::daemon_logging_excludes_http_payloads_under_trace_overrides",
                "--nocapture",
            ])
            .env(CHILD_LOG_PATH, &path)
            .env("RUST_LOG", "trace,ureq_proto::util=trace,ureq::body=trace")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = ChildProcess::spawn(&mut command).unwrap();
        let stderr = child.take_stderr().unwrap();
        let (outcome, stderr) = std::thread::scope(|scope| {
            let reader = scope.spawn(move || {
                let mut bytes = Vec::new();
                stderr.take(16 * 1024).read_to_end(&mut bytes).unwrap();
                bytes
            });
            let outcome = child.wait_until(Instant::now() + Duration::from_secs(5), || false);
            child.terminate_in_place().unwrap();
            (outcome.unwrap(), reader.join().unwrap())
        });
        assert!(matches!(outcome, WaitOutcome::Exited(status) if status.success()));
        for output in [
            std::fs::read_to_string(path).unwrap(),
            String::from_utf8(stderr).unwrap(),
        ] {
            assert!(output.contains("application-marker"));
            assert!(!output.contains("private-response-marker"));
            assert!(!output.contains("private-header-marker"));
        }
    }
}
