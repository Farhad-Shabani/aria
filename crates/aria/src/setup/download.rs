//! Model downloads for `aria setup`. curl moves the bytes and shows its
//! progress bar; a `.part` file next to the target lets a rerun resume.

use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use signal_hook::consts::SIGINT;

use crate::primitives::process::{ChildProcess, WaitOutcome};
use aria_kit::backend::validate_model_file;

use super::SetupError;

/// A rerun resumes where curl stopped, so a stall costs at most this.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
const PART_SUFFIX: &str = ".part";
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// curl and tar write under the caller's umask, so a umask of 002 would
/// leave every model group-writable and fail the trust check.
const MODEL_FILE_MODE: u32 = 0o644;
const ARCHIVE_SUFFIX: &str = ".tar.bz2";

/// Downloads `url` to `dest` unless it is already there. `meter` lets curl
/// draw its bar on the terminal; `received` gets the byte count on disk so
/// far either way, and an error from it ends the transfer.
pub(super) fn fetch(
    url: &str,
    dest: &Path,
    meter: bool,
    received: &mut dyn FnMut(u64) -> Result<(), SetupError>,
) -> Result<(), SetupError> {
    if dest.exists() {
        return trusted(dest);
    }
    let part = partial(dest);
    discard_untrusted_part(&part)?;
    let mut command = Command::new("curl");
    command
        .args([
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
        ])
        .arg(if meter { "--progress-bar" } else { "--silent" })
        .args(["--show-error", "--continue-at", "-"])
        .arg("--output")
        .arg(&part)
        .arg(url)
        .stdin(Stdio::null());
    complete(command, &part, received)?;
    promote(&part, dest)
}

/// Downloads a bzip2 tarball and extracts one member of it to `dest`.
pub(super) fn fetch_member(
    url: &str,
    member: &str,
    dest: &Path,
    meter: bool,
    received: &mut dyn FnMut(u64) -> Result<(), SetupError>,
) -> Result<(), SetupError> {
    if dest.exists() {
        return trusted(dest);
    }
    let archive = with_suffix(dest, ARCHIVE_SUFFIX);
    let extracted = extract(url, member, dest, &archive, meter, received);
    // The archive is scratch either way; a kept one would be resumed as if
    // it were a finished download.
    if let Err(source) = fs::remove_file(&archive)
        && source.kind() != io::ErrorKind::NotFound
    {
        return Err(SetupError::Io {
            action: "remove",
            path: archive,
            source,
        });
    }
    extracted
}

fn extract(
    url: &str,
    member: &str,
    dest: &Path,
    archive: &Path,
    meter: bool,
    received: &mut dyn FnMut(u64) -> Result<(), SetupError>,
) -> Result<(), SetupError> {
    fetch(url, archive, meter, received)?;
    let part = partial(dest);
    let output = File::create(&part).map_err(|source| SetupError::Io {
        action: "create",
        path: part.clone(),
        source,
    })?;
    let mut command = Command::new("tar");
    command
        .arg("-xjf")
        .arg(archive)
        .args(["-O", member])
        .stdin(Stdio::null())
        .stdout(output);
    complete(command, &part, &mut |_| Ok(()))?;
    promote(&part, dest)
}

/// Runs the transfer to completion. An interrupt, the deadline, or an
/// error from `received` kills it and keeps `part` for the next run.
/// `received` gets the size of `part` every `PROGRESS_EVERY`.
fn complete(
    mut command: Command,
    part: &Path,
    received: &mut dyn FnMut(u64) -> Result<(), SetupError>,
) -> Result<(), SetupError> {
    let name = command.get_program().to_string_lossy().into_owned();
    let interrupt = Interrupt::arm()?;
    let mut process = ChildProcess::spawn(&mut command).map_err(|source| SetupError::Spawn {
        command: name.clone(),
        source,
    })?;
    let mut reported = Instant::now();
    let mut unwatched = None;
    let outcome = process
        .wait_until(Instant::now() + DOWNLOAD_TIMEOUT, || {
            if reported.elapsed() >= PROGRESS_EVERY {
                reported = Instant::now();
                if let Ok(file) = fs::metadata(part) {
                    unwatched = received(file.len()).err();
                }
            }
            unwatched.is_some() || interrupt.raised()
        })
        .map_err(|source| SetupError::Wait {
            command: name.clone(),
            source,
        })?;
    if let Some(error) = unwatched {
        return Err(error);
    }
    match outcome {
        WaitOutcome::Exited(status) if status.success() => Ok(()),
        WaitOutcome::Exited(status) => Err(SetupError::Command {
            command: name,
            status,
        }),
        WaitOutcome::Cancelled | WaitOutcome::TimedOut => Err(SetupError::Interrupted {
            part: part.to_path_buf(),
        }),
    }
}

/// Moves a finished `part` into place, once it is a regular file no one
/// else can write. A rejected transfer is deleted rather than left where a
/// rerun would mistake it for a finished download.
fn promote(part: &Path, dest: &Path) -> Result<(), SetupError> {
    fs::set_permissions(part, fs::Permissions::from_mode(MODEL_FILE_MODE)).map_err(|source| {
        SetupError::Io {
            action: "set the mode of",
            path: part.to_path_buf(),
            source,
        }
    })?;
    if let Err(error) = trusted(part) {
        let _ = fs::remove_file(part);
        return Err(error);
    }
    fs::rename(part, dest).map_err(|source| SetupError::Io {
        action: "rename",
        path: part.to_path_buf(),
        source,
    })
}

/// Ctrl-C ends the transfer instead of the program, because the child runs
/// in its own process group and would otherwise keep downloading unparented.
/// signal-hook cannot give SIGINT its default action back once it installs a
/// handler, so the handlers stay for the process and `TERMINATE` decides
/// which one acts: outside a transfer Ctrl-C still ends the program.
struct Interrupt;

/// True whenever no transfer is running, so the default action applies.
static TERMINATE: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static CANCELLED: OnceLock<Arc<AtomicBool>> = OnceLock::new();

impl Interrupt {
    fn arm() -> Result<Self, SetupError> {
        let terminate = match TERMINATE.get() {
            Some(flag) => flag,
            None => {
                let terminate = Arc::new(AtomicBool::new(true));
                let cancelled = Arc::new(AtomicBool::new(false));
                // Registered first so it acts first: a raised `TERMINATE`
                // ends the process before the flag below is ever read.
                signal_hook::flag::register_conditional_default(SIGINT, Arc::clone(&terminate))
                    .map_err(SetupError::Signal)?;
                signal_hook::flag::register(SIGINT, Arc::clone(&cancelled))
                    .map_err(SetupError::Signal)?;
                let _ = CANCELLED.set(cancelled);
                TERMINATE.get_or_init(|| terminate)
            }
        };
        Self::cancelled().store(false, Ordering::SeqCst);
        terminate.store(false, Ordering::SeqCst);
        Ok(Self)
    }

    fn raised(&self) -> bool {
        Self::cancelled().load(Ordering::SeqCst)
    }

    fn cancelled() -> &'static Arc<AtomicBool> {
        CANCELLED.get_or_init(|| Arc::new(AtomicBool::new(false)))
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        if let Some(terminate) = TERMINATE.get() {
            terminate.store(true, Ordering::SeqCst);
        }
    }
}

/// curl resumes into a leftover `.part`, so its existing bytes must be
/// trusted. One another user could have written is dropped and the transfer
/// starts over.
fn discard_untrusted_part(part: &Path) -> Result<(), SetupError> {
    if !part.exists() || trusted(part).is_ok() {
        return Ok(());
    }
    fs::remove_file(part).map_err(|source| SetupError::Io {
        action: "remove",
        path: part.to_path_buf(),
        source,
    })
}

/// The gate every engine reapplies at load: a regular file no other user
/// can write.
fn trusted(path: &Path) -> Result<(), SetupError> {
    validate_model_file(path.to_path_buf())
        .map(drop)
        .map_err(|source| SetupError::Untrusted {
            path: path.to_path_buf(),
            source,
        })
}

/// Where `dest` is written until it is complete.
pub(super) fn partial(dest: &Path) -> PathBuf {
    with_suffix(dest, PART_SUFFIX)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn promote_renames_the_part_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.onnx");
        let part = with_suffix(&dest, PART_SUFFIX);
        fs::write(&part, b"weights").unwrap();

        promote(&part, &dest).unwrap();

        assert!(!part.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"weights");
    }

    /// curl writes under the caller's umask, so a lax one must not fail the
    /// download it just finished.
    #[test]
    fn promote_tightens_a_part_others_could_write() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("model.onnx");
        let part = with_suffix(&dest, PART_SUFFIX);
        fs::write(&part, b"weights").unwrap();
        fs::set_permissions(&part, fs::Permissions::from_mode(0o666)).unwrap();

        promote(&part, &dest).unwrap();

        let mode = fs::metadata(&dest).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, MODEL_FILE_MODE);
    }

    #[test]
    fn a_part_others_could_write_is_not_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let part = with_suffix(&dir.path().join("model.onnx"), PART_SUFFIX);
        fs::write(&part, b"half").unwrap();
        fs::set_permissions(&part, fs::Permissions::from_mode(0o666)).unwrap();

        discard_untrusted_part(&part).unwrap();

        assert!(!part.exists());
    }

    #[test]
    fn our_own_part_is_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let part = with_suffix(&dir.path().join("model.onnx"), PART_SUFFIX);
        fs::write(&part, b"half").unwrap();

        discard_untrusted_part(&part).unwrap();

        assert_eq!(fs::read(&part).unwrap(), b"half");
    }

    #[test]
    fn suffix_keeps_the_full_file_name() {
        let path = Path::new("/models/pyannote/segmentation-3.0.onnx");
        assert_eq!(
            with_suffix(path, ARCHIVE_SUFFIX),
            Path::new("/models/pyannote/segmentation-3.0.onnx.tar.bz2")
        );
    }
}
