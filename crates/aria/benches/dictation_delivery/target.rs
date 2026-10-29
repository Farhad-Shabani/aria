use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use aria_primitives::process::{ChildProcess, WaitOutcome};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_REPLY_BYTES: u64 = 1024 * 1024;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct Target {
    process: ChildProcess,
    connection: BufReader<UnixStream>,
    _directory: tempfile::TempDir,
    trial: u64,
    pid: u32,
}

impl Target {
    pub(crate) fn launch() -> anyhow::Result<Self> {
        ensure!(
            cfg!(target_os = "macos"),
            "streaming insertion requires macOS"
        );
        let directory = tempfile::tempdir()?;
        let source = directory.path().join("target.swift");
        std::fs::write(&source, include_str!("../streaming_target.swift"))?;
        let binary = directory.path().join("streaming-target");
        let mut compiler = ChildProcess::spawn(
            Command::new("/usr/bin/xcrun")
                .args(["swiftc", "-swift-version", "6", "-O"])
                .arg(&source)
                .arg("-o")
                .arg(&binary)
                .stdin(Stdio::null()),
        )?;
        ensure!(
            matches!(
                compiler.wait_until(Instant::now() + Duration::from_secs(120), || false)?,
                WaitOutcome::Exited(status) if status.success()
            ),
            "streaming target compilation failed or timed out"
        );
        let (connection, child_connection) = UnixStream::pair()?;
        connection.set_read_timeout(Some(RESPONSE_TIMEOUT))?;
        connection.set_write_timeout(Some(RESPONSE_TIMEOUT))?;
        let process = ChildProcess::spawn(
            Command::new(binary)
                .stdin(Stdio::from(OwnedFd::from(child_connection.try_clone()?)))
                .stdout(Stdio::from(OwnedFd::from(child_connection))),
        )?;
        let mut target = Self {
            process,
            connection: BufReader::new(connection),
            _directory: directory,
            trial: 0,
            pid: 0,
        };
        let ready = target
            .receive()
            .context("target startup did not complete")?;
        ensure!(
            ready.trial == 0 && ready.pid > 0,
            "invalid target readiness reply"
        );
        target.pid = ready.pid;
        Ok(target)
    }

    pub(super) fn reset(
        &mut self,
        text: &str,
        selection_location: usize,
        selection_length: usize,
    ) -> anyhow::Result<()> {
        self.trial += 1;
        self.send(&json!({
            "op": "reset", "trial": self.trial, "text": text,
            "selection_location": selection_location, "selection_length": selection_length,
        }))?;
        let ready = self.receive().context("target reset did not complete")?;
        ready.validate_identity(self.pid, self.trial)?;
        ensure!(
            ready.active && !ready.focus_lost && ready.events.is_empty() && ready.text == text,
            "target did not reset and acquire keyboard focus"
        );
        Ok(())
    }

    pub(super) fn snapshot(&mut self) -> anyhow::Result<Snapshot> {
        self.send(&json!({"op": "snapshot", "trial": self.trial}))?;
        let snapshot = self.receive().context("target snapshot failed")?;
        snapshot.validate_identity(self.pid, self.trial)?;
        Ok(snapshot)
    }

    pub(super) async fn finish(
        &mut self,
        expected: &str,
        started_ns: u64,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let snapshot = self.snapshot()?;
            ensure!(
                snapshot.active && !snapshot.focus_lost,
                "target lost keyboard focus during trial {}; keep the desktop idle while running this benchmark",
                snapshot.trial
            );
            if snapshot.text == expected {
                return snapshot.metrics(started_ns, expected);
            }
            ensure!(
                Instant::now() < deadline,
                "target did not receive the exact dictation result within 5 seconds"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub(crate) fn stop(mut self) -> anyhow::Result<()> {
        self.send(&json!({"op": "quit"}))?;
        let reply = read_reply(&mut self.connection)?;
        ensure!(reply["ok"] == true, "target did not acknowledge shutdown");
        ensure!(
            matches!(
                self.process.wait_until(Instant::now() + Duration::from_secs(5), || false)?,
                WaitOutcome::Exited(status) if status.success()
            ),
            "target did not exit successfully"
        );
        Ok(())
    }

    fn send(&mut self, request: &Value) -> anyhow::Result<()> {
        let mut frame = serde_json::to_vec(request)?;
        ensure!(
            frame.len() < 512 * 1024,
            "target request exceeds frame limit"
        );
        frame.push(b'\n');
        self.connection
            .get_mut()
            .write_all(&frame)
            .context("failed to send target command")
    }

    fn receive(&mut self) -> anyhow::Result<Snapshot> {
        let reply = read_reply(&mut self.connection)?;
        if let Some(error) = reply.get("error") {
            anyhow::bail!(
                "streaming target: {}",
                error.as_str().unwrap_or("invalid error reply")
            );
        }
        serde_json::from_value(reply).context("invalid streaming target reply")
    }
}

fn read_reply(reader: &mut impl BufRead) -> anyhow::Result<Value> {
    let mut frame = Vec::new();
    reader
        .take(MAX_REPLY_BYTES)
        .read_until(b'\n', &mut frame)
        .context("target reply failed or timed out")?;
    ensure!(
        frame.len() < MAX_REPLY_BYTES as usize && frame.last() == Some(&b'\n'),
        "target closed its connection or exceeded the reply limit"
    );
    serde_json::from_slice(&frame).context("target reply is not JSON")
}

pub(super) fn uptime_ns() -> anyhow::Result<u64> {
    #[cfg(target_os = "macos")]
    {
        Ok(cidre::time::Clock::UptimeRaw.time_nsec_np())
    }
    #[cfg(not(target_os = "macos"))]
    {
        anyhow::bail!("streaming insertion requires macOS")
    }
}

#[derive(Deserialize)]
pub(super) struct Snapshot {
    pid: u32,
    trial: u64,
    uptime_ns: u64,
    pub(super) text: String,
    pub(super) active: bool,
    pub(super) focus_lost: bool,
    events: Vec<Receipt>,
}

#[derive(Deserialize)]
struct Receipt {
    uptime_ns: u64,
    utf8_bytes: usize,
}

impl Snapshot {
    fn validate_identity(&self, pid: u32, trial: u64) -> anyhow::Result<()> {
        ensure!(
            self.trial == trial && self.pid == pid,
            "stale or foreign target reply"
        );
        Ok(())
    }

    fn metrics(&self, started_ns: u64, expected: &str) -> anyhow::Result<Value> {
        ensure!(
            self.active && !self.focus_lost,
            "target lost keyboard focus during trial {}; keep the desktop idle while running this benchmark",
            self.trial
        );
        ensure!(
            self.text == expected,
            "target text differs from the dictation result"
        );
        ensure!(
            self.events.len() <= 4096 && self.text.len() <= 64 * 1024,
            "target observations exceed trial limits"
        );
        let first = self
            .events
            .first()
            .context("target received no text changes")?;
        let last = self
            .events
            .last()
            .context("target received no text changes")?;
        ensure!(
            last.utf8_bytes == expected.len(),
            "final receipt length differs from the result"
        );
        let mut previous_ns = started_ns;
        let mut previous_bytes = 0;
        let mut receipts = Vec::new();
        for (index, event) in self.events.iter().enumerate() {
            ensure!(
                event.uptime_ns >= previous_ns && event.uptime_ns <= self.uptime_ns,
                "target receipt is outside the trial or out of order"
            );
            ensure!(
                event.utf8_bytes > previous_bytes && event.utf8_bytes <= expected.len(),
                "target text did not grow monotonically"
            );
            receipts.push(json!({
                "elapsed_us": (event.uptime_ns - started_ns) / 1000,
                "gap_us": (index > 0).then(|| (event.uptime_ns - previous_ns) / 1000),
                "utf8_bytes": event.utf8_bytes,
            }));
            previous_ns = event.uptime_ns;
            previous_bytes = event.utf8_bytes;
        }
        Ok(json!({
            "trial": self.trial,
            "clock": "CLOCK_UPTIME_RAW",
            "request_to_first_receipt_ms": (first.uptime_ns - started_ns) / 1_000_000,
            "request_to_final_receipt_ms": (last.uptime_ns - started_ns) / 1_000_000,
            "receipts": receipts,
            "text_fingerprint": aria_harness::artifact::text_fingerprint(&self.text)?,
        }))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn receipts_require_exact_unicode_text_and_ordered_trial_times() {
        use super::{Receipt, Snapshot};
        let mut snapshot = Snapshot {
            pid: 1,
            trial: 7,
            uptime_ns: 5_000_000,
            text: "hé🙂".into(),
            active: true,
            focus_lost: false,
            events: vec![
                Receipt {
                    uptime_ns: 2_000_000,
                    utf8_bytes: 3,
                },
                Receipt {
                    uptime_ns: 4_000_000,
                    utf8_bytes: 7,
                },
            ],
        };
        assert!(snapshot.validate_identity(1, 7).is_ok());
        assert!(snapshot.validate_identity(1, 6).is_err());
        assert!(snapshot.validate_identity(2, 7).is_err());
        let metrics = snapshot.metrics(1_000_000, "hé🙂").unwrap();
        assert_eq!(metrics["request_to_first_receipt_ms"], 1);
        assert_eq!(metrics["request_to_final_receipt_ms"], 3);
        assert_eq!(metrics["receipts"][1]["gap_us"], 2000);
        assert_eq!(metrics["text_fingerprint"]["utf8_bytes"], 7);
        assert!(!serde_json::to_string(&metrics).unwrap().contains("hé🙂"));
        assert!(snapshot.metrics(3_000_000, "hé🙂").is_err());
        assert!(snapshot.metrics(1_000_000, "wrong").is_err());
        snapshot.active = false;
        assert!(snapshot.metrics(1_000_000, "hé🙂").is_err());
        snapshot.active = true;
        snapshot.focus_lost = true;
        assert!(snapshot.metrics(1_000_000, "hé🙂").is_err());
        snapshot.focus_lost = false;
        snapshot.events[1].uptime_ns = 1_500_000;
        assert!(snapshot.metrics(1_000_000, "hé🙂").is_err());
        snapshot.events[1].uptime_ns = 6_000_000;
        assert!(snapshot.metrics(1_000_000, "hé🙂").is_err());
        snapshot.events[1].uptime_ns = 4_000_000;
        snapshot.events[0].utf8_bytes = 7;
        assert!(snapshot.metrics(1_000_000, "hé🙂").is_err());
        snapshot.events.clear();
        assert!(snapshot.metrics(1_000_000, "hé🙂").is_err());
    }

    #[test]
    fn protocol_rejects_truncated_oversized_and_stalled_replies() {
        use super::{BufReader, Duration, MAX_REPLY_BYTES, UnixStream, read_reply};
        assert!(read_reply(&mut &b"{\"ok\":true}\n"[..]).is_ok());
        assert!(read_reply(&mut &b"{\"ok\":true}"[..]).is_err());
        assert!(read_reply(&mut &b"not json\n"[..]).is_err());
        assert!(read_reply(&mut &vec![b' '; MAX_REPLY_BYTES as usize][..]).is_err());
        let (connection, peer) = UnixStream::pair().unwrap();
        connection
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let mut reader = BufReader::new(connection);
        assert!(read_reply(&mut reader).is_err());
        drop(peer);
        assert!(read_reply(&mut reader).is_err());
    }
}
