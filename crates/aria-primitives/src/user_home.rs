//! Resolution and validation of the current user's home directory.

use std::path::PathBuf;

pub fn resolve() -> Result<PathBuf, UserHomeError> {
    let path = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(UserHomeError::NotSet)?;
    if !path.is_absolute() {
        return Err(UserHomeError::NotAbsolute { path });
    }
    Ok(path)
}

#[derive(Debug, thiserror::Error)]
pub enum UserHomeError {
    #[error("HOME is not set")]
    NotSet,

    #[error("HOME is not an absolute path: {}", path.display())]
    NotAbsolute { path: PathBuf },
}
