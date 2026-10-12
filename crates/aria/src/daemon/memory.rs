//! The consolidation backlog in a persistent daemon: once at start, for
//! whatever the last daemon left, then after each lull in the owner's
//! asks. Extraction runs on the blocking pool and stops with the daemon.

use std::time::Duration;

use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::tasks::Supervised;
use crate::config::Environment;
use crate::memory::run_backlog;
use crate::primitives::error_chain;

/// How long the asks have to go quiet before their turns are extracted.
const QUIET: Duration = Duration::from_secs(120);

/// One nudge per recorded turn; more than one waiting says nothing new.
pub(super) fn wake_channel() -> (mpsc::Sender<()>, mpsc::Receiver<()>) {
    mpsc::channel(1)
}

pub(super) struct MemoryBacklog {
    environment: Environment,
    wake: mpsc::Receiver<()>,
    vocabulary: std::sync::Arc<super::vocabulary::Runtime>,
}

impl MemoryBacklog {
    pub(super) fn new(
        environment: Environment,
        wake: mpsc::Receiver<()>,
        vocabulary: std::sync::Arc<super::vocabulary::Runtime>,
    ) -> Self {
        Self {
            environment,
            wake,
            vocabulary,
        }
    }

    async fn consolidate(&self, shutdown: &CancellationToken) {
        let shutdown = shutdown.clone();
        let cancel = CancelToken::from_fn(move || shutdown.is_cancelled());
        let environment = self.environment.clone();
        match tokio::task::spawn_blocking(move || run_backlog(&environment, &cancel)).await {
            Ok(Ok(_)) => self.vocabulary.prepare(None),
            Ok(Err(error)) => {
                tracing::warn!(error = %error_chain(&error), "the memory backlog did not run");
            }
            Err(error) => tracing::warn!(%error, "the memory backlog did not finish"),
        }
    }

    /// Waits for a nudge, then for the asks to stay quiet.
    async fn lull(&mut self, shutdown: &CancellationToken) -> bool {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return false,
            nudged = self.wake.recv() => if nudged.is_none() { return false },
        }
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return false,
                () = tokio::time::sleep(QUIET) => return true,
                nudged = self.wake.recv() => if nudged.is_none() { return true },
            }
        }
    }
}

impl Supervised for MemoryBacklog {
    async fn run(mut self, shutdown: CancellationToken) {
        self.consolidate(&shutdown).await;
        while self.lull(&shutdown).await {
            self.consolidate(&shutdown).await;
        }
    }
}
