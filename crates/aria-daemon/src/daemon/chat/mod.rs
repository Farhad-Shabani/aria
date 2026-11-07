//! Daemon ownership of chat: the runtime and the supervised actor that
//! schedules it.

mod actor;
mod error;
mod runtime;

pub(in crate::daemon) use actor::{ChatActor, ChatCallError, ChatHandle, channel};
pub(in crate::daemon) use error::ChatError;
pub(in crate::daemon) use runtime::ChatRuntime;
#[cfg(test)]
pub(in crate::daemon) use runtime::tests::{Log, logging_assistants, runtime_with};
