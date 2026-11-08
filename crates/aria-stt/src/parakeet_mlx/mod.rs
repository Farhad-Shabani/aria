//! Transcription through a supervised parakeet-mlx child process.
//!
//! The model runs as an MLX child process on macOS; recordings reach it
//! as WAV files.
//!
//! ```no_run
//! use aria_stt::{parakeet_mlx::ParakeetMlx, prelude::*};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let transcription = Transcription::complete_wav(ParakeetMlx::load("interpreter", "model")?)
//!     .with(RemoveHesitations)
//!     .with(Vocabulary::new(["aria-stt", "Cargo.toml"])?);
//! let transcript: Transcript = transcription.transcribe_file("recording.wav")?;
//! # Ok(())
//! # }
//! ```
//!
//! The engine decodes one finished recording per request. With
//! [`ParakeetMlx::stream_chunks`] the sidecar feeds that recording through
//! the model's native streaming path in capture-sized chunks, reproducing
//! what a live microphone at that cadence would have produced.

mod engine;
mod protocol;

pub use engine::{ParakeetMlx, ParakeetMlxConfig};

/// Names this engine under `stt.backend` and in its own errors.
pub const NAME: &str = "parakeet-mlx";
