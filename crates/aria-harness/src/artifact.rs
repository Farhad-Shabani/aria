use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize, Deserialize)]
pub struct Provenance {
    pub run_id: String,
    pub host: String,
    pub os: String,
    pub arch: String,
    pub debug_assertions: bool,
    pub executable_sha256: String,
}

impl Provenance {
    pub fn capture() -> Result<Self, ArtifactError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ArtifactError::Clock)?;
        let system = rustix::system::uname();
        Ok(Self {
            run_id: format!("{}-{}", now.as_nanos(), std::process::id()),
            host: system.nodename().to_string_lossy().into_owned(),
            os: format!(
                "{} {}",
                system.sysname().to_string_lossy(),
                system.release().to_string_lossy()
            ),
            arch: system.machine().to_string_lossy().into_owned(),
            debug_assertions: cfg!(debug_assertions),
            executable_sha256: fingerprint(&std::env::current_exe()?)?,
        })
    }
}

pub fn fingerprint(path: &Path) -> Result<String, ArtifactError> {
    let mut digest = Sha256::new();
    hash_path(path, path, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn hash_path(root: &Path, path: &Path, digest: &mut Sha256) -> Result<(), ArtifactError> {
    let metadata = fs::metadata(path)?;
    if metadata.is_file() {
        let mut file = File::open(path)?;
        let mut buffer = [0; 65536];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
        }
        return Ok(());
    }
    if !metadata.is_dir() || fs::symlink_metadata(path)?.is_symlink() {
        return Err(ArtifactError::Unsupported(path.display().to_string()));
    }
    digest.update(b"directory\0");
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    for entry in entries {
        let relative = entry.strip_prefix(root).expect("entry is below root");
        let name = relative.as_os_str().as_encoded_bytes();
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name);
        // Each child has its own digest so file boundaries cannot collide.
        let mut child = Sha256::new();
        hash_path(root, &entry, &mut child)?;
        digest.update(child.finalize());
    }
    Ok(())
}

pub fn fingerprint_value(value: &impl Serialize) -> Result<String, ArtifactError> {
    let encoded = serde_json::to_vec(value)?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

pub fn write_json(path: &Path, value: &impl Serialize) -> Result<(), ArtifactError> {
    write_artifact(path, |file| {
        serde_json::to_writer_pretty(&mut *file, value)?;
        writeln!(file)?;
        Ok(())
    })
}

pub fn write_jsonl(path: &Path, rows: &[impl Serialize]) -> Result<(), ArtifactError> {
    write_artifact(path, |file| {
        for row in rows {
            serde_json::to_writer(&mut *file, row)?;
            writeln!(file)?;
        }
        Ok(())
    })
}

fn write_artifact(
    path: &Path,
    serialize: impl FnOnce(&mut File) -> Result<(), ArtifactError>,
) -> Result<(), ArtifactError> {
    let parent = path.parent().filter(|path| !path.as_os_str().is_empty());
    let mut pending = tempfile::NamedTempFile::new_in(parent.unwrap_or(Path::new(".")))?;
    serialize(pending.as_file_mut())?;
    pending.as_file_mut().sync_all()?;
    pending
        .persist_noclobber(path)
        .map_err(|error| ArtifactError::Io(error.error))?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ArtifactError {
    #[error("benchmark artifact I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("benchmark artifact encoding failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot fingerprint directory symlink or special file: {0}")]
    Unsupported(String),
    #[error("system time precedes the Unix epoch")]
    Clock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn identity_tracks_contents_and_names_not_directory_location() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for directory in [first.path(), second.path()] {
            fs::write(directory.join("weights"), b"abcd").unwrap();
        }
        let original = fingerprint(first.path()).unwrap();
        assert_eq!(original, fingerprint(second.path()).unwrap());
        fs::write(second.path().join("weights"), b"abce").unwrap();
        assert_ne!(original, fingerprint(second.path()).unwrap());
        fs::write(second.path().join("weights"), b"abcd").unwrap();
        fs::rename(second.path().join("weights"), second.path().join("other")).unwrap();
        assert_ne!(original, fingerprint(second.path()).unwrap());
    }

    #[test]
    fn artifacts_are_private_complete_and_never_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("run.json");
        write_json(&path, &vec![1, 2, 3]).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(write_json(&path, &vec![4]).is_err());
        let saved: Vec<u32> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved, vec![1, 2, 3]);
    }

    #[test]
    fn directory_symlinks_cannot_recurse_forever() {
        let directory = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(directory.path(), directory.path().join("cycle")).unwrap();
        assert!(matches!(
            fingerprint(directory.path()),
            Err(ArtifactError::Unsupported(_))
        ));
    }
}
