//! Daemon ownership of speech: runtime composition and the supervised actor
//! that schedules it.

mod actor;
mod assembly;
mod runtime;

pub(in crate::daemon) use actor::{SpeechActor, SpeechHandle, channel};
pub(in crate::daemon) use runtime::SpeechRuntime;
#[cfg(test)]
pub(in crate::daemon) use runtime::next_utterance;
