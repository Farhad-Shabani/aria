//! Bounded reading and parsing of the operator-managed configuration file.

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{ConfigError, ConfigParseError};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// A section nobody set reads the same absent as one written as `{}`, so
/// the serializer leaves it out entirely.
fn is_default<T: Default + PartialEq>(section: &T) -> bool {
    *section == T::default()
}

pub(crate) fn read(path: &Path) -> Result<FileConfig, ConfigError> {
    let Some(contents) = read_text(path)? else {
        return Ok(FileConfig::default());
    };
    if contents.trim().is_empty() {
        return Ok(FileConfig::default());
    }

    serde_saphyr::from_str(&contents).map_err(|error| ConfigError::Parse {
        path: path.to_path_buf(),
        source: ConfigParseError::from_yaml(&error),
    })
}

fn read_text(path: &Path) -> Result<Option<String>, ConfigError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Open {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let metadata = file.metadata().map_err(|source| ConfigError::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(ConfigError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::FileTooLarge {
            maximum_bytes: MAX_CONFIG_BYTES,
        });
    }

    let capacity = usize::try_from(metadata.len()).map_err(|_error| ConfigError::FileTooLarge {
        maximum_bytes: MAX_CONFIG_BYTES,
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.by_ref()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(ConfigError::FileTooLarge {
            maximum_bytes: MAX_CONFIG_BYTES,
        });
    }

    String::from_utf8(bytes)
        .map(Some)
        .map_err(ConfigError::InvalidUtf8)
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileConfig {
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) capture: FileCapture,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) transcription: FileTranscription,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) listen: FileListen,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) speaker: FileSpeaker,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) wake: FileWake,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) hotkey: FileHotkey,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) phone: FilePhone,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) dictation_timeouts: FileTimeouts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dictation_cleanup: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dictation_backtrack: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dictation_cleanup_llm: Option<LlmCleanupConfig>,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) tts: FileTts,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) chat: FileChat,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) memory: FileMemory,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LlmCleanupConfig {
    pub(crate) endpoint: String,
    pub(crate) model: String,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileMemory {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) extract_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_facts_per_session: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dir: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileChat {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workdir: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) project_roots: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) timeout: Option<f64>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileSpeaker {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) segmentation_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) speakers_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileCapture {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pace: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileTranscription {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) interpreter: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileListen {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) transcripts_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileHotkey {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FilePhone {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) port: Option<u16>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileWake {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) threshold: Option<f32>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileTts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) voices: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tokenizer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) voice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fallback: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct FileTimeouts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recording: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) delivery: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) overall: Option<f64>,
}
