//! Ctrl-C belongs to the process that opens a session, whether the daemon it
//! reaches runs elsewhere or inside it. Each case runs the probe in a child
//! process, because the probe raises SIGINT at itself. Every child runs in a
//! sandbox, so its daemon reads a temporary home instead of the developer's.
#![cfg(target_os = "macos")]

use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use aria::daemon::{Daemon, DaemonLifetime, DaemonSession};
use aria::primitives::process::{ChildProcess, WaitOutcome};
use aria_harness::Sandbox;
use signal_hook::consts::SIGINT;

const CHILD_TIMEOUT: Duration = Duration::from_secs(20);
/// Long enough for a daemon that caught the signal to start draining.
const DRAIN_WINDOW: Duration = Duration::from_millis(300);

#[test]
fn ctrl_c_stays_with_the_caller_when_another_process_owns_the_daemon() {
    let sandbox = Sandbox::create("").expect("the sandbox should be created");
    let mut host = start(&sandbox, "host");
    let socket = sandbox.socket_path();
    match host
        .wait_until(Instant::now() + CHILD_TIMEOUT, || socket.exists())
        .expect("the host should be waited on")
    {
        WaitOutcome::Cancelled => {}
        WaitOutcome::Exited(status) => panic!("the host ended before serving with {status}"),
        WaitOutcome::TimedOut => panic!("the host never bound its socket"),
    }

    let status = finish(start(&sandbox, "probe"));
    assert!(status.success(), "the probe ended with {status}");
    let status = finish(host);
    assert!(status.success(), "the host ended with {status}");
}

#[test]
fn ctrl_c_stays_with_the_caller_when_its_own_daemon_serves() {
    let sandbox = Sandbox::create("").expect("the sandbox should be created");

    let status = finish(start(&sandbox, "probe"));
    assert!(status.success(), "the probe ended with {status}");
}

#[test]
#[ignore = "runs in a child process, which the cases above start"]
fn host() {
    // On demand, so it serves without a model and stops once idle.
    let daemon = Daemon::builder()
        .lifetime(DaemonLifetime::OnDemand)
        .open()
        .expect("the daemon should configure")
        .spawn()
        .expect("the daemon should spawn");
    daemon.join().expect("the idle daemon should stop cleanly");
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
        .get_settings()
        .expect("the daemon should still serve after Ctrl-C");
}

/// Reruns this test binary as `case` inside `sandbox`.
fn start(sandbox: &Sandbox, case: &str) -> ChildProcess {
    let mut command =
        Command::new(std::env::current_exe().expect("the test binary should resolve"));
    sandbox.apply(&mut command);
    command
        .args([case, "--exact", "--ignored"])
        .stdout(Stdio::null());
    ChildProcess::spawn(&mut command).expect("the child should start")
}

fn finish(mut child: ChildProcess) -> ExitStatus {
    match child
        .wait_until(Instant::now() + CHILD_TIMEOUT, || false)
        .expect("the child should be waited on")
    {
        WaitOutcome::Exited(status) => status,
        WaitOutcome::Cancelled | WaitOutcome::TimedOut => panic!("the child did not finish"),
    }
}
