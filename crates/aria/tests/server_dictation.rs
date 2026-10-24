use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aria::daemon::{Daemon, DaemonSession};
use aria::dictation::{DeliveryMode, DictationOutcome, DictationRequest, StopMode};
use aria::primitives::cancel::CancelToken;
use aria::primitives::process::{self, Bounds, Capture, ChildProcess, WaitOutcome};
use aria_harness::Sandbox;

const TIMEOUT: Duration = Duration::from_secs(60);

fn run(sandbox: &Sandbox, command: &mut Command, input: &[u8]) -> Vec<u8> {
    sandbox.apply(command);
    let output = process::run(
        command,
        input,
        Bounds {
            timeout: TIMEOUT,
            stdout: Capture::FailAbove(32 * 1024),
            stderr: Capture::KeepTail(8 * 1024),
        },
        &CancelToken::never(),
    )
    .unwrap();
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn desktop_dictates_through_a_server_without_local_stt() {
    let Some(model) = std::env::var_os("ARIA_TEST_WHISPER_MODEL") else {
        eprintln!("set ARIA_TEST_WHISPER_MODEL to exercise real server inference");
        return;
    };
    let model = aria_harness::yaml_string(&Path::new(&model).display().to_string()).unwrap();
    let server = Sandbox::create(&format!(
        "transcription:\n  backend: whisper\n  model: {model}\n"
    ))
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let endpoint = format!("https://{address}");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aria"));
    server.apply(&mut command);
    command
        .args(["serve", "--bind", &address.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut host = ChildProcess::spawn(&mut command).unwrap();
    let socket = server
        .home_path()
        .join(".aria/inference/runtime/daemon.sock");
    assert!(matches!(
        host.wait_until(Instant::now() + TIMEOUT, || socket.exists())
            .unwrap(),
        WaitOutcome::Cancelled
    ));
    let invitation = run(
        &server,
        Command::new(env!("CARGO_BIN_EXE_aria")).args([
            "remote",
            "invite",
            "--endpoint",
            &endpoint,
        ]),
        &[],
    );
    let audio =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../aria-wake/tests/fixtures/negative.wav");
    let audio = aria_harness::yaml_string(&audio.display().to_string()).unwrap();
    let device = Sandbox::create(&format!(
        "transcription:\n  backend: server\n  server: {endpoint}\ncapture:\n  backend: wav-file\n  path: {audio}\nwake:\n  enabled: false\nhotkey:\n  enabled: false\n"
    ))
    .unwrap();
    run(
        &device,
        Command::new(env!("CARGO_BIN_EXE_aria")).args(["remote", "pair"]),
        &invitation,
    );
    run(
        &device,
        Command::new(std::env::current_exe().unwrap()).args([
            "desktop_probe",
            "--exact",
            "--ignored",
            "--nocapture",
        ]),
        &[],
    );
    assert!(!device.home_path().join(".aria/models").exists());
    host.request_termination().unwrap();
    assert!(matches!(
        host.wait_until(Instant::now() + TIMEOUT, || false).unwrap(),
        WaitOutcome::Exited(status) if status.success()
    ));
}

#[test]
#[ignore = "runs inside an isolated desktop home"]
fn desktop_probe() {
    let daemon = Daemon::open().unwrap().spawn().unwrap();
    let session = DaemonSession::open().unwrap();
    let outcome = session
        .request_dictation(
            DictationRequest {
                delivery_mode: Some(DeliveryMode::Complete),
                transcript_only: true,
                ..DictationRequest::new(StopMode::Manual)
            },
            |_| {},
        )
        .unwrap();
    let DictationOutcome::Completed(result) = outcome else {
        panic!("dictation did not complete");
    };
    assert!(!result.text.is_empty());
    drop(session);
    daemon.stop().unwrap();
}
