//! Local speech-to-text engines behind `aria-kit`'s transcription contract.
//!
//! Each engine is a module behind a Cargo feature, and none is on by default:
//! `moonshine`, `nemotron`, `parakeet`, `parakeet-mlx`, `whisper`, and
//! `whisper-candle`. The `microphone` feature adds live capture.
//! `EngineConfig` picks among the enabled engines at run time, on native
//! targets.

#![cfg_attr(docsrs, feature(doc_cfg))]

// Every engine config names model files on disk, which a browser lacks.
#[cfg(all(
    not(target_family = "wasm"),
    any(
        feature = "moonshine",
        feature = "nemotron",
        feature = "parakeet",
        all(feature = "parakeet-mlx", not(target_os = "ios")),
        feature = "whisper",
        feature = "whisper-candle"
    )
))]
mod engine;

pub use aria_kit::transcription::prelude;
pub use aria_kit::transcription::prelude::*;
#[cfg(all(
    not(target_family = "wasm"),
    any(
        feature = "moonshine",
        feature = "nemotron",
        feature = "parakeet",
        all(feature = "parakeet-mlx", not(target_os = "ios")),
        feature = "whisper",
        feature = "whisper-candle"
    )
))]
pub use engine::EngineConfig;

#[cfg(all(feature = "moonshine", not(target_family = "wasm")))]
pub mod moonshine;
#[cfg(all(feature = "nemotron", not(target_family = "wasm")))]
pub mod nemotron;
#[cfg(all(feature = "parakeet", not(target_family = "wasm")))]
pub mod parakeet;
#[cfg(all(
    feature = "parakeet-mlx",
    not(target_family = "wasm"),
    not(target_os = "ios")
))]
pub mod parakeet_mlx;
#[cfg(all(feature = "whisper", not(target_family = "wasm")))]
pub mod whisper;
#[cfg(feature = "whisper-candle")]
pub mod whisper_candle;
