//! What a capability announces.
//!
//! Events cross a channel, so nothing a listener does can slow an operation
//! down or fail one.

use std::sync::Arc;
use std::sync::mpsc::{TrySendError, sync_channel};
use std::thread;

/// How many events wait on a subscriber before the emitter starts dropping
/// them.
const EVENT_BUFFER: usize = 16;

#[derive(Clone)]
pub(crate) struct EventEmitter<T> {
    forward: Arc<dyn Fn(T) + Send + Sync>,
}

impl<T: Send + 'static> EventEmitter<T> {
    /// Hands the subscriber a thread of its own and returns the emitter that
    /// feeds it.
    pub(crate) fn subscribed_by(
        thread_name: &'static str,
        subscriber: impl Fn(T) + Send + 'static,
    ) -> Self {
        let (events, incoming) = sync_channel(EVENT_BUFFER);
        let spawned = thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                for event in incoming {
                    subscriber(event);
                }
            });
        match spawned {
            Ok(_thread) => Self::forwarded_by(move |event| match events.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    tracing::warn!("dropping an event the subscriber stopped taking");
                }
                Err(TrySendError::Disconnected(_)) => {
                    tracing::warn!("the event thread is gone");
                }
            }),
            Err(error) => {
                tracing::warn!(%error, thread_name, "failed to start the event thread");
                Self::unsubscribed()
            }
        }
    }

    /// Nobody is listening, so nothing is sent and no thread runs.
    pub(crate) fn unsubscribed() -> Self {
        Self::forwarded_by(|_| {})
    }

    // `forward` must only enqueue, such as into another emitter, so subscriber
    // work stays off the emitting thread.
    pub(crate) fn forwarded_by(forward: impl Fn(T) + Send + Sync + 'static) -> Self {
        Self {
            forward: Arc::new(forward),
        }
    }

    /// An event that cannot be delivered is dropped. Its contents are never
    /// logged: an event may carry what the user said.
    pub(crate) fn emit(&self, stage: impl Into<T>) {
        (self.forward)(stage.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn a_wedged_subscriber_never_holds_up_an_emit() {
        let (release, wait) = channel::<()>();
        let emitter = EventEmitter::subscribed_by("aria-test-events", move |_stage: ()| {
            let _ = wait.recv();
        });
        // On its own thread, so an emit that blocks fails the test instead
        // of hanging it.
        let (emitted, finished) = channel();
        std::thread::spawn(move || {
            for _event in 0..EVENT_BUFFER * 2 {
                emitter.emit(());
            }
            let _ = emitted.send(());
        });

        let outcome = finished.recv_timeout(Duration::from_secs(5));
        drop(release);
        assert!(outcome.is_ok(), "an emit waited on a wedged subscriber");
    }
}
