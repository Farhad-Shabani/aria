pub use aria_kit::synthesis::prelude;
pub use aria_kit::synthesis::prelude::*;

#[cfg(all(feature = "kokoro", not(target_family = "wasm")))]
pub mod kokoro;
#[cfg(all(feature = "qwen", not(target_family = "wasm")))]
pub mod qwen;
#[cfg(all(feature = "system", any(target_os = "macos", target_os = "linux")))]
pub mod system;
