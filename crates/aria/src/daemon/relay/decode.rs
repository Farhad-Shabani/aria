//! Relayed audio into the canonical 16 kHz mono PCM the engines read.
//!
//! AAC decoding runs in an `ffmpeg` child process.
//!
//! The bytes come off the LAN, so the child is fenced in: it may open no
//! protocol but its own pipes, decoded output is bounded, and it is killed
//! when it outstays [`DECODE_TIMEOUT`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aria_kit::primitives::process::{ChildProcess, ChildProcessError, WaitOutcome};

use crate::audio::{CANONICAL_SAMPLE_RATE, canonical_samples};

/// Deadline for the decoder child.
const DECODE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_DECODE_DETAIL_BYTES: u64 = 4 * 1024;

/// Bounds decoded duration separately from compressed upload size.
const MAX_DECODED_SECONDS: u64 = 600;

/// Clients stop recording at [`MAX_DECODED_SECONDS`]. AAC frame padding and
/// recorder overshoot run slightly past it, so a full-length recording still fits.
const DECODE_GRACE_SECONDS: u64 = 1;

/// What the duration cap and its grace are worth at the canonical rate, in
/// bytes of 16-bit mono. A child that ignores `-t` is cut off here.
const MAX_PCM_BYTES: u64 = canonical_samples(Duration::from_secs(
    MAX_DECODED_SECONDS + DECODE_GRACE_SECONDS,
)) * 2;

/// Finds an executable `ffmpeg` on this process's PATH without running it.
pub(crate) fn ffmpeg_path() -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|directory| directory.join("ffmpeg"))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Decodes AAC to canonical mono PCM. The source rate rides in the stream, so
/// ffmpeg is told what to produce rather than what to read.
pub(super) fn aac_to_pcm16(aac: &[u8]) -> Result<Vec<i16>, DecodeError> {
    if aac.is_empty() {
        return Err(DecodeError::Empty);
    }
    let pcm = run_ffmpeg(aac)?;
    if pcm.is_empty() {
        return Err(DecodeError::NoOutput);
    }
    Ok(samples_from(&pcm))
}

/// Reinterprets little-endian 16-bit PCM. A trailing odd byte is a truncated
/// sample, so the audio is not what the client meant to send.
pub(super) fn pcm16_from_bytes(bytes: &[u8]) -> Result<Vec<i16>, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    if !bytes.len().is_multiple_of(2) {
        return Err(DecodeError::OddPcmLength { bytes: bytes.len() });
    }
    // The same ceiling AAC gets, applied where PCM's own length is the only
    // thing bounding it.
    if bytes.len() as u64 > MAX_PCM_BYTES {
        return Err(DecodeError::TooLong);
    }
    Ok(samples_from(bytes))
}

fn samples_from(bytes: &[u8]) -> Vec<i16> {
    bytes
        .as_chunks()
        .0
        .iter()
        .copied()
        .map(i16::from_le_bytes)
        .collect()
}

/// Each pipe gets its own thread. A payload at the size cap is far larger
/// than a pipe buffer, so feeding the child before reading its output would
/// deadlock.
fn run_ffmpeg(aac: &[u8]) -> Result<Vec<u8>, DecodeError> {
    // Share executable lookup with doctor; each uses its own process environment.
    let ffmpeg = ffmpeg_path().ok_or(DecodeError::FfmpegMissing)?;
    let mut command = Command::new(ffmpeg);
    command
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            // Never read the terminal, and never open anything but the pipes
            // below. Without this a crafted playlist reaches `file:` and
            // `http:` — the LAN would be handing the Mac a fetch primitive.
            "-nostdin",
            "-protocol_whitelist",
            "pipe",
            "-i",
            "pipe:0",
            // Decode past the accepted duration so overflow is rejected, not truncated.
            "-t",
            &(MAX_DECODED_SECONDS + DECODE_GRACE_SECONDS + 1).to_string(),
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-ar",
            &CANONICAL_SAMPLE_RATE.to_string(),
            "-ac",
            "1",
            "pipe:1",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = ChildProcess::spawn(&mut command).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => DecodeError::FfmpegMissing,
        _ => DecodeError::Spawn(source),
    })?;
    let stdin = child.take_stdin().ok_or(DecodeError::PipesGone)?;
    let stdout = child.take_stdout().ok_or(DecodeError::PipesGone)?;
    let mut stderr = child.take_stderr().ok_or(DecodeError::PipesGone)?;

    // Scoped, so the payload is borrowed rather than copied a second time.
    thread::scope(|threads| {
        // `stdin` is moved in so it drops here rather than at the end of this
        // function: closing the pipe is what tells ffmpeg the input ended,
        // and one held open is a child that waits forever for more.
        //
        // A child that died before reading closes the pipe from its side. Its
        // exit status says why, and a broken pipe here would only bury it.
        let feeding = threads.spawn(move || {
            let mut stdin = stdin;
            drop(stdin.write_all(aac).and_then(|()| stdin.flush()));
        });
        let draining = threads.spawn(move || {
            let mut pcm = Vec::new();
            // One byte past the cap detects overflow without retaining the full decode.
            stdout
                .take(MAX_PCM_BYTES + 1)
                .read_to_end(&mut pcm)
                .map(|_read| pcm)
        });
        let complaining = threads.spawn(move || read_decode_detail(&mut stderr));

        let outcome = child.wait_until(Instant::now() + DECODE_TIMEOUT, || false);
        // Terminate before joining the pipes, including when waiting fails:
        // scoped threads finish before the child's drop can close those pipes.
        if !matches!(outcome, Ok(WaitOutcome::Exited(_))) {
            drop(child.terminate_in_place());
        }
        drop(feeding.join());
        let pcm = draining.join();
        let detail = complaining.join().unwrap_or_default();
        let outcome = outcome.map_err(DecodeError::Wait)?;
        let pcm = pcm
            .map_err(|_panic| DecodeError::PipesGone)?
            .map_err(DecodeError::Read)?;

        if pcm.len() as u64 > MAX_PCM_BYTES {
            return Err(DecodeError::TooLong);
        }
        match outcome {
            WaitOutcome::Exited(status) if status.success() => Ok(pcm),
            WaitOutcome::Exited(_status) => Err(DecodeError::Failed {
                detail: detail.trim().to_owned(),
            }),
            // Nothing here cancels; `wait_until` is only told how long to wait.
            WaitOutcome::TimedOut | WaitOutcome::Cancelled => Err(DecodeError::TimedOut),
        }
    })
}

fn read_decode_detail(stderr: &mut impl Read) -> String {
    let mut detail = Vec::new();
    drop(
        stderr
            .take(MAX_DECODE_DETAIL_BYTES)
            .read_to_end(&mut detail),
    );
    drop(std::io::copy(stderr, &mut std::io::sink()));
    String::from_utf8_lossy(&detail).into_owned()
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DecodeError {
    #[error("the relay stream carried no audio")]
    Empty,

    #[error("relayed PCM ended mid-sample: {bytes} bytes is not a whole number of samples")]
    OddPcmLength { bytes: usize },

    #[error("the relayed recording is longer than the {MAX_DECODED_SECONDS} second limit")]
    TooLong,

    #[error("ffmpeg is not installed; the relay needs it to decode AAC (brew install ffmpeg)")]
    FfmpegMissing,

    #[error("failed to start ffmpeg")]
    Spawn(#[source] std::io::Error),

    #[error("ffmpeg started without the pipes the decode reads")]
    PipesGone,

    #[error("failed to read ffmpeg's output")]
    Read(#[source] std::io::Error),

    #[error("failed to wait for ffmpeg")]
    Wait(#[source] ChildProcessError),

    #[error("ffmpeg took longer than {DECODE_TIMEOUT:?} to decode the relayed audio")]
    TimedOut,

    #[error("ffmpeg could not decode the relayed audio: {detail}")]
    Failed { detail: String },

    #[error("ffmpeg decoded the relayed audio to nothing")]
    NoOutput,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audio encoded to AAC by ffmpeg, for decoding back. The fixture is
    /// generated rather than committed: the encoder is the binary the decoder
    /// is, so a machine without it skips rather than fails on a file it could
    /// not have read either.
    fn aac_fixture(source: &str, container: &str) -> Option<Vec<u8>> {
        let ffmpeg = ffmpeg_path()?;
        let mut command = Command::new(ffmpeg);
        command.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            source,
            "-ac",
            "1",
            "-c:a",
            "aac",
        ]);
        if container == "mp4" {
            command.args(["-movflags", "frag_keyframe+empty_moov"]);
        }
        command
            .args(["-f", container, "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = ChildProcess::spawn(&mut command).ok()?;
        let mut stdout = child.take_stdout()?;
        let draining = thread::spawn(move || {
            let mut aac = Vec::new();
            stdout.read_to_end(&mut aac).map(|_read| aac)
        });
        let outcome = child
            .wait_until(Instant::now() + DECODE_TIMEOUT, || false)
            .ok()?;
        let aac = draining.join().ok()?.ok()?;
        matches!(outcome, WaitOutcome::Exited(status) if status.success()).then_some(aac)
    }

    #[test]
    fn aac_decodes_to_a_second_of_canonical_audio() {
        let Some(aac) = aac_fixture("sine=frequency=440:duration=1", "adts") else {
            eprintln!("skipped: no usable ffmpeg on PATH");
            return;
        };

        let samples = aac_to_pcm16(&aac).expect("the fixture should decode");

        let seconds = samples.len() as f64 / f64::from(CANONICAL_SAMPLE_RATE);
        assert!(
            (0.9..1.2).contains(&seconds),
            "a one-second tone decoded to {seconds} seconds"
        );
    }

    #[test]
    fn m4a_uses_its_embedded_sample_rate() {
        let Some(aac) = aac_fixture("sine=frequency=440:duration=1:sample_rate=22050", "mp4")
        else {
            eprintln!("skipped: no usable ffmpeg on PATH");
            return;
        };

        let samples = aac_to_pcm16(&aac).expect("M4A decodes at its embedded rate");

        let seconds = samples.len() as f64 / f64::from(CANONICAL_SAMPLE_RATE);
        assert!((0.9..1.2).contains(&seconds));
    }

    /// The cap that keeps a small upload from becoming gigabytes of PCM.
    ///
    /// The overlong input is one encoded second repeated, not ten minutes
    /// encoded: ADTS is a bare frame stream, so concatenation is valid AAC,
    /// and encoding the real duration costs enough CPU to starve whatever
    /// else the suite is running.
    #[test]
    fn audio_past_the_duration_cap_is_refused() {
        let Some(second) = aac_fixture("sine=frequency=440:duration=1", "adts") else {
            eprintln!("skipped: no usable ffmpeg on PATH");
            return;
        };
        let overlong = second.repeat(MAX_DECODED_SECONDS as usize + 120);

        let result = aac_to_pcm16(&overlong);

        assert!(matches!(result, Err(DecodeError::TooLong)));
    }

    #[test]
    fn audio_that_is_not_aac_fails_loudly() {
        if ffmpeg_path().is_none() {
            eprintln!("skipped: no ffmpeg on PATH");
            return;
        }

        let error = aac_to_pcm16(b"this is not audio").expect_err("garbage is not AAC");

        assert!(matches!(
            error,
            DecodeError::Failed { .. } | DecodeError::NoOutput
        ));
    }

    /// The child may open its own pipes and nothing else, so a playlist
    /// naming a local file cannot make it fetch one.
    #[test]
    fn a_playlist_cannot_reach_the_filesystem() {
        if ffmpeg_path().is_none() {
            eprintln!("skipped: no ffmpeg on PATH");
            return;
        }
        let playlist = b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\nfile:///etc/passwd\n";

        let error = aac_to_pcm16(playlist).expect_err("no protocol but pipe is allowed");

        assert!(matches!(
            error,
            DecodeError::Failed { .. } | DecodeError::NoOutput
        ));
    }

    #[test]
    fn pcm_bytes_become_samples_little_endian() {
        let samples = pcm16_from_bytes(&[0x01, 0x00, 0xff, 0xff]).expect("whole samples decode");

        assert_eq!(samples, [1, -1]);
    }

    #[test]
    fn pcm_that_ends_mid_sample_is_refused() {
        let error = pcm16_from_bytes(&[0x01, 0x00, 0xff]).expect_err("a half sample is not audio");

        assert!(matches!(error, DecodeError::OddPcmLength { bytes: 3 }));
    }

    #[test]
    fn a_full_length_recording_with_codec_padding_fits() {
        let padded = Duration::from_secs(MAX_DECODED_SECONDS) + Duration::from_millis(200);
        let bytes = vec![0u8; canonical_samples(padded) as usize * 2];

        assert!(pcm16_from_bytes(&bytes).is_ok());
    }

    #[test]
    fn pcm_past_the_duration_cap_is_refused() {
        let overlong = vec![0u8; MAX_PCM_BYTES as usize + 2];
        assert!(pcm16_from_bytes(&overlong[..MAX_PCM_BYTES as usize]).is_ok());

        let error = pcm16_from_bytes(&overlong).expect_err("the duration cap covers PCM too");

        assert!(matches!(error, DecodeError::TooLong));
    }

    #[test]
    fn decoder_detail_is_bounded_without_leaving_the_pipe_undrained() {
        let bytes = vec![b'x'; MAX_DECODE_DETAIL_BYTES as usize * 4];
        let mut stderr = std::io::Cursor::new(bytes);

        let detail = read_decode_detail(&mut stderr);

        assert_eq!(detail.len(), MAX_DECODE_DETAIL_BYTES as usize);
        assert!(detail.bytes().all(|byte| byte == b'x'));
        assert_eq!(stderr.position(), stderr.get_ref().len() as u64);
    }

    #[test]
    fn empty_audio_is_refused_before_a_child_starts() {
        assert!(matches!(aac_to_pcm16(&[]), Err(DecodeError::Empty)));
        assert!(matches!(pcm16_from_bytes(&[]), Err(DecodeError::Empty)));
    }
}
