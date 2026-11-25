//! The decoder on a thread of its own, posting each update to the board as
//! it comes so the caller's thread can decide on it whenever anything moves.
//!
//! When a walk hears the recording, the decoder skips what comes before
//! speech: a recognizer handed audio that holds no speech names words in it.

use std::sync::Arc;
use std::thread::{Scope, ScopedJoinHandle};
use std::time::Duration;

use aria_kit::audio::{RecordingReader, canonical_samples};
use aria_kit::transcription::next_heard_update;

use super::board::{Board, Decoded, Ending};
use crate::stt::{Operation, Recognizer, Transcript, TranscriptionError};

/// How much silence the decoder lets go of at a time while it waits for
/// speech, so a long quiet start is not kept.
const SILENCE_HELD: u64 = canonical_samples(Duration::from_secs(1));

pub(super) struct DecodeThread<'scope> {
    board: Arc<Board>,
    worker: Option<ScopedJoinHandle<'scope, Result<Transcript, TranscriptionError>>>,
}

impl<'scope> DecodeThread<'scope> {
    /// Decodes `recording` to its end. Every update goes on `board`, and the
    /// decoder's end follows the last, however the decoder ends.
    ///
    /// With `lead_in`, a walk hears the recording, and the decoder starts
    /// that many samples before the first speech. Without it, the decoder
    /// starts at the recording's start.
    pub(super) fn spawn<'env, T: Recognizer>(
        scope: &'scope Scope<'scope, 'env>,
        mut operation: Operation<'scope, T>,
        recording: &'scope mut RecordingReader,
        previews: bool,
        lead_in: Option<u64>,
        board: Arc<Board>,
    ) -> std::io::Result<Self> {
        let worker = std::thread::Builder::new()
            .name("aria-hearing-decoder".into())
            .spawn_scoped(scope, {
                let board = Arc::clone(&board);
                // A new thread starts outside the press's span.
                let press = tracing::Span::current();
                move || {
                    let _press = press.entered();
                    let _done = Done(&board);
                    decode(&mut operation, recording, previews, lead_in, &board)
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

fn decode<T: Recognizer>(
    operation: &mut Operation<'_, T>,
    recording: &mut RecordingReader,
    previews: bool,
    lead_in: Option<u64>,
    board: &Board,
) -> Result<Transcript, TranscriptionError> {
    if let Some(lead_in) = lead_in
        && !skip_before_speech(operation, recording, lead_in, board)?
    {
        tracing::info!("no speech detected in the recording; nothing is decoded");
        return Err(TranscriptionError::NoSpeech);
    }
    while let Some(update) = next_heard_update(operation, recording, previews)? {
        let decoded = Decoded {
            update,
            read_to: operation.read_to(),
            retained_from: operation.retained_from(),
        };
        if board.push_decoded(decoded).is_err() {
            // Nobody will read another update.
            return Err(TranscriptionError::Cancelled);
        }
    }
    operation.finish(recording)
}

/// Skips the audio up to `lead_in` samples before the first speech the
/// walk hears, waiting for it. `false` when the recording ended without any.
///
/// A map that could not be judged counts as speech, so the decoder starts
/// on what it still holds. An operation that reads the recording from its
/// start decodes all of it.
fn skip_before_speech<T: Recognizer>(
    operation: &mut Operation<'_, T>,
    recording: &mut RecordingReader,
    lead_in: u64,
    board: &Board,
) -> Result<bool, TranscriptionError> {
    loop {
        // Read before the snapshot: a walk that had ended left its last one.
        let ending = board.walk_ending();
        let speech = board.snapshot().speech;
        if speech.is_broken() {
            return Ok(true);
        }
        let speech_from = speech.speech_from();
        let silent_to = speech.reached();
        let start = speech_from.unwrap_or(silent_to).saturating_sub(lead_in);
        if !operation.skip_before(recording, start)? || speech_from.is_some() {
            return Ok(true);
        }
        match ending {
            Some(Ending::Heard) => {
                // The walk heard a failed capture as its end. This reader
                // owns the failure, so it reads to the end to report it.
                recording.fill().map_err(TranscriptionError::Capture)?;
                return Ok(false);
            }
            Some(Ending::Stopped) => return Err(TranscriptionError::Cancelled),
            // A walk that panicked left a broken map, which returned above.
            Some(Ending::Panicked) | None => {}
        }
        board
            .wait_walk(|snapshot| {
                let speech = &snapshot.speech;
                speech.is_broken()
                    || speech.speech_from().is_some()
                    || speech.reached() >= silent_to + SILENCE_HELD
            })
            .map_err(|_| TranscriptionError::Cancelled)?;
    }
}

#[cfg(test)]
mod tests {
    use aria_kit::audio::Recording;
    use aria_kit::transcription::{Cadence, Hypothesis};
    use aria_kit::voice_activity::SpeechDetector;

    use super::*;
    use crate::dictation::testing::{Hypotheses, Loud};
    use crate::hearing::turns::Snapshot;
    use crate::stt::{Language, Transcription};

    /// How `skip_before_speech` answers once a walk that heard no speech in
    /// `recording` has ended as `ending`.
    fn skipped_after(
        mut recording: RecordingReader,
        ending: Ending,
    ) -> Result<bool, TranscriptionError> {
        let board = Board::new();
        board.start_walk(
            recording.waker(),
            Snapshot {
                speech: Arc::new(SpeechDetector::new(Loud, 0).map().clone()),
                timeline: None,
            },
        );
        board.end_walk(ending);
        let transcription = Transcription::agreement(
            Cadence::from_millis(100, 20_000),
            Hypotheses(|_: &[f32]| Ok(Hypothesis::new("hello"))),
        );
        let mut operation = transcription.start(&Language::Automatic).unwrap();

        skip_before_speech(&mut operation, &mut recording, 16, &board)
    }

    #[test]
    fn a_walk_stopped_before_any_speech_ends_the_decoder_cancelled() {
        let recording = RecordingReader::from_samples(&[0.0; 16_000]);

        let skipped = skipped_after(recording, Ending::Stopped);

        assert!(matches!(skipped, Err(TranscriptionError::Cancelled)));
    }

    #[test]
    fn a_capture_that_fails_before_any_speech_is_not_taken_for_silence() {
        let (recording, mut writer) = Recording::open();
        writer.push(&[0.0; 1_000]);
        // Dropped unfinished, as a capture thread that vanished.
        drop(writer);

        let skipped = skipped_after(recording, Ending::Heard);

        assert!(matches!(skipped, Err(TranscriptionError::Capture(_))));
    }
}
