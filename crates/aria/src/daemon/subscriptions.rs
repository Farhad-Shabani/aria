//! Bounded event streams for daemon subscribers.
//!
//! A full queue drops level events; other events close the subscription.
//! Disconnected subscribers are removed.

use std::sync::Mutex;

use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;

use crate::ipc::wire;
use crate::rpc::RpcErrorCode;

/// Each `aria ask` waiting on its reply holds one, beside the app and
/// `aria events`.
const MAX_SUBSCRIBERS: usize = 64;
/// Room for a full dictation's stages while the peer catches up.
pub(in crate::daemon) const EVENT_BUFFER: usize = 16;

pub(super) type EventStream = ReceiverStream<Result<wire::Event, Status>>;

/// What a subscriber that cannot take an event right now loses by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backlog {
    EndsTheSubscription,
    CostsAnEvent,
}

pub(super) struct Subscriptions {
    registry: Mutex<Registry>,
}

struct Registry {
    senders: Vec<mpsc::Sender<Result<wire::Event, Status>>>,
    /// Set by `close_all`; a closed registry admits nobody, so a `Subscribe`
    /// racing a shutdown cannot register a stream the drain would wait on.
    closed: bool,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SubscribeError {
    #[error("subscriber capacity is exhausted")]
    Exhausted,

    #[error("the daemon is stopping")]
    ShuttingDown,
}

impl From<&SubscribeError> for RpcErrorCode {
    fn from(refusal: &SubscribeError) -> Self {
        match refusal {
            SubscribeError::Exhausted => Self::Busy,
            SubscribeError::ShuttingDown => Self::ShuttingDown,
        }
    }
}

impl Subscriptions {
    pub(super) fn new() -> Self {
        Self {
            registry: Mutex::new(Registry {
                senders: Vec::new(),
                closed: false,
            }),
        }
    }

    /// Registers before returning, so no event can slip past a stream the
    /// caller is about to hand out.
    pub(super) fn subscribe(&self) -> Result<EventStream, SubscribeError> {
        let mut registry = self.lock_registry();
        if registry.closed {
            return Err(SubscribeError::ShuttingDown);
        }
        registry.senders.retain(|sender| !sender.is_closed());
        if registry.senders.len() >= MAX_SUBSCRIBERS {
            return Err(SubscribeError::Exhausted);
        }
        let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
        registry.senders.push(sender);
        Ok(ReceiverStream::new(receiver))
    }

    pub(super) fn broadcast(&self, stage: impl Into<wire::Event>) {
        self.send(stage.into(), Backlog::EndsTheSubscription);
    }

    /// Drops the event when the queue is full, keeping the subscription open.
    pub(super) fn broadcast_lossy(&self, stage: impl Into<wire::Event>) {
        self.send(stage.into(), Backlog::CostsAnEvent);
    }

    fn send(&self, event: wire::Event, backlog: Backlog) {
        self.lock_registry()
            .senders
            .retain(|sender| match sender.try_send(Ok(event.clone())) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if backlog == Backlog::CostsAnEvent {
                        return true;
                    }
                    tracing::debug!("dropping a subscriber that stopped taking events");
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            });
    }

    /// Ends every stream and refuses new ones, so a shutdown drain never
    /// waits on a subscriber.
    pub(super) fn close_all(&self) {
        let mut registry = self.lock_registry();
        registry.closed = true;
        registry.senders.clear();
    }

    fn lock_registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use tokio_stream::StreamExt;

    use crate::dictation::{DictationStage, RecordingId};

    use super::*;

    fn admitted(subscriptions: &Subscriptions, count: usize) -> Vec<EventStream> {
        (0..count)
            .map(|_subscriber| {
                subscriptions
                    .subscribe()
                    .expect("the subscription should be admitted")
            })
            .collect()
    }

    #[tokio::test]
    async fn a_vanished_subscriber_no_longer_holds_a_slot() {
        let subscriptions = Subscriptions::new();
        let streams = admitted(&subscriptions, MAX_SUBSCRIBERS);
        drop(streams);

        // No broadcast in between: admission alone must reap the dead.
        subscriptions
            .subscribe()
            .expect("a slot should be free once its holder vanished");
    }

    #[tokio::test]
    async fn a_ninth_subscriber_is_refused() {
        let subscriptions = Subscriptions::new();
        let _streams = admitted(&subscriptions, MAX_SUBSCRIBERS);

        assert!(subscriptions.subscribe().is_err());
    }

    #[tokio::test]
    async fn a_subscriber_that_stops_taking_events_is_evicted() {
        let subscriptions = Subscriptions::new();
        let _wedged = subscriptions
            .subscribe()
            .expect("the subscription should be admitted");

        for _event in 0..=EVENT_BUFFER {
            subscriptions.broadcast(DictationStage::Recording(RecordingId(1)));
        }

        // Its slot is free again even though the stream was never dropped.
        let _streams: Vec<_> = (0..MAX_SUBSCRIBERS)
            .map(|_subscriber| {
                subscriptions
                    .subscribe()
                    .expect("the full-buffer eviction should have freed the slot")
            })
            .collect();
    }

    #[tokio::test]
    async fn a_broadcast_reaches_every_subscriber() {
        let subscriptions = Subscriptions::new();
        let mut streams = admitted(&subscriptions, 2);

        subscriptions.broadcast(DictationStage::Delivered {
            outcome: None,
            words: 0,
        });
        subscriptions.close_all();

        for stream in &mut streams {
            let event = stream
                .next()
                .await
                .expect("the event should arrive before the stream ends")
                .expect("the event should not be a status");
            assert_eq!(
                event,
                wire::Event::from(DictationStage::Delivered {
                    outcome: None,
                    words: 0
                })
            );
            assert!(
                stream.next().await.is_none(),
                "close_all should end the stream"
            );
        }
    }
}
