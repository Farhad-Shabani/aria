//! Filesystem primitives for state that must remain private to the current user.
//!
//! Errors here name the fault, not the path. The caller knows which of its
//! files it was preparing, and says so when it wraps the failure.

use std::fs::{self, DirBuilder, File, Metadata, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
pub const PRIVATE_FILE_MODE: u32 = 0o600;

pub fn prepare_directory(path: &Path) -> Result<(), PrivateDirectoryError> {
    match DirBuilder::new().mode(PRIVATE_DIRECTORY_MODE).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => return Err(PrivateDirectoryError::Create(source)),
    }

    let metadata = fs::symlink_metadata(path).map_err(PrivateDirectoryError::Inspect)?;
    if !metadata.file_type().is_dir() {
        return Err(PrivateDirectoryError::NotDirectory);
    }

    let expected_uid = rustix::process::getuid().as_raw();
    if metadata.uid() != expected_uid {
        return Err(PrivateDirectoryError::WrongOwner {
            expected_uid,
            actual_uid: metadata.uid(),
        });
    }

    if metadata.permissions().mode() & 0o777 != PRIVATE_DIRECTORY_MODE {
        fs::set_permissions(path, Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
            .map_err(PrivateDirectoryError::SetPermissions)?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PrivateDirectoryError {
    #[error("failed to create the directory")]
    Create(#[source] io::Error),

    #[error("failed to inspect the directory")]
    Inspect(#[source] io::Error),

    #[error("the path is not a directory")]
    NotDirectory,

    #[error("the directory belongs to uid {actual_uid}, expected uid {expected_uid}")]
    WrongOwner { expected_uid: u32, actual_uid: u32 },

    #[error("failed to secure the directory permissions")]
    SetPermissions(#[source] io::Error),
}

/// For files the daemon writes, and therefore owns: a drifted mode is repaired.
pub fn prepare_file(file: &File) -> Result<Metadata, PrivateFileError> {
    let metadata = own_regular_file(file)?;
    if metadata.permissions().mode() & 0o777 != PRIVATE_FILE_MODE {
        file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
            .map_err(PrivateFileError::SetPermissions)?;
    }
    Ok(metadata)
}

/// For files the daemon reads back: a drifted mode is refused, never repaired.
pub fn verify_file(file: &File) -> Result<Metadata, PrivateFileError> {
    let metadata = own_regular_file(file)?;
    let actual_mode = metadata.permissions().mode() & 0o777;
    if actual_mode != PRIVATE_FILE_MODE {
        return Err(PrivateFileError::NotPrivate {
            expected_mode: PRIVATE_FILE_MODE,
            actual_mode,
        });
    }
    Ok(metadata)
}

fn own_regular_file(file: &File) -> Result<Metadata, PrivateFileError> {
    let metadata = file.metadata().map_err(PrivateFileError::Inspect)?;
    if !metadata.is_file() {
        return Err(PrivateFileError::NotRegularFile);
    }

    let expected_uid = rustix::process::getuid().as_raw();
    if metadata.uid() != expected_uid {
        return Err(PrivateFileError::WrongOwner {
            expected_uid,
            actual_uid: metadata.uid(),
        });
    }
    Ok(metadata)
}

#[derive(Debug, thiserror::Error)]
pub enum PrivateFileError {
    #[error("failed to inspect the file")]
    Inspect(#[source] io::Error),

    #[error("the path is not a regular file")]
    NotRegularFile,

    #[error("the file belongs to uid {actual_uid}, expected uid {expected_uid}")]
    WrongOwner { expected_uid: u32, actual_uid: u32 },

    #[error("the file has mode {actual_mode:o}; expected {expected_mode:o}")]
    NotPrivate {
        expected_mode: u32,
        actual_mode: u32,
    },

    #[error("failed to secure the file permissions")]
    SetPermissions(#[source] io::Error),
}
