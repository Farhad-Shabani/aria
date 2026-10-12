use serde::{Deserialize, Serialize};

use super::Cadence;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(super) enum Decoding {
    Whole,
    Agreement {
        stride_samples: usize,
        max_buffer_samples: usize,
    },
    Native {
        chunk_samples: usize,
    },
    ChunkedFile {
        chunk_samples: usize,
    },
}

impl Decoding {
    pub(super) fn agreement(cadence: Cadence) -> Self {
        Self::Agreement {
            stride_samples: cadence.stride_samples(),
            max_buffer_samples: cadence.max_buffer_samples(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "pace", rename_all = "snake_case")]
pub(super) enum Replay {
    Immediate,
    Stepped { stride_samples: usize },
    Realtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Conditions {
    pub(super) decoding: Decoding,
    pub(super) replay: Replay,
    pub(super) previews: bool,
}

impl Conditions {
    pub(super) fn new(decoding: Decoding, realtime: bool, previews: bool) -> Self {
        let replay = match decoding {
            Decoding::Whole | Decoding::ChunkedFile { .. } => Replay::Immediate,
            Decoding::Agreement { stride_samples, .. }
            | Decoding::Native {
                chunk_samples: stride_samples,
            } => {
                if realtime {
                    Replay::Realtime
                } else {
                    Replay::Stepped { stride_samples }
                }
            }
        };
        Self {
            decoding,
            replay,
            previews: previews && matches!(decoding, Decoding::Agreement { .. }),
        }
    }
}
