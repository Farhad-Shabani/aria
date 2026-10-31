//! A daemon on a thread of the test process, driven through `DaemonSession`.
//! Under `cfg(test)` its home is its spool directory and the run is
//! unattended, so it never touches the real `~/.aria` or plays a sound. It
//! runs on demand, so it loads no model until a press and exits soon after
//! it goes idle.

use std::path::Path;

use crate::config::WakeWordOverride;
use crate::dictation::DictationExecutor;
use crate::runtime::{PreparedRuntimePaths, RuntimePaths};

use super::assembly;
use super::host::{DaemonLifetime, SpawnedRuntime, spawn_server};
use super::server::Server;
use super::session::DaemonSession;
use super::shutdown::ShutdownSignal;

pub(in crate::daemon) struct HostedDaemon {
    _directory: tempfile::TempDir,
    paths: PreparedRuntimePaths,
    runtime: SpawnedRuntime,
}

impl HostedDaemon {
    /// The real assembly, reading `config(home)` as its `settings.toml`.
    pub(in crate::daemon) fn configured(config: impl FnOnce(&Path) -> String) -> Self {
        Self::serve(|paths| {
            let home = paths.spool_dir();
            std::fs::create_dir_all(home.join(".aria")).expect("the settings directory is created");
            std::fs::write(crate::settings::default_path(home), config(home))
                .expect("the settings are written");
            assembly::build(paths, DaemonLifetime::OnDemand, WakeWordOverride::Inherit)
        })
    }

    /// Presses run `executor` in place of capture, a model, and delivery.
    pub(in crate::daemon) fn scripted(executor: impl DictationExecutor) -> Self {
        Self::serve(|paths| assembly::build_scripted(paths, Box::new(executor)))
    }

    fn serve(
        build: impl FnOnce(&PreparedRuntimePaths) -> Result<Server, super::DaemonError>,
    ) -> Self {
        let directory = tempfile::tempdir().expect("the runtime directory is created");
        let paths = RuntimePaths::in_directory(directory.path().to_path_buf())
            .prepare()
            .expect("the runtime directory is usable");
        let server = build(&paths).expect("the daemon builds");
        // `new`, not `install`, so the test process keeps its own signal handlers.
        let shutdown = ShutdownSignal::new().expect("the shutdown pipe opens");
        let runtime = spawn_server(server, shutdown).expect("the daemon spawns");
        Self {
            _directory: directory,
            paths,
            runtime,
        }
    }

    pub(in crate::daemon) fn session(&self) -> DaemonSession {
        DaemonSession::connect_at(&self.paths).expect("a session connects")
    }

    pub(in crate::daemon) fn settings_path(&self) -> std::path::PathBuf {
        crate::settings::default_path(self.paths.spool_dir())
    }

    pub(in crate::daemon) fn socket_path(&self) -> &Path {
        self.paths.socket_path()
    }

    pub(in crate::daemon) fn stop(self) {
        self.runtime.stop().expect("the daemon stops cleanly");
    }
}
