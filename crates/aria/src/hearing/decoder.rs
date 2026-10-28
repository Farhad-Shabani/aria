//! The decoder on a thread of its own, posting each update to the board as
//! it comes so the caller's thread can decide on it whenever anything moves.

use std::sync::Arc;
use std::thread::{Scope, ScopedJoinHandle};

use aria_kit::audio::RecordingReader;
use aria_kit::transcription::{HeardUpdate, next_heard, next_heard_update};

use super::board::{Board, Decoded};
use crate::transcription::{Operation, Transcriber, Transcript, TranscriptionError};

pub(super) struct DecodeThread<'scope> {
    board: Arc<Board>,
    worker: Option<ScopedJoinHandle<'scope, Result<Transcript, TranscriptionError>>>,
}

impl<'scope> DecodeThread<'scope> {
    /// Decodes `recording` to its end. Every update goes on `board`, and the
    /// decoder's end follows the last, however the decoder ends.
    pub(super) fn spawn<'env, T: Transcriber>(
        scope: &'scope Scope<'scope, 'env>,
        mut operation: Operation<'scope, T>,
        recording: &'scope mut RecordingReader,
        previews: bool,
        board: Arc<Board>,
    ) -> std::io::Result<Self> {
        let worker = std::thread::Builder::new()
            .name("aria-hearing-decoder".into())
            .spawn_scoped(scope, {
                let board = Arc::clone(&board);
                move || {
                    let _done = Done(&board);
                    decode(&mut operation, recording, previews, &board)
                }
            })?;
        Ok(Self {
            board,
            worker: Some(worker),
        })
    }

    /// Joins the decoder. `None` when it panicked.
    pub(super) fn finish(mut self) -> Option<Result<Transcript, TranscriptionError>> {
        let worker = self.worker.take().expect("the decoder is joined only once");
        worker.join().ok()
    }
}

/// A decoder dropped unfinished is stopped, then joined.
impl Drop for DecodeThread<'_> {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.board.abandon();
            if worker.join().is_err() {
                tracing::error!("the decoder panicked");
            }
        }
    }
}

/// Marks the decoder done however it ends, a panic included.
struct Done<'a>(&'a Board);

impl Drop for Done<'_> {
    fn drop(&mut self) {
        self.0.decoder_done();
    }
}

fn decode<T: Transcriber>(
    operation: &mut Operation<'_, T>,
    recording: &mut RecordingReader,
    previews: bool,
    board: &Board,
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
        let decoded = Decoded {
            update,
            read_to: operation.read_to(),
        };
        if board.push_decoded(decoded).is_err() {
            // Nobody will read another update.
            return Err(TranscriptionError::Cancelled);
        }
    }
    operation.finish(recording)
}
