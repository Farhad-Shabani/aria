//! What a listen session announces while it runs.

use std::sync::Arc;

use crate::event::EventEmitter;
use crate::rpc::RpcErrorCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListenStage {
    SegmentTranscribed {
        text: Arc<str>,
        /// The tentative live speaker label; the transcript's final rewrite
        /// may settle on another.
        label: Option<Arc<str>>,
        /// Seconds into the recording, so a follower's clock reads the same
        /// as the transcript file's whenever it attached.
        at_seconds: u64,
    },
    /// A voice turned out to be one already heard. Everything `from` was
    /// named is now `into`, in the file and from here on.
    SpeakerMerged { from: Arc<str>, into: Arc<str> },
    /// The session ended as asked.
    Stopped,
    /// The session ended on its own, and why.
    Failed(RpcErrorCode),
}

pub(crate) type ListenEventEmitter = EventEmitter<ListenStage>;

pub(crate) const EVENT_THREAD: &str = "aria-listen-events";
