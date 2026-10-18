//! Local speech-to-text engines behind `aria-kit`'s transcription contract.
//!
//! Each engine is a module behind a Cargo feature, and none is on by default:
//! `moonshine`, `nemotron`, `parakeet`, `parakeet-mlx`, `whisper`, and
//! `whisper-candle`. The `microphone` feature adds live capture. The root
//! re-exports the transcription prelude, so one import covers the common path.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub use aria_kit::transcription::prelude;
pub use aria_kit::transcription::prelude::*;

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
