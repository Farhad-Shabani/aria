//! Cleanup of the temporary WAVs older builds wrote while capturing.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;

const FILE_PREFIX: &str = "aria-recording-";
const FILE_SUFFIX: &str = ".wav";
const MAX_CLEANUP_ENTRIES: usize = 256;

// Best-effort: failures are logged, never returned, so cleanup cannot block
// startup. The caller must hold the daemon's exclusive lock, because every
// matching WAV in `directory` is deleted.
pub fn remove_all_spools(directory: &Path) {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(
                directory = %directory.display(),
                %error,
                "cannot list temporary recordings"
            );
            return;
        }
    };

    let mut removed = 0;
    for (scanned, entry) in entries.enumerate() {
        if scanned == MAX_CLEANUP_ENTRIES {
            tracing::warn!(
                scanned,
                "temporary recording cleanup reached its scan limit"
            );
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "cannot read a temporary recording entry");
                continue;
            }
        };
        if !is_recording_artifact(&entry.file_name()) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!(
                path = %entry.path().display(),
                %error,
                "cannot remove an abandoned temporary recording"
            ),
        }
    }

    if removed > 0 {
        tracing::info!(removed, "removed abandoned temporary recordings");
    }
}

// The spool directory is user-configurable, so cleanup removes only files
// named like this crate's spools.
fn is_recording_artifact(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with(FILE_PREFIX) && name.ends_with(FILE_SUFFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plant(directory: &Path, name: &str) {
        std::fs::write(directory.join(name), []).expect("fixture must be writable");
    }

    #[test]
    fn cleanup_removes_only_recording_artifacts() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        plant(directory.path(), "aria-recording-999999-abc.wav");
        plant(directory.path(), "aria-recording-notapid.wav");
        plant(directory.path(), "keep-me.txt");

        remove_all_spools(directory.path());

        assert!(directory.path().join("keep-me.txt").exists());
        assert!(
            !directory
                .path()
                .join("aria-recording-999999-abc.wav")
                .exists()
        );
        assert!(!directory.path().join("aria-recording-notapid.wav").exists());
    }

    #[test]
    fn cleanup_continues_past_an_unremovable_entry() {
        let directory = tempfile::tempdir().expect("tempdir must be creatable");
        std::fs::create_dir(directory.path().join("aria-recording-666-trap.wav"))
            .expect("trap must be creatable");
        plant(directory.path(), "aria-recording-1-orphan.wav");

        remove_all_spools(directory.path());

        assert!(
            !directory
                .path()
                .join("aria-recording-1-orphan.wav")
                .exists()
        );
        assert!(
            directory
                .path()
                .join("aria-recording-666-trap.wav")
                .exists()
        );
    }
}
