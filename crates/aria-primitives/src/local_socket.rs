//! Exclusive ownership of a private Unix listener and its filesystem paths.

use std::fs::{self, File, Metadata, Permissions, TryLockError};
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};

use crate::private_fs::{self, PRIVATE_FILE_MODE, PrivateFileError};

type Result<T> = std::result::Result<T, LocalSocketError>;

/// A private Unix listener held under an advisory ownership lock.
///
/// The lock is acquired before a stale socket is removed and remains held for
/// the listener's lifetime. Cleanup removes the socket path only while it still
/// identifies this listener.
pub struct ExclusiveListener {
    listener: UnixListener,
    socket_path: PathBuf,
    socket_identity: SocketIdentity,
    _lock: File,
}

impl ExclusiveListener {
    pub fn bind(socket_path: impl AsRef<Path>, lock_path: impl AsRef<Path>) -> Result<Self> {
        let lock = acquire_ownership(lock_path.as_ref())?;
        let socket_path = socket_path.as_ref().to_path_buf();
        remove_stale_socket(&socket_path)?;
        let listener = bind_listener(&socket_path)?;
        let socket_identity =
            socket_identity(&socket_path)?.ok_or_else(|| LocalSocketError::SocketDisappeared {
                path: socket_path.clone(),
            })?;
        let listener = Self {
            listener,
            socket_path,
            socket_identity,
            _lock: lock,
        };

        listener.configure()?;
        listener.secure()?;
        listener.verify_identity()?;
        Ok(listener)
    }

    /// The bound listener, for a caller that accepts through its own reactor.
    /// This handle keeps owning the lock and the socket path.
    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }

    pub fn path(&self) -> &Path {
        &self.socket_path
    }

    fn configure(&self) -> Result<()> {
        self.listener
            .set_nonblocking(true)
            .map_err(|source| LocalSocketError::ConfigureSocket {
                path: self.socket_path.clone(),
                source,
            })
    }

    fn secure(&self) -> Result<()> {
        fs::set_permissions(&self.socket_path, Permissions::from_mode(PRIVATE_FILE_MODE)).map_err(
            |source| LocalSocketError::SecureSocket {
                path: self.socket_path.clone(),
                source,
            },
        )
    }

    fn verify_identity(&self) -> Result<()> {
        if socket_identity(&self.socket_path)? != Some(self.socket_identity) {
            return Err(LocalSocketError::SocketChanged {
                path: self.socket_path.clone(),
            });
        }
        Ok(())
    }
}

fn acquire_ownership(path: &Path) -> Result<File> {
    let file = open_lock(path)?;
    private_fs::prepare_file(&file).map_err(|source| LocalSocketError::UnusableLock {
        path: path.to_path_buf(),
        source,
    })?;
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => LocalSocketError::AlreadyOwned,
        TryLockError::Error(error) => LocalSocketError::AcquireLock(error),
    })?;
    write_pid(&file, path)?;
    Ok(file)
}

/// The lock doubles as a pidfile: a controller that wants the owner gone
/// reads the PID here and signals it. Only meaningful while the lock is
/// held — after a crash the contents are stale until the next owner writes.
fn write_pid(mut file: &File, path: &Path) -> Result<()> {
    let error = |source: io::Error| LocalSocketError::WritePid {
        path: path.to_path_buf(),
        source,
    };
    file.set_len(0).map_err(error)?;
    io::Write::write_all(&mut file, std::process::id().to_string().as_bytes()).map_err(error)
}

/// The PID recorded by [`ExclusiveListener::bind`], or `None` when the lock
/// is missing or holds no number. Only meaningful while the socket answers;
/// the caller establishes that first.
pub fn owner_pid(lock_path: &Path) -> Option<u32> {
    fs::read_to_string(lock_path).ok()?.trim().parse().ok()
}

fn open_lock(path: &Path) -> Result<File> {
    let flags =
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let descriptor = rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR).map_err(|source| {
        LocalSocketError::OpenLock {
            path: path.to_path_buf(),
            source: source.into(),
        }
    })?;
    Ok(File::from(descriptor))
}

fn remove_stale_socket(path: &Path) -> Result<()> {
    if socket_identity(path)?.is_none() {
        return Ok(());
    }

    fs::remove_file(path).map_err(|source| LocalSocketError::RemoveStaleSocket {
        path: path.to_path_buf(),
        source,
    })
}

fn bind_listener(path: &Path) -> Result<UnixListener> {
    UnixListener::bind(path).map_err(|source| LocalSocketError::BindSocket {
        path: path.to_path_buf(),
        source,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

impl SocketIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

fn socket_identity(path: &Path) -> Result<Option<SocketIdentity>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(LocalSocketError::InspectSocket {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    validate_socket(path, &metadata)?;
    Ok(Some(SocketIdentity::from_metadata(&metadata)))
}

fn validate_socket(path: &Path, metadata: &Metadata) -> Result<()> {
    if !metadata.file_type().is_socket() {
        return Err(LocalSocketError::NotASocket {
            path: path.to_path_buf(),
        });
    }

    let expected_uid = rustix::process::getuid().as_raw();
    if metadata.uid() != expected_uid {
        return Err(LocalSocketError::WrongSocketOwner {
            path: path.to_path_buf(),
            expected_uid,
            actual_uid: metadata.uid(),
        });
    }
    Ok(())
}

impl Drop for ExclusiveListener {
    fn drop(&mut self) {
        match socket_identity(&self.socket_path) {
            Ok(Some(identity)) if identity == self.socket_identity => {
                if let Err(error) = fs::remove_file(&self.socket_path) {
                    tracing::warn!(
                        path = %self.socket_path.display(),
                        %error,
                        "failed to remove local socket"
                    );
                }
            }
            Ok(None) => {}
            Ok(Some(_)) => {
                tracing::warn!(
                    path = %self.socket_path.display(),
                    "leaving local socket path because its identity changed"
                );
            }
            Err(error) => {
                tracing::warn!(
                    path = %self.socket_path.display(),
                    %error,
                    "leaving local socket path because it could not be verified"
                );
            }
        }
    }
}

// Two paths are in play: the lock file that grants ownership, and the socket.
// Every variant names which of them it means.
#[derive(Debug, thiserror::Error)]
pub enum LocalSocketError {
    #[error("failed to open local socket lock file {}", path.display())]
    OpenLock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("local socket lock file {} is not usable", path.display())]
    UnusableLock {
        path: PathBuf,
        #[source]
        source: PrivateFileError,
    },

    #[error("local socket endpoint is already owned by another process")]
    AlreadyOwned,

    #[error("failed to acquire local socket ownership lock")]
    AcquireLock(#[source] io::Error),

    #[error("failed to record the owner PID in lock file {}", path.display())]
    WritePid {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to inspect local socket path {}", path.display())]
    InspectSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("local socket path is not a Unix socket: {}", path.display())]
    NotASocket { path: PathBuf },

    #[error(
        "local socket {} belongs to uid {actual_uid}, expected uid {expected_uid}",
        path.display()
    )]
    WrongSocketOwner {
        path: PathBuf,
        expected_uid: u32,
        actual_uid: u32,
    },

    #[error("failed to remove stale local socket {}", path.display())]
    RemoveStaleSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to bind local socket {}", path.display())]
    BindSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to configure local socket {}", path.display())]
    ConfigureSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("local socket disappeared while binding: {}", path.display())]
    SocketDisappeared { path: PathBuf },

    #[error("failed to secure local socket permissions at {}", path.display())]
    SecureSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("local socket path changed while binding: {}", path.display())]
    SocketChanged { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_file_records_the_owner_pid() {
        let directory = tempfile::tempdir().expect("test directory is created");
        let lock_path = directory.path().join("daemon.lock");
        let _listener = ExclusiveListener::bind(directory.path().join("daemon.sock"), &lock_path)
            .expect("the listener should bind");

        let recorded = fs::read_to_string(&lock_path).expect("the lock file should be readable");

        assert_eq!(recorded, std::process::id().to_string());
        assert_eq!(owner_pid(&lock_path), Some(std::process::id()));
    }

    #[test]
    fn a_missing_or_malformed_lock_has_no_owner() {
        let directory = tempfile::tempdir().expect("test directory is created");
        let lock_path = directory.path().join("daemon.lock");

        assert_eq!(owner_pid(&lock_path), None);

        fs::write(&lock_path, "not a pid\n").expect("the lock file should be writable");
        assert_eq!(owner_pid(&lock_path), None);

        fs::write(&lock_path, " 4242\n").expect("the lock file should be writable");
        assert_eq!(owner_pid(&lock_path), Some(4242));
    }
}
