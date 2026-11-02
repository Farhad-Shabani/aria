//! What the on-demand idle exit watches: requests in flight, and how long
//! since the last one finished.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tonic::codegen::http;

/// A streaming response counts only until its stream is handed out, so open
/// event subscriptions do not keep an on-demand daemon alive. A handler
/// whose stream carries real work takes its guard out of the request and
/// holds it as long as the work runs.
#[derive(Clone)]
pub(super) struct Activity {
    state: Arc<Mutex<ActivityState>>,
}

struct ActivityState {
    active: usize,
    last_finished: Instant,
}

impl Activity {
    pub(super) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ActivityState {
                active: 0,
                last_finished: Instant::now(),
            })),
        }
    }

    fn begin(&self) -> ActivityGuard {
        lock_state(&self.state).active += 1;
        ActivityGuard {
            activity: self.clone(),
        }
    }

    pub(super) fn active_count(&self) -> usize {
        lock_state(&self.state).active
    }

    pub(super) fn is_idle_for(&self, window: Duration) -> bool {
        let state = lock_state(&self.state);
        state.active == 0 && state.last_finished.elapsed() >= window
    }
}

/// One request in flight, until this drops.
pub(in crate::daemon) struct ActivityGuard {
    activity: Activity,
}

impl ActivityGuard {
    /// The guard the layer left on this request, for a handler that keeps
    /// working after it has answered. `None` when the request was not
    /// counted, or on a server without the layer.
    pub(in crate::daemon) fn take<T>(request: &mut tonic::Request<T>) -> Option<Arc<Self>> {
        request.extensions_mut().remove::<Arc<Self>>()
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let mut state = lock_state(&self.activity.state);
        state.last_finished = Instant::now();
        state.active -= 1;
    }
}

fn lock_state(state: &Mutex<ActivityState>) -> MutexGuard<'_, ActivityState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Counts every Aria request against [`Activity`], whatever service it hits.
#[derive(Clone)]
pub(super) struct TrackActivityLayer {
    pub(super) activity: Activity,
}

/// Health polls and reflection queries say nothing about whether the daemon is
/// doing work. Counting them would let any orchestrator or monitoring script
/// polling faster than the idle timeout keep an on-demand daemon alive forever.
fn counts_as_activity(path: &str) -> bool {
    !path.starts_with("/grpc.health.") && !path.starts_with("/grpc.reflection.")
}

impl<S> tower::Layer<S> for TrackActivityLayer {
    type Service = TrackActivity<S>;

    fn layer(&self, inner: S) -> Self::Service {
        TrackActivity {
            inner,
            activity: self.activity.clone(),
        }
    }
}

#[derive(Clone)]
pub(super) struct TrackActivity<S> {
    inner: S,
    activity: Activity,
}

impl<S, B> tower::Service<http::Request<B>> for TrackActivity<S>
where
    S: tower::Service<http::Request<B>>,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        let guard =
            counts_as_activity(request.uri().path()).then(|| Arc::new(self.activity.begin()));
        // Shared with the handler, which may keep it past its answer.
        if let Some(guard) = &guard {
            request.extensions_mut().insert(Arc::clone(guard));
        }
        let future = self.inner.call(request);
        Box::pin(async move {
            let result = future.await;
            drop(guard);
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_is_idle_only_after_the_last_request_ends() {
        let activity = Activity::new();

        let guard = activity.begin();
        assert!(
            !activity.is_idle_for(Duration::ZERO),
            "an active request is not idle"
        );

        drop(guard);
        assert_eq!(activity.active_count(), 0);
        assert!(!activity.is_idle_for(Duration::from_secs(60)));
        assert!(activity.is_idle_for(Duration::ZERO));
    }

    #[test]
    fn a_handler_that_keeps_its_guard_keeps_the_daemon_busy() {
        let activity = Activity::new();
        let layer = Arc::new(activity.begin());
        let mut request = tonic::Request::new(());
        request.extensions_mut().insert(Arc::clone(&layer));

        let kept = ActivityGuard::take(&mut request).expect("the layer left a guard");
        drop(layer);
        assert_eq!(
            activity.active_count(),
            1,
            "the handler's copy still counts"
        );

        drop(kept);
        assert_eq!(activity.active_count(), 0);
    }

    #[test]
    fn only_aria_traffic_keeps_an_on_demand_daemon_alive() {
        assert!(counts_as_activity("/aria.daemon.v1.DictationService/Start"));
        assert!(!counts_as_activity("/grpc.health.v1.Health/Check"));
        assert!(!counts_as_activity("/grpc.health.v1.Health/Watch"));
        assert!(!counts_as_activity(
            "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo"
        ));
        assert!(!counts_as_activity(
            "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo"
        ));
    }
}
