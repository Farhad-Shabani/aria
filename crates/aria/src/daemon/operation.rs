//! The call boundary shared by the daemon's operation owners: cancellation,
//! the reply an actor owes its caller, and the wait on the other side.

use aria_kit::cancel::CancelToken;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// The operation stops when its caller cancels or the daemon shuts down.
pub(super) fn operation_cancellation(
    request: CancelToken,
    shutdown: CancellationToken,
) -> CancelToken {
    CancelToken::from_fn(move || request.is_cancelled() || shutdown.is_cancelled())
}

struct DisconnectGuard(CancelToken);

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// How one call into an actor can fail. The first two variants come from
/// the actor itself; only the last comes from the capability's own work.
#[derive(Debug, thiserror::Error)]
pub(super) enum CallError<E> {
    #[error("the actor is not accepting requests")]
    Unavailable,

    /// The job panicked. The actor stays operational; the operation's own
    /// resources were released by unwinding.
    #[error("the job failed unexpectedly")]
    JobFailed,

    #[error(transparent)]
    Runtime(#[from] E),
}

/// The answer an actor owes one caller. Every path out of it answers, so a
/// caller blocked on the response is never left waiting on a dropped sender.
pub(super) struct Reply<T, E>(oneshot::Sender<Result<T, CallError<E>>>);

impl<T, E> Reply<T, E>
where
    T: Send + 'static,
    E: Send + 'static,
{
    /// Runs the work on the blocking pool and answers with its result. The
    /// sender lives inside the job, so a caller blocked on the response is
    /// released when the job returns — shutdown included.
    pub(super) fn answer_with(
        self,
        jobs: &mut JoinSet<()>,
        job: &'static str,
        work: impl FnOnce() -> Result<T, E> + Send + 'static,
    ) {
        jobs.spawn_blocking(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .map(|result| result.map_err(CallError::Runtime))
                .unwrap_or_else(|panic| {
                    // Answered, not dropped: the caller must not read a panic
                    // as daemon shutdown.
                    tracing::error!(job, panic = panic_message(panic.as_ref()), "a job panicked");
                    Err(CallError::JobFailed)
                });
            // A caller that stopped waiting no longer wants the answer.
            let _ = self.0.send(outcome);
        });
    }

    /// Answers without running anything, for a call whose work goes on
    /// after the answer.
    pub(super) fn answer(self, value: T) {
        let _ = self.0.send(Ok(value));
    }

    /// The actor will never run this command.
    pub(super) fn unavailable(self) {
        let _ = self.0.send(Err(CallError::Unavailable));
    }
}

/// One submitted call from the caller's side: the reply slot, and for a
/// cancellable call the guard that ties the operation's lifetime to its own.
pub(super) struct Pending<T, E> {
    _cancel_on_drop: Option<DisconnectGuard>,
    response: oneshot::Receiver<Result<T, CallError<E>>>,
}

impl<T, E> Pending<T, E> {
    pub(super) async fn wait(self) -> Result<T, CallError<E>> {
        finish(self.response.await)
    }

    /// The same wait for a caller already on a blocking thread. Never call it
    /// from an async context.
    pub(super) fn wait_blocking(self) -> Result<T, CallError<E>> {
        finish(self.response.blocking_recv())
    }
}

/// Opens a call the caller can cancel, and builds the command that carries
/// it. The guard inside the [`Pending`] cancels the operation if the caller
/// goes away before the answer arrives.
pub(super) fn cancellable_call<T, E, C>(
    command: impl FnOnce(CancelToken, Reply<T, E>) -> C,
) -> (C, Pending<T, E>) {
    let cancellation = CancelToken::new();
    let (reply, response) = oneshot::channel();
    let pending = Pending {
        _cancel_on_drop: Some(DisconnectGuard(cancellation.clone())),
        response,
    };
    (command(cancellation, Reply(reply)), pending)
}

/// Opens a call with no cancellation, for work too short to interrupt.
pub(super) fn call<T, E, C>(command: impl FnOnce(Reply<T, E>) -> C) -> (C, Pending<T, E>) {
    let (reply, response) = oneshot::channel();
    let pending = Pending {
        _cancel_on_drop: None,
        response,
    };
    (command(Reply(reply)), pending)
}

fn finish<T, E>(
    outcome: Result<Result<T, CallError<E>>, oneshot::error::RecvError>,
) -> Result<T, CallError<E>> {
    match outcome {
        Ok(result) => result,
        // The actor dropped the reply without answering: it terminated.
        Err(_closed) => Err(CallError::Unavailable),
    }
}

/// Renders a panic payload for logging, without the payload's type noise.
pub(super) fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    panic
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("non-string panic payload")
}
