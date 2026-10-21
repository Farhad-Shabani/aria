//! Runtime choice among this crate's engines.

use std::path::PathBuf;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::{
    AudioInputKind, Engine, Stream, StreamingEngine, Transcriber, TranscriptionError,
};

/// One engine's configuration, for a caller that picks the engine at run time.
///
/// A variant exists only while its engine's feature is on. Another crate in
/// the build can turn on more features, so a match needs a wildcard arm.
///
/// ```
/// use aria_kit::transcription::{Streaming, Transcriber, Transcription};
/// use aria_stt::{CancelToken, EngineConfig, TranscriptionError};
///
/// fn live(
///     config: EngineConfig,
/// ) -> Result<Option<Transcription<Box<dyn Transcriber>, Streaming>>, TranscriptionError> {
///     let Some(stream) = config.stream() else {
///         return Ok(None);
///     };
///     config.check()?;
///     let transcriber = config.load(&CancelToken::never())?;
///     Ok(Some(Transcription::stream(stream, transcriber)))
/// }
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub enum EngineConfig {
    #[cfg(feature = "moonshine")]
    Moonshine(crate::moonshine::MoonshineConfig),
    #[cfg(feature = "nemotron")]
    Nemotron(crate::nemotron::NemotronConfig),
    #[cfg(feature = "parakeet")]
    Parakeet(crate::parakeet::ParakeetConfig),
    #[cfg(all(feature = "parakeet-mlx", not(target_os = "ios")))]
    ParakeetMlx(crate::parakeet_mlx::ParakeetMlxConfig),
    #[cfg(feature = "whisper")]
    Whisper(crate::whisper::WhisperConfig),
    #[cfg(feature = "whisper-candle")]
    WhisperCandle(crate::whisper_candle::WhisperCandleConfig),
}

impl EngineConfig {
    pub fn name(&self) -> &'static str {
        match self {
            #[cfg(feature = "moonshine")]
            Self::Moonshine(_) => crate::moonshine::NAME,
            #[cfg(feature = "nemotron")]
            Self::Nemotron(_) => crate::nemotron::NAME,
            #[cfg(feature = "parakeet")]
            Self::Parakeet(_) => crate::parakeet::NAME,
            #[cfg(all(feature = "parakeet-mlx", not(target_os = "ios")))]
            Self::ParakeetMlx(_) => crate::parakeet_mlx::NAME,
            #[cfg(feature = "whisper")]
            Self::Whisper(_) => crate::whisper::NAME,
            #[cfg(feature = "whisper-candle")]
            Self::WhisperCandle(_) => crate::whisper_candle::NAME,
        }
    }

    /// The engine's [`Engine::INPUT`].
    pub fn input(&self) -> AudioInputKind {
        match self {
            #[cfg(feature = "moonshine")]
            Self::Moonshine(_) => crate::moonshine::Moonshine::INPUT,
            #[cfg(feature = "nemotron")]
            Self::Nemotron(_) => crate::nemotron::Nemotron::INPUT,
            #[cfg(feature = "parakeet")]
            Self::Parakeet(_) => crate::parakeet::Parakeet::INPUT,
            #[cfg(all(feature = "parakeet-mlx", not(target_os = "ios")))]
            Self::ParakeetMlx(_) => crate::parakeet_mlx::ParakeetMlx::INPUT,
            #[cfg(feature = "whisper")]
            Self::Whisper(_) => crate::whisper::Whisper::INPUT,
            #[cfg(feature = "whisper-candle")]
            Self::WhisperCandle(_) => crate::whisper_candle::WhisperCandle::INPUT,
        }
    }

    /// The engine's [`StreamingEngine::STREAM`], or `None` for an engine that
    /// is not a [`StreamingEngine`].
    pub fn stream(&self) -> Option<Stream> {
        match self {
            #[cfg(feature = "moonshine")]
            Self::Moonshine(_) => Some(crate::moonshine::Moonshine::STREAM),
            #[cfg(feature = "nemotron")]
            Self::Nemotron(_) => Some(crate::nemotron::Nemotron::STREAM),
            #[cfg(feature = "parakeet")]
            Self::Parakeet(_) => Some(crate::parakeet::Parakeet::STREAM),
            #[cfg(all(feature = "parakeet-mlx", not(target_os = "ios")))]
            Self::ParakeetMlx(_) => None,
            #[cfg(feature = "whisper")]
            Self::Whisper(_) => Some(crate::whisper::Whisper::STREAM),
            #[cfg(feature = "whisper-candle")]
            Self::WhisperCandle(_) => None,
        }
    }

    /// Resolves what [`load`](Self::load) would read, without reading it or
    /// starting a child.
    pub fn check(&self) -> Result<PathBuf, TranscriptionError> {
        match self {
            #[cfg(feature = "moonshine")]
            Self::Moonshine(config) => crate::moonshine::Moonshine::check_from(config),
            #[cfg(feature = "nemotron")]
            Self::Nemotron(config) => crate::nemotron::Nemotron::check_from(config),
            #[cfg(feature = "parakeet")]
            Self::Parakeet(config) => crate::parakeet::Parakeet::check_from(config),
            #[cfg(all(feature = "parakeet-mlx", not(target_os = "ios")))]
            Self::ParakeetMlx(config) => crate::parakeet_mlx::ParakeetMlx::check_from(config),
            #[cfg(feature = "whisper")]
            Self::Whisper(config) => crate::whisper::Whisper::check_from(config),
            #[cfg(feature = "whisper-candle")]
            Self::WhisperCandle(config) => crate::whisper_candle::WhisperCandle::check_from(config),
        }
    }

    pub fn load(self, cancel: &CancelToken) -> Result<Box<dyn Transcriber>, TranscriptionError> {
        fn boxed(engine: impl Transcriber + 'static) -> Box<dyn Transcriber> {
            Box::new(engine)
        }

        match self {
            #[cfg(feature = "moonshine")]
            Self::Moonshine(config) => {
                crate::moonshine::Moonshine::load_from(config, cancel).map(boxed)
            }
            #[cfg(feature = "nemotron")]
            Self::Nemotron(config) => {
                crate::nemotron::Nemotron::load_from(config, cancel).map(boxed)
            }
            #[cfg(feature = "parakeet")]
            Self::Parakeet(config) => {
                crate::parakeet::Parakeet::load_from(config, cancel).map(boxed)
            }
            #[cfg(all(feature = "parakeet-mlx", not(target_os = "ios")))]
            Self::ParakeetMlx(config) => {
                crate::parakeet_mlx::ParakeetMlx::load_from(config, cancel).map(boxed)
            }
            #[cfg(feature = "whisper")]
            Self::Whisper(config) => crate::whisper::Whisper::load_from(config, cancel).map(boxed),
            #[cfg(feature = "whisper-candle")]
            Self::WhisperCandle(config) => {
                crate::whisper_candle::WhisperCandle::load_from(config, cancel).map(boxed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn configs(missing: &Path) -> Vec<EngineConfig> {
        vec![
            #[cfg(feature = "moonshine")]
            EngineConfig::Moonshine(crate::moonshine::MoonshineConfig::new(missing)),
            #[cfg(feature = "nemotron")]
            EngineConfig::Nemotron(crate::nemotron::NemotronConfig::new(missing)),
            #[cfg(feature = "parakeet")]
            EngineConfig::Parakeet(crate::parakeet::ParakeetConfig::new(
                missing,
                crate::parakeet::Device::Cpu,
            )),
            #[cfg(all(feature = "parakeet-mlx", not(target_os = "ios")))]
            EngineConfig::ParakeetMlx(crate::parakeet_mlx::ParakeetMlxConfig {
                interpreter: missing.to_path_buf(),
                model: "unused".to_owned(),
            }),
            #[cfg(feature = "whisper")]
            EngineConfig::Whisper(crate::whisper::WhisperConfig::new(missing)),
            #[cfg(feature = "whisper-candle")]
            EngineConfig::WhisperCandle(crate::whisper_candle::WhisperCandleConfig::new(missing)),
        ]
    }

    fn names_engine(error: &TranscriptionError, name: &str) -> bool {
        matches!(
            error,
            TranscriptionError::Model { backend, .. } | TranscriptionError::Load { backend, .. }
                if *backend == name
        )
    }

    #[test]
    fn each_config_checks_and_loads_through_its_own_engine() {
        let directory = tempfile::tempdir().expect("test directory should be created");
        let missing = directory.path().join("missing");

        for config in configs(&missing) {
            let name = config.name();
            let checked = config.check().expect_err("a missing model fails its check");
            let Err(loaded) = config.load(&CancelToken::never()) else {
                panic!("{name} loaded a missing model");
            };

            assert!(names_engine(&checked, name), "{name} check: {checked}");
            assert!(names_engine(&loaded, name), "{name} load: {loaded}");
        }
    }
}
