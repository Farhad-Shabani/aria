//! Aria's dictation pipeline, callable from a browser page.
//!
//! JavaScript owns the microphone and the network; this owns the audio and
//! the model. A page pushes 16 kHz mono frames in and takes a transcript out,
//! and nothing it pushes leaves the machine.

mod silence;

#[cfg(target_family = "wasm")]
mod dictation;

#[cfg(target_family = "wasm")]
pub use dictation::Dictation;
