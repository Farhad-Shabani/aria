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
