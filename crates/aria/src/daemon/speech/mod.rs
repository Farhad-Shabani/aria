//! Daemon ownership of speech: runtime composition and the supervised actor
//! that schedules it.

mod actor;
mod assembly;
mod runtime;

pub(in crate::daemon) use actor::{SpeechActor, SpeechCallError, SpeechHandle, channel};
pub use assembly::SpeechInitializationError;
pub(in crate::daemon) use runtime::{SpeechRuntime, SpeechRuntimeError};
