//! Daemon ownership of dictation: runtime composition and the supervised
//! actor that schedules it.

mod actor;
mod assembly;
mod model_load;
mod runtime;

pub(in crate::daemon) use actor::{DictationActor, DictationCallError, DictationHandle, channel};
pub use assembly::DictationInitializationError;
pub(in crate::daemon) use runtime::{
    AutoStopRule, DictationRuntime, DictationRuntimeError, DictationSetup,
};
