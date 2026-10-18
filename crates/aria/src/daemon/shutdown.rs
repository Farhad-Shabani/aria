//! Process-signal wakeup for graceful daemon shutdown.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use signal_hook::SigId;
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::primitives::wakeup::{Wakeup, WakeupError};

type Result<T> = std::result::Result<T, ShutdownError>;

/// How many installed `ShutdownSignal`s are alive in this process. The
/// lock also serializes every write to the disposition flag: a drop racing an
/// install must not leave a serving daemon with termination-by-default enabled.
static ACTIVE_SIGNALS: Mutex<usize> = Mutex::new(0);

/// When true, a termination signal runs its default action. signal-hook
/// never uninstalls its process-wide handler, so without this an embedder
/// that stopped its daemon would silently swallow SIGINT/SIGTERM forever.
static TERMINATE_BY_DEFAULT: OnceLock<Arc<AtomicBool>> = OnceLock::new();

/// Registers the process-lifetime fallback actions once. They run before the
/// pipe actions and do nothing while an installed `ShutdownSignal` is alive.
fn default_disposition_flag() -> Result<&'static Arc<AtomicBool>> {
    if let Some(flag) = TERMINATE_BY_DEFAULT.get() {
        return Ok(flag);
    }
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [SIGINT, SIGTERM] {
        signal_hook::flag::register_conditional_default(signal, Arc::clone(&flag))
            .map_err(|source| ShutdownError::RegisterSignal { signal, source })?;
    }
    // A racing second registration is harmless: the losing flag is never
    // stored, never set, and its actions stay inert forever.
    Ok(TERMINATE_BY_DEFAULT.get_or_init(|| flag))
}

/// Converts a stopper byte, and process termination signals once installed,
/// into normal file-descriptor readiness.
pub(super) struct ShutdownSignal {
    /// Declared first, so it drops before the pipe closes.
    process: Option<ProcessSignals>,
    wakeup: Wakeup,
}

impl ShutdownSignal {
    /// Wakes only on a stopper byte, and registers nothing for process
    /// signals.
    pub(super) fn new() -> Result<Self> {
        let wakeup = Wakeup::new().map_err(ShutdownError::Wakeup)?;
        Ok(Self {
            process: None,
            wakeup,
        })
    }

    /// Registers SIGINT and SIGTERM for this process. Their default
    /// disposition returns once every installed `ShutdownSignal` drops.
    pub(super) fn install() -> Result<Self> {
        let mut signal = Self::new()?;
        signal.process = Some(ProcessSignals::register(&signal.wakeup)?);
        Ok(signal)
    }

    /// An owned writer whose bytes read as a shutdown request.
    pub(super) fn stopper_writer(&self) -> Result<UnixStream> {
        self.wakeup.clone_writer().map_err(ShutdownError::Wakeup)
    }

    /// Completes once a signal or stopper byte arrives. Must run inside a
    /// tokio runtime.
    pub(super) async fn triggered(&mut self) -> Result<()> {
        let reader = self.wakeup.clone_reader().map_err(ShutdownError::Wakeup)?;
        let watcher = tokio::net::UnixStream::from_std(reader).map_err(ShutdownError::Watch)?;
        watcher.readable().await.map_err(ShutdownError::Watch)?;
        drop(watcher);
        self.wakeup.drain().map_err(ShutdownError::Wakeup)
    }
}

/// SIGINT and SIGTERM routed into a wakeup pipe.
struct ProcessSignals {
    registrations: [SigId; 2],
}

impl ProcessSignals {
    fn register(wakeup: &Wakeup) -> Result<Self> {
        let flag = default_disposition_flag()?;

        let interrupt = register_signal(SIGINT, clone_writer(wakeup)?)?;
        let terminate = match register_signal(SIGTERM, clone_writer(wakeup)?) {
            Ok(registration) => registration,
            Err(error) => {
                signal_hook::low_level::unregister(interrupt);
                return Err(error);
            }
        };

        {
            let mut active = lock_active_signals();
            *active += 1;
            flag.store(false, Ordering::SeqCst);
        }

        Ok(Self {
            registrations: [interrupt, terminate],
        })
    }
}

impl Drop for ProcessSignals {
    fn drop(&mut self) {
        for registration in self.registrations {
            signal_hook::low_level::unregister(registration);
        }
        let mut active = lock_active_signals();
        *active -= 1;
        if *active == 0
            && let Some(flag) = TERMINATE_BY_DEFAULT.get()
        {
            flag.store(true, Ordering::SeqCst);
        }
    }
}

fn lock_active_signals() -> std::sync::MutexGuard<'static, usize> {
    ACTIVE_SIGNALS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn clone_writer(wakeup: &Wakeup) -> Result<UnixStream> {
    wakeup.clone_writer().map_err(ShutdownError::Wakeup)
}

fn register_signal(signal: i32, writer: UnixStream) -> Result<SigId> {
    signal_hook::low_level::pipe::register(signal, writer)
        .map_err(|source| ShutdownError::RegisterSignal { signal, source })
}

#[derive(Debug, thiserror::Error)]
pub enum ShutdownError {
    #[error("the daemon shutdown wakeup failed")]
    Wakeup(#[source] WakeupError),

    #[error("failed to register daemon shutdown signal {signal}")]
    RegisterSignal {
        signal: i32,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to watch the daemon shutdown descriptor")]
    Watch(#[source] std::io::Error),
}
