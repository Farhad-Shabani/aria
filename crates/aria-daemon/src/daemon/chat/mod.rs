//! Daemon ownership of chat: the runtime and the supervised actor that
//! schedules it.

mod actor;
mod runtime;

pub(in crate::daemon) use actor::{ChatActor, ChatCallError, ChatHandle, channel};
pub(in crate::daemon) use runtime::ChatRuntime;
