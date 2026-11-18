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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn conditions_record_the_effective_replay_and_previews() {
        let agreement = Decoding::agreement(Cadence::from_millis(500, 20_000));
        for (decoding, realtime, expected) in [
            (
                agreement,
                false,
                json!({
                    "decoding": {
                        "mode": "agreement", "stride_samples": 8000, "max_buffer_samples": 320000
                    },
                    "replay": {"pace": "stepped", "stride_samples": 8000},
                    "previews": true
                }),
            ),
            (
                agreement,
                true,
                json!({
                    "decoding": {
                        "mode": "agreement", "stride_samples": 8000, "max_buffer_samples": 320000
                    },
                    "replay": {"pace": "realtime"},
                    "previews": true
                }),
            ),
            (
                Decoding::Whole,
                true,
                json!({
                    "decoding": {"mode": "whole"},
                    "replay": {"pace": "immediate"},
                    "previews": false
                }),
            ),
            (
                Decoding::ChunkedFile {
                    chunk_samples: 4000,
                },
                true,
                json!({
                    "decoding": {"mode": "chunked_file", "chunk_samples": 4000},
                    "replay": {"pace": "immediate"},
                    "previews": false
                }),
            ),
            (
                Decoding::Native {
                    chunk_samples: 12800,
                },
                false,
                json!({
                    "decoding": {"mode": "native", "chunk_samples": 12800},
                    "replay": {"pace": "stepped", "stride_samples": 12800},
                    "previews": false
                }),
            ),
            (
                Decoding::Native {
                    chunk_samples: 12800,
                },
                true,
                json!({
                    "decoding": {"mode": "native", "chunk_samples": 12800},
                    "replay": {"pace": "realtime"},
                    "previews": false
                }),
            ),
        ] {
            let conditions = Conditions::new(decoding, realtime, true);

            assert_eq!(serde_json::to_value(conditions).unwrap(), expected);
        }
    }
}
