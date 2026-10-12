//! What every engine backend shares, whichever capability it serves: the
//! boxed shape of its own failures, and validation of the model files it
//! loads. Each capability wraps these in its own error vocabulary.

#[cfg(not(target_family = "wasm"))]
mod model_path;

use std::path::PathBuf;

#[cfg(not(target_family = "wasm"))]
pub use model_path::{validate_model_directory, validate_model_file};

// The engine's own error stays boxed rather than named. Which engine failed is
// already on the capability error, and naming the types here would put every
// engine crate's version in Aria's public API.
#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error(transparent)]
    Engine(Box<dyn std::error::Error + Send + Sync>),

    #[error("the backend answered, but the answer is unusable: {0}")]
    Unusable(String),
}

impl BackendError {
    pub fn engine(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Engine(Box::new(error))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[cfg(not(target_family = "wasm"))]
    #[error(transparent)]
    Path(#[from] crate::primitives::file_inspection::InspectionError),

    #[error("failed to read the model directory {}", path.display())]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("model directory {} is missing {file}", directory.display())]
    Missing {
        directory: PathBuf,
        file: &'static str,
    },

    #[error("failed to read the model file {}", path.display())]
    Unreadable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the model file {} is malformed", path.display())]
    Malformed {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}
