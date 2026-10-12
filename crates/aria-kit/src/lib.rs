//! Shared audio workflows and engine contracts for transcription, synthesis, and voice analysis.

pub mod audio;
pub mod backend;
pub use aria_primitives as primitives;
pub use aria_primitives::cancel;
pub mod segmentation;
pub mod speaker;
pub mod synthesis;
pub mod transcription;
#[cfg(not(target_family = "wasm"))]
pub mod voice_activity;
pub mod wake_word;
