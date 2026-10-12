use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Context {
    #[default]
    Default,
    Directory(Directory),
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Directory(PathBuf);

#[derive(Debug, thiserror::Error)]
#[error(
    "project directory must be an absolute UTF-8 path without parent components, at most 4096 bytes"
)]
pub struct DirectoryError;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum State {
    Ready,
    Refreshing,
    Stale,
    #[default]
    Unavailable,
    TooLarge,
    Disabled,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub state: State,
    pub catalog_revision: u64,
    pub runtime_revision: u64,
    pub terms: usize,
    pub error: String,
}

impl Directory {
    pub fn new(path: PathBuf) -> Result<Self, DirectoryError> {
        let text = path.to_str().ok_or(DirectoryError)?;
        if !path.is_absolute()
            || text.len() > 4096
            || text.contains('\0')
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(DirectoryError);
        }
        Ok(Self(path.components().collect()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.to_str().expect("Directory validates UTF-8")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_validates_without_filesystem_access() {
        for path in ["relative", "/one/../two", "/one\0two"] {
            assert!(Directory::new(PathBuf::from(path)).is_err());
        }
        assert!(Directory::new(PathBuf::from(format!("/{}", "a".repeat(4096)))).is_err());
        assert_eq!(
            Directory::new(PathBuf::from("/missing/./project"))
                .unwrap()
                .as_str(),
            "/missing/project"
        );
    }
}
