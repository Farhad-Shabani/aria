//! Wake-word detection with openWakeWord and livekit-wakeword models.
//!
//! Each detector is a module behind a Cargo feature, and none is on by
//! default: `openwakeword` and `livekit`. Both implement `aria-kit`'s
//! `WakeWordDetector`.

#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(all(
    any(feature = "livekit", feature = "openwakeword"),
    not(target_family = "wasm")
))]
mod frontend;
#[cfg(all(
    any(feature = "livekit", feature = "openwakeword"),
    not(target_family = "wasm")
))]
pub use frontend::{Config, ConfigError, Error, ModelFiles};

#[cfg(all(feature = "livekit", not(target_family = "wasm")))]
pub mod livekit;
#[cfg(all(feature = "openwakeword", not(target_family = "wasm")))]
pub mod openwakeword;
