//! Inspection of filesystem targets: canonical paths, metadata snapshots,
//! executability, and whether the daemon may rely on them.

use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::PathBuf;

const GROUP_OR_OTHER_WRITE: u32 = 0o022;

pub struct InspectedFile {
    path: PathBuf,
    metadata: Metadata,
}

impl InspectedFile {
    pub fn inspect(path: PathBuf) -> Result<Self, InspectionError> {
        if !path.is_absolute() {
            return Err(InspectionError::NotAbsolute { path });
        }

        let path = fs::canonicalize(&path).map_err(|source| InspectionError::Inspect {
            path: path.clone(),
            source,
        })?;
        let metadata = fs::metadata(&path).map_err(|source| InspectionError::Inspect {
            path: path.clone(),
            source,
        })?;

        Ok(Self { path, metadata })
    }

    pub fn require_regular_file(self) -> Result<Self, InspectionError> {
        if self.metadata.is_file() {
            return Ok(self);
        }
        Err(InspectionError::NotRegularFile {
            path: self.into_path(),
        })
    }

    pub fn require_directory(self) -> Result<Self, InspectionError> {
        if self.metadata.is_dir() {
            return Ok(self);
        }
        Err(InspectionError::NotDirectory {
            path: self.into_path(),
        })
    }

    #[cfg(not(target_os = "ios"))]
    pub fn require_executable(self) -> Result<Self, InspectionError> {
        let runnable = self.metadata.is_file()
            && rustix::fs::access(&self.path, rustix::fs::Access::EXEC_OK).is_ok();
        if runnable {
            return Ok(self);
        }
        Err(InspectionError::NotExecutable {
            path: self.into_path(),
        })
    }

    pub fn is_regular_file(&self) -> bool {
        self.metadata.is_file()
    }

    pub fn require_trusted(self) -> Result<PathBuf, InspectionError> {
        let expected_uid = rustix::process::getuid().as_raw();
        let actual_uid = self.metadata.uid();
        if actual_uid != expected_uid && actual_uid != 0 {
            return Err(InspectionError::UntrustedOwner {
                path: self.into_path(),
                expected_uid,
                actual_uid,
            });
        }

        let actual_mode = self.metadata.permissions().mode() & 0o777;
        if actual_mode & GROUP_OR_OTHER_WRITE != 0 {
            return Err(InspectionError::WritableByOthers {
                path: self.into_path(),
                actual_mode,
            });
        }
        Ok(self.into_path())
    }

    pub fn into_path(self) -> PathBuf {
        self.path
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InspectionError {
    #[error("path must be absolute: {}", path.display())]
    NotAbsolute { path: PathBuf },

    #[error("failed to inspect {}", path.display())]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("path is not a regular file: {}", path.display())]
    NotRegularFile { path: PathBuf },

    #[error("path is not a directory: {}", path.display())]
    NotDirectory { path: PathBuf },

    #[error("path is not a runnable regular file: {}", path.display())]
    NotExecutable { path: PathBuf },

    #[error("{} belongs to uid {actual_uid}; expected uid {expected_uid} or root", path.display())]
    UntrustedOwner {
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
    },

    #[error(
        "{} has mode {actual_mode:o}; group and other users must not have write access",
        path.display()
    )]
    WritableByOthers { path: PathBuf, actual_mode: u32 },
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use tempfile::TempDir;

    use super::*;

    #[cfg(not(target_os = "ios"))]
    const EXECUTABLE_MODE: u32 = 0o750;
    const REGULAR_FILE_MODE: u32 = 0o640;
    const GROUP_WRITABLE_MODE: u32 = 0o660;

    struct FileFixture {
        _directory: TempDir,
        target: PathBuf,
        link: PathBuf,
    }

    impl FileFixture {
        fn with_mode(mode: u32) -> Self {
            let directory = TempDir::new().expect("test directory should be created");
            let target = directory.path().join("target");
            let link = directory.path().join("link");

            fs::write(&target, []).expect("test target should be written");
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))
                .expect("test target permissions should be set");
            symlink(&target, &link).expect("test symlink should be created");

            Self {
                _directory: directory,
                target,
                link,
            }
        }

        fn canonical_target(&self) -> PathBuf {
            fs::canonicalize(&self.target).expect("test target should have a canonical path")
        }
    }

    #[test]
    fn inspect_rejects_a_relative_path() {
        let relative = PathBuf::from("relative/file");

        let result = InspectedFile::inspect(relative.clone());

        assert!(matches!(
            result,
            Err(InspectionError::NotAbsolute { path }) if path == relative
        ));
    }

    #[test]
    fn inspect_reports_the_canonical_target_and_its_metadata() {
        let fixture = FileFixture::with_mode(REGULAR_FILE_MODE);

        let file =
            InspectedFile::inspect(fixture.link.clone()).expect("test symlink should be inspected");

        assert!(file.is_regular_file());
        assert_eq!(file.into_path(), fixture.canonical_target());
    }

    #[cfg(not(target_os = "ios"))]
    #[test]
    fn inspect_accepts_a_runnable_regular_file() {
        let fixture = FileFixture::with_mode(EXECUTABLE_MODE);
        let expected_path = fixture.canonical_target();

        let executable = InspectedFile::inspect(fixture.link)
            .and_then(InspectedFile::require_executable)
            .expect("test executable should be inspected");

        assert_eq!(executable.into_path(), expected_path);
    }

    #[test]
    fn a_file_only_its_owner_can_write_is_trusted() {
        let fixture = FileFixture::with_mode(REGULAR_FILE_MODE);
        let file = InspectedFile::inspect(fixture.link.clone()).expect("the file should inspect");

        let trusted = file
            .require_trusted()
            .expect("an owned private file is trusted");

        assert_eq!(trusted, fixture.canonical_target());
    }

    #[test]
    fn a_file_other_users_can_write_is_refused() {
        let fixture = FileFixture::with_mode(GROUP_WRITABLE_MODE);
        let file = InspectedFile::inspect(fixture.link).expect("the file should inspect");

        let error = file
            .require_trusted()
            .expect_err("a group-writable file must be refused");

        assert!(matches!(
            error,
            InspectionError::WritableByOthers { actual_mode, .. } if actual_mode == GROUP_WRITABLE_MODE
        ));
    }

    #[cfg(not(target_os = "ios"))]
    #[test]
    fn inspect_rejects_a_regular_file_that_is_not_runnable() {
        let fixture = FileFixture::with_mode(REGULAR_FILE_MODE);
        let expected_path = fixture.canonical_target();

        let result =
            InspectedFile::inspect(fixture.link).and_then(InspectedFile::require_executable);

        assert!(matches!(
            result,
            Err(InspectionError::NotExecutable { path }) if path == expected_path
        ));
    }
}
