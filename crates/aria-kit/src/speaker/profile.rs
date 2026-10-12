//! What an enrolled voice is: a name, a role, and the vector a
//! [`Diarizer`](super::Diarizer) claims that name from.
//!
//! Reading and writing them is [`store`](super::store)'s business; nothing
//! here touches a filesystem, so it compiles for the browser too.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Long enough for a name, short enough to stay a filename.
const MAX_NAME_BYTES: usize = 64;

/// What an enrolled voice is allowed to be. Only an owner's voice answers
/// "was that you?", so a guest can be recognized by name without turning on
/// dictation meant for the person who set the machine up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Guest,
}

impl std::fmt::Display for Role {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Owner => "owner",
            Self::Guest => "guest",
        })
    }
}

/// An enrolled person's name, which is also their filename. Holding one is
/// proof it cannot escape the speakers directory.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct SpeakerName(String);

impl SpeakerName {
    pub fn new(name: impl Into<String>) -> Result<Self, SpeakerNameError> {
        let name = name.into();
        if name.is_empty() {
            return Err(SpeakerNameError::Empty);
        }
        if name.len() > MAX_NAME_BYTES {
            return Err(SpeakerNameError::TooLong {
                actual_bytes: name.len(),
                maximum_bytes: MAX_NAME_BYTES,
            });
        }
        if name.trim() != name {
            return Err(SpeakerNameError::Padded);
        }
        // A separator would make the name a path, and a leading dot would
        // hide the profile from anyone looking at the directory.
        if name.starts_with('.')
            || name
                .chars()
                .any(|character| character.is_control() || matches!(character, '/' | '\\' | ':'))
        {
            return Err(SpeakerNameError::Unusable);
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SpeakerName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One enrolled voice.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: SpeakerName,
    pub role: Role,
    /// The engine that measured `vector`, by its `Engine::NAME`.
    pub engine: String,
    pub enrolled_at: SystemTime,
    /// Unit norm, so a cosine against it is a plain dot product.
    pub vector: Vec<f32>,
}

/// A profile carrying one voice. The engine and the moment it was recorded
/// do not change what a diarizer decides, so they take fixed values.
#[cfg(any(test, feature = "test-support"))]
pub fn enrolled_voice(name: &str, role: Role, vector: Vec<f32>) -> Profile {
    Profile {
        name: SpeakerName::new(name).expect("a test name is usable"),
        role,
        engine: "test".to_owned(),
        enrolled_at: SystemTime::UNIX_EPOCH,
        vector,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpeakerNameError {
    #[error("a speaker name cannot be empty")]
    Empty,

    #[error("a speaker name holds {actual_bytes} bytes; maximum is {maximum_bytes}")]
    TooLong {
        actual_bytes: usize,
        maximum_bytes: usize,
    },

    #[error("a speaker name cannot start or end with a space")]
    Padded,

    #[error("a speaker name cannot start with a dot or hold a path separator")]
    Unusable,
}
