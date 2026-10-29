//! The ground truth written beside each own-voice corpus WAV: who speaks
//! each turn and where its audio sits.

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Truth {
    pub turns: Vec<Placed>,
    /// Only on a corpus that mixes an owner and another voice over each
    /// other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overlap_probe: Option<OverlapProbe>,
}

/// One turn's audio in its scenario, in seconds.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct Placed {
    pub owner: bool,
    pub text: String,
    pub start: f64,
    pub end: f64,
}

/// The folder in each owner's corpus that holds the owner's profile.
pub const SPEAKERS_DIR: &str = "speakers";

#[derive(Debug, Serialize, Deserialize)]
pub struct OverlapProbe {
    pub artifact_kind: ArtifactKind,
    pub controls: OverlapControls,
}

/// What one WAV of an overlap probe holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// The owner and another voice over each other.
    Mix,
    /// The mix's owner turns alone, on the mix's clock.
    OwnerControl,
    /// The mix's other voice alone, on the mix's clock.
    GuestControl,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OverlapControls {
    pub owner_control: String,
}
