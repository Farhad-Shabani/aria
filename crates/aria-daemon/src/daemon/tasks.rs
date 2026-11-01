//! Cancellation and completion ownership for daemon background tasks.

use std::future::Future;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// A long-lived task the daemon owns for its whole life: it runs until the
/// shutdown token fires, then drains whatever it started.
pub(super) trait Supervised: Send + 'static {
    fn run(self, shutdown: CancellationToken) -> impl Future<Output = ()> + Send;
}

pub(super) struct RuntimeTasks {
    shutdown: CancellationToken,
    tracker: TaskTracker,
}

impl RuntimeTasks {
    pub(super) fn new() -> Self {
        Self {
            shutdown: CancellationToken::new(),
            tracker: TaskTracker::new(),
        }
    }

    pub(super) fn cancellation(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Tracks the task and hands it the shutdown token, so waiting on this
    /// tracker waits for it.
    pub(super) fn supervise(&self, task: impl Supervised) {
        self.tracker.spawn(task.run(self.shutdown.clone()));
    }

    pub(super) fn request_stop(&self) {
        self.shutdown.cancel();
        self.tracker.close();
    }

    pub(super) async fn wait(&self) {
        self.tracker.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    struct WaitForShutdown {
        finished: Arc<AtomicBool>,
    }

    impl Supervised for WaitForShutdown {
        async fn run(self, shutdown: CancellationToken) {
            shutdown.cancelled().await;
            self.finished.store(true, Ordering::Release);
        }
    }

    #[tokio::test]
    async fn stop_cancels_and_waits_for_tracked_tasks() {
        let tasks = RuntimeTasks::new();
        let finished = Arc::new(AtomicBool::new(false));
        tasks.supervise(WaitForShutdown {
            finished: Arc::clone(&finished),
        });

        tasks.request_stop();
        tokio::time::timeout(Duration::from_secs(1), tasks.wait())
            .await
            .expect("tracked tasks should stop within the deadline");

        assert!(finished.load(Ordering::Acquire));
    }
}
