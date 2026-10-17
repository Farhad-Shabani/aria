//! Ctrl-C belongs to the process that opens a session, whether the daemon it
//! reaches runs elsewhere or inside it. Each case runs the probe in a child
//! process, because the probe raises SIGINT at itself.
#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aria::daemon::{Daemon, DaemonLifetime, DaemonSession};
use aria::primitives::process::{ChildProcess, WaitOutcome};
use signal_hook::consts::SIGINT;

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Long enough for a daemon that caught the signal to start draining.
const DRAIN_WINDOW: Duration = Duration::from_millis(300);

#[test]
fn ctrl_c_stays_with_the_caller_when_another_process_owns_the_daemon() {
    let runtime = tempfile::tempdir().expect("the runtime directory should be created");
    // On demand, so it serves without a model.
    let _daemon = Daemon::builder()
        .runtime_dir(runtime.path())
        .lifetime(DaemonLifetime::OnDemand)
        .open()
        .expect("the daemon should configure")
        .spawn()
        .expect("the daemon should spawn");

    let status = run_probe(runtime.path());
    assert!(status.success(), "the probe ended with {status}");
}

#[test]
fn ctrl_c_stays_with_the_caller_when_its_own_daemon_serves() {
    let runtime = tempfile::tempdir().expect("the runtime directory should be created");

    let status = run_probe(runtime.path());
    assert!(status.success(), "the probe ended with {status}");
}

#[test]
#[ignore = "runs in a child process, which the cases above start"]
fn probe() {
    let session = DaemonSession::open().expect("a session should open");
    let delivered = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&delivered))
        .expect("the caller should watch for Ctrl-C");

    signal_hook::low_level::raise(SIGINT).expect("Ctrl-C should be raised");
    thread::sleep(DRAIN_WINDOW);

    assert!(delivered.load(Ordering::SeqCst), "the caller saw Ctrl-C");
    session
        .get_preferences()
        .expect("the daemon should still serve after Ctrl-C");
}

fn run_probe(runtime: &Path) -> ExitStatus {
    let mut command =
        Command::new(std::env::current_exe().expect("the test binary should resolve"));
    command
        .args(["probe", "--exact", "--ignored"])
        .env("ARIA_RUNTIME_DIR", runtime)
        .stdout(Stdio::null());
    let mut probe = ChildProcess::spawn(&mut command).expect("the probe should start");

    match probe
        .wait_until(Instant::now() + PROBE_TIMEOUT, || false)
        .expect("the probe should be waited on")
    {
        WaitOutcome::Exited(status) => status,
        WaitOutcome::Cancelled | WaitOutcome::TimedOut => panic!("the probe did not finish"),
    }
}
