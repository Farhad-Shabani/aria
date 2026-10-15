pub use aria_kit::{audio, synthesis};
pub use aria_primitives as primitives;

#[cfg(target_os = "macos")]
pub mod accessibility;
pub mod chat;
pub mod config;
pub mod daemon;
pub(crate) mod delivery;
pub mod dictation;
pub mod doctor;
pub mod enrollment;
pub(crate) mod event;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) mod feedback;
pub mod history;
pub(crate) mod hotkey;
pub(crate) mod ipc;
pub mod listen;
pub mod memory;
pub(crate) mod microphone;
pub mod preferences;
pub mod rpc;
pub mod runtime;
pub mod setup;
pub mod speaker;
pub mod speech;
pub mod supervisor;
pub mod transcription;
pub mod vocabulary;
