//! Per-user paths shared by the daemon and its clients.

use std::path::{Path, PathBuf};

use crate::primitives::private_fs::{PrivateDirectoryError, prepare_directory};

type Result<T> = std::result::Result<T, RuntimeError>;

pub(crate) const RUNTIME_DIR_ENV: &str = "ARIA_RUNTIME_DIR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimePaths {
    directory: PathBuf,
    spool_dir: PathBuf,
    socket: PathBuf,
    lock: PathBuf,
    log: PathBuf,
    status: PathBuf,
}

/// Runtime paths whose private directories have been created and validated.
#[derive(Debug)]
pub struct PreparedRuntimePaths {
    paths: RuntimePaths,
}

impl RuntimePaths {
    pub fn for_current_user() -> Self {
        match std::env::var_os(RUNTIME_DIR_ENV) {
            Some(directory) => Self::in_directory(PathBuf::from(directory)),
            None => Self::for_uid(rustix::process::getuid().as_raw()),
        }
    }

    /// Creates and validates the private directory containing runtime state.
    pub fn prepare(self) -> Result<PreparedRuntimePaths> {
        if !self.directory.is_absolute() {
            return Err(RuntimeError::NotAbsolute {
                path: self.directory,
            });
        }
        prepare_private_directory(&self.directory)?;
        prepare_private_directory(&self.spool_dir)?;
        Ok(PreparedRuntimePaths { paths: self })
    }

    fn for_uid(uid: u32) -> Self {
        Self::in_directory(PathBuf::from(format!("/tmp/aria-{uid}")))
    }

    pub(crate) fn in_directory(directory: PathBuf) -> Self {
        let spool_dir = directory.join("recordings");
        let socket = directory.join("daemon.sock");
        let lock = directory.join("daemon.lock");
        let log = directory.join("daemon.log");
        let status = directory.join("status");
        Self {
            directory,
            spool_dir,
            socket,
            lock,
            log,
            status,
        }
    }
}

impl PreparedRuntimePaths {
    pub(crate) fn directory(&self) -> &Path {
        &self.paths.directory
    }

    /// Directory for temporary recordings owned by the daemon.
    pub(crate) fn spool_dir(&self) -> &Path {
        &self.paths.spool_dir
    }

    /// Local RPC socket shared by the daemon and its clients.
    pub fn socket_path(&self) -> &Path {
        &self.paths.socket
    }

    /// Advisory lock that protects exclusive daemon ownership.
    pub(crate) fn lock_path(&self) -> &Path {
        &self.paths.lock
    }

    /// Private daemon log destination.
    pub fn log_path(&self) -> &Path {
        &self.paths.log
    }

    /// What the daemon is doing, for an external indicator to read. Empty
    /// contents mean idle.
    pub(crate) fn status_path(&self) -> &Path {
        &self.paths.status
    }
}

fn prepare_private_directory(path: &Path) -> Result<()> {
    prepare_directory(path).map_err(|source| RuntimeError::Directory {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("runtime directory path must be absolute: {}", path.display())]
    NotAbsolute { path: PathBuf },

    #[error("runtime directory {} is not usable", path.display())]
    Directory {
        path: PathBuf,
        #[source]
        source: PrivateDirectoryError,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    /// The Mac app finds the socket and lock by these names on its own.
    /// Renaming either breaks its liveness probe and stop button.
    #[test]
    fn runtime_file_names_match_the_client_contract() {
        let paths = RuntimePaths::in_directory("/tmp/contract".into());

        assert!(paths.socket.ends_with("daemon.sock"));
        assert!(paths.lock.ends_with("daemon.lock"));
    }

    #[test]
    fn prepare_creates_a_private_runtime_directory() {
        let fixture = Fixture::new();
        let paths = fixture.runtime_paths();

        paths
            .clone()
            .prepare()
            .expect("runtime directory should be created");

        let metadata =
            fs::symlink_metadata(&paths.directory).expect("runtime directory should exist");
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn prepare_repairs_runtime_directory_permissions() {
        let fixture = Fixture::new();
        let paths = fixture.runtime_paths();
        fs::create_dir(&paths.directory).expect("runtime directory should be created");
        fs::set_permissions(&paths.directory, fs::Permissions::from_mode(0o755))
            .expect("test permissions should be set");

        paths
            .clone()
            .prepare()
            .expect("permissions should be repaired");

        let metadata =
            fs::symlink_metadata(&paths.directory).expect("runtime directory should exist");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn prepare_rejects_a_relative_directory() {
        let paths = RuntimePaths::in_directory(PathBuf::from("relative-runtime"));

        let error = paths
            .clone()
            .prepare()
            .expect_err("a relative runtime directory must be rejected");

        assert!(matches!(error, RuntimeError::NotAbsolute { path } if path == paths.directory));
    }

    #[test]
    fn prepare_rejects_a_regular_file() {
        let fixture = Fixture::new();
        let paths = fixture.runtime_paths();
        fs::write(&paths.directory, []).expect("test file should be created");

        let error = paths
            .clone()
            .prepare()
            .expect_err("a file is not a runtime directory");

        assert!(matches!(
            error,
            RuntimeError::Directory {
                path,
                source: PrivateDirectoryError::NotDirectory,
            } if path == paths.directory
        ));
    }

    #[test]
    fn prepare_rejects_a_symbolic_link() {
        let fixture = Fixture::new();
        let paths = fixture.runtime_paths();
        let target = fixture.root().join("target");
        fs::create_dir(&target).expect("symlink target should be created");
        symlink(target, &paths.directory).expect("runtime symlink should be created");

        let error = paths
            .clone()
            .prepare()
            .expect_err("a symlink is not a runtime directory");

        assert!(matches!(
            error,
            RuntimeError::Directory {
                path,
                source: PrivateDirectoryError::NotDirectory,
            } if path == paths.directory
        ));
    }

    struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().expect("test directory should be created"),
            }
        }

        fn root(&self) -> &Path {
            self.root.path()
        }

        fn runtime_paths(&self) -> RuntimePaths {
            RuntimePaths::in_directory(self.root().join("runtime"))
        }
    }
}
