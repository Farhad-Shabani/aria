//! Hosting a daemon: configure it, run it in the foreground, or spawn it on
//! its own thread with a handle that stops it.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::runtime::{PreparedRuntimePaths, RuntimeError, RuntimePaths};

use super::assembly::{self, DaemonError};
use super::shutdown::ShutdownSignal;
use crate::config::WakeWordOverride;

/// Policy governing how long the daemon accepts work.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonLifetime {
    Persistent,
    OnDemand,
}

impl DaemonLifetime {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Persistent => "persistent",
            Self::OnDemand => "on_demand",
        }
    }
}

/// A configured daemon, ready to serve. `open()` chooses the current user's
/// runtime directory and a persistent lifetime; the builder overrides either.
pub struct Daemon {
    paths: PreparedRuntimePaths,
    lifetime: DaemonLifetime,
    wake_word: WakeWordOverride,
}

impl Daemon {
    pub fn open() -> Result<Self, RuntimeError> {
        Self::builder().open()
    }

    pub fn open_wake_word() -> Result<Self, RuntimeError> {
        Self::builder().enabling_wake_word().open()
    }

    pub fn builder() -> DaemonBuilder {
        DaemonBuilder {
            runtime_dir: None,
            lifetime: DaemonLifetime::Persistent,
            wake_word: WakeWordOverride::Inherit,
        }
    }

    /// Where the daemon writes its log, for the host to route logging.
    pub fn log_path(&self) -> &Path {
        self.paths.log_path()
    }

    /// Where clients connect.
    pub fn socket_path(&self) -> &Path {
        self.paths.socket_path()
    }

    /// Serves until SIGINT or SIGTERM. Blocks the calling thread.
    // Signals install before the build: a persistent build blocks on model
    // load, and a signal during it must queue rather than kill the process.
    pub fn run(self) -> Result<(), DaemonError> {
        let shutdown = ShutdownSignal::install().map_err(DaemonError::Shutdown)?;
        assembly::build(&self.paths, self.lifetime, self.wake_word)?
            .run(shutdown)
            .map_err(DaemonError::Run)
    }

    /// Serves on its own thread. The returned handle stops and joins it;
    /// SIGINT and SIGTERM also stop it.
    // Signals install before the build for the same reason as in `run`.
    pub fn spawn(self) -> Result<RunningDaemon, DaemonError> {
        let shutdown = ShutdownSignal::install().map_err(DaemonError::Shutdown)?;
        let inner = spawn_runtime(&self.paths, self.lifetime, self.wake_word, shutdown)?;
        Ok(RunningDaemon {
            paths: self.paths,
            inner,
        })
    }
}

pub struct DaemonBuilder {
    runtime_dir: Option<PathBuf>,
    lifetime: DaemonLifetime,
    wake_word: WakeWordOverride,
}

impl DaemonBuilder {
    /// Where the socket, lock, log, and recordings live.
    pub fn runtime_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.runtime_dir = Some(directory.into());
        self
    }

    pub fn lifetime(mut self, lifetime: DaemonLifetime) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// Monitors for the wake word whatever the settings say.
    pub fn enabling_wake_word(mut self) -> Self {
        self.wake_word = WakeWordOverride::Enable;
        self
    }

    pub fn open(self) -> Result<Daemon, RuntimeError> {
        let paths = match self.runtime_dir {
            Some(directory) => RuntimePaths::in_directory(directory).prepare()?,
            None => PreparedRuntimePaths::for_current_user()?,
        };
        Ok(Daemon {
            paths,
            lifetime: self.lifetime,
            wake_word: self.wake_word,
        })
    }
}

/// A daemon serving on its own thread.
pub struct RunningDaemon {
    paths: PreparedRuntimePaths,
    inner: SpawnedRuntime,
}

impl RunningDaemon {
    /// Where clients connect.
    pub fn socket_path(&self) -> &Path {
        self.paths.socket_path()
    }

    /// Where the daemon writes its log.
    pub fn log_path(&self) -> &Path {
        self.paths.log_path()
    }

    pub fn stopper(&self) -> DaemonStopper {
        self.inner.stopper()
    }

    /// Stops the daemon and waits for it to finish.
    pub fn stop(self) -> Result<(), DaemonFailure> {
        self.inner.stop()
    }

    /// Waits for daemon exit without requesting a stop.
    pub fn join(self) -> Result<(), DaemonFailure> {
        self.inner.join()
    }
}

/// Stops a spawned daemon from any thread, as a termination signal does.
/// Open connections get a few seconds to drain, then close.
#[derive(Clone)]
pub struct DaemonStopper {
    writer: Arc<std::os::unix::net::UnixStream>,
}

impl DaemonStopper {
    /// Requests a stop. A daemon that already stopped counts as stopped, so
    /// nothing fails; an unexpected write error is logged.
    pub fn stop(&self) {
        match (&*self.writer).write_all(b"X") {
            Ok(()) => {}
            // A full pipe already carries the message; a closed one means
            // the daemon is gone, which is what a stop wants.
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::BrokenPipe
                ) => {}
            Err(error) => tracing::warn!(%error, "failed to signal the daemon to stop"),
        }
    }
}

/// How a spawned daemon ended, when it did not end cleanly.
#[derive(Debug, thiserror::Error)]
pub enum DaemonFailure {
    #[error(transparent)]
    Terminated(#[from] DaemonError),

    #[error("the daemon thread terminated unexpectedly")]
    Panicked,
}

/// The thread and stopper of a spawned daemon, without its paths.
pub(super) struct SpawnedRuntime {
    thread: Option<JoinHandle<Result<(), DaemonError>>>,
    stopper: DaemonStopper,
}

impl SpawnedRuntime {
    pub(super) fn stopper(&self) -> DaemonStopper {
        self.stopper.clone()
    }

    pub(super) fn stop(mut self) -> Result<(), DaemonFailure> {
        self.stopper.stop();
        self.join_inner()
    }

    pub(super) fn join(mut self) -> Result<(), DaemonFailure> {
        self.join_inner()
    }

    fn join_inner(&mut self) -> Result<(), DaemonFailure> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread
            .join()
            .map_err(|_panic| DaemonFailure::Panicked)?
            .map_err(DaemonFailure::Terminated)
    }
}

impl Drop for SpawnedRuntime {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stopper.stop();
        }
        if let Err(error) = self.join_inner() {
            tracing::error!(
                error = %crate::primitives::error_chain(&error),
                "spawned daemon failed during cleanup"
            );
        }
    }
}

/// Binds in the calling thread, so a socket already owned by another
/// daemon surfaces synchronously as a bind error.
pub(super) fn spawn_runtime(
    paths: &PreparedRuntimePaths,
    lifetime: DaemonLifetime,
    wake_word: WakeWordOverride,
    shutdown: ShutdownSignal,
) -> Result<SpawnedRuntime, DaemonError> {
    spawn_server(assembly::build(paths, lifetime, wake_word)?, shutdown)
}

pub(super) fn spawn_server(
    server: super::server::Server,
    shutdown: ShutdownSignal,
) -> Result<SpawnedRuntime, DaemonError> {
    let stopper = DaemonStopper {
        writer: Arc::new(shutdown.stopper_writer().map_err(DaemonError::Shutdown)?),
    };
    let thread = thread::Builder::new()
        .name("aria-daemon".into())
        .spawn(move || server.run(shutdown).map_err(DaemonError::Run))
        .map_err(DaemonError::SpawnThread)?;
    Ok(SpawnedRuntime {
        thread: Some(thread),
        stopper,
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_stopper_byte_triggers_the_shutdown_signal() {
        let mut shutdown = ShutdownSignal::new().expect("the shutdown pipe should open");
        let stopper = DaemonStopper {
            writer: Arc::new(shutdown.stopper_writer().expect("the writer should clone")),
        };

        stopper.stop();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .expect("the test runtime should build");
        runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), shutdown.triggered()).await
            })
            .expect("the stop byte should trigger the signal within the timeout")
            .expect("the shutdown watcher should not fail");
    }
}
