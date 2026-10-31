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
        "{command:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
#[ignore = "requires ARIA_TEST_WHISPER_MODEL and real native inference"]
fn desktop_dictates_through_a_server_without_local_stt() {
    let model = std::env::var_os("ARIA_TEST_WHISPER_MODEL")
        .expect("set ARIA_TEST_WHISPER_MODEL to the test model");
    let model = aria_harness::toml_string(&Path::new(&model).display().to_string()).unwrap();
    let server =
        Sandbox::create(&format!("[stt]\nbackend = \"whisper\"\nmodel = {model}\n")).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let endpoint = format!("https://{address}");
    let server_log = server.home_path().join("inference.log");
    let mut command = Command::new(env!("CARGO_BIN_EXE_aria"));
    server.apply(&mut command);
    command
        .args(["serve", "--bind", &address.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&server_log).unwrap());
    let mut host = ChildProcess::spawn(&mut command).unwrap();
    let startup = host
        .wait_until(Instant::now() + TIMEOUT, || {
            std::fs::read_to_string(&server_log)
                .unwrap()
                .contains("inference server ready")
        })
        .unwrap();
    assert!(
        matches!(startup, WaitOutcome::Cancelled),
        "server did not become ready:\n{}",
        std::fs::read_to_string(&server_log).unwrap()
    );
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
    let audio = aria_harness::toml_string(&audio.display().to_string()).unwrap();
    let device = Sandbox::create(&format!(
        "[stt]\nbackend = \"server\"\nserver = \"{endpoint}\"\n[capture]\nbackend = \"wav_file\"\npath = {audio}\n[wake]\nenabled = false\n"
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
