//! The decoder on a thread of its own, sending each update as it comes so
//! the caller's thread can decide on it whenever anything moves.

use std::sync::mpsc::SyncSender;
use std::thread::{Scope, ScopedJoinHandle};

use aria_kit::audio::RecordingReader;
use aria_kit::transcription::{HeardUpdate, next_heard, next_heard_update};

use super::Wake;
use crate::transcription::{Operation, Transcriber, Transcript, TranscriptionError};

pub(super) struct DecodeThread<'scope> {
    worker: Option<ScopedJoinHandle<'scope, Result<Transcript, TranscriptionError>>>,
}

impl<'scope> DecodeThread<'scope> {
    /// Decodes `recording` to its end. Every update goes to `wake`, and
    /// [`Wake::DecoderDone`] follows the last, however the decoder ends.
    pub(super) fn spawn<'env, T: Transcriber>(
        scope: &'scope Scope<'scope, 'env>,
        mut operation: Operation<'scope, T>,
        recording: &'scope mut RecordingReader,
        previews: bool,
        wake: SyncSender<Wake>,
    ) -> std::io::Result<Self> {
        let worker = std::thread::Builder::new()
            .name("aria-hearing-decoder".into())
            .spawn_scoped(scope, move || {
                let done = Done(wake);
                decode(&mut operation, recording, previews, &done.0)
            })?;
        Ok(Self {
            worker: Some(worker),
        })
    }

    /// Joins the decoder. `None` when it panicked.
    pub(super) fn finish(mut self) -> Option<Result<Transcript, TranscriptionError>> {
        let worker = self.worker.take().expect("the decoder is joined only once");
        worker.join().ok()
    }
}

/// A decoder dropped unfinished is joined here; the hearing's cancel token
/// has already told it to stop.
impl Drop for DecodeThread<'_> {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("the decoder panicked");
        }
    }
}

/// Sends [`Wake::DecoderDone`] when the decoder ends, a panic included:
/// the turn thread holds a sender too, so the channel alone never closes.
struct Done(SyncSender<Wake>);

impl Drop for Done {
    fn drop(&mut self) {
        let _ = self.0.send(Wake::DecoderDone);
    }
}

fn decode<T: Transcriber>(
    operation: &mut Operation<'_, T>,
    recording: &mut RecordingReader,
    previews: bool,
    wake: &SyncSender<Wake>,
) -> Result<Transcript, TranscriptionError> {
    loop {
        let update = if previews {
            next_heard_update(operation, recording)?
        } else {
            next_heard(operation, recording)?.map(HeardUpdate::Finalized)
        };
        let Some(update) = update else {
            break;
        };
        let decoded = Wake::Decoded {
            update,
            read_to: operation.read_to(),
        };
        if wake.send(decoded).is_err() {
            // Nobody will read another update.
            return Err(TranscriptionError::Cancelled);
        }
    }
    operation.finish(recording)
}
