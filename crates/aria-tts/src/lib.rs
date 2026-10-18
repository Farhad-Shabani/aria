//! Local text-to-speech engines behind `aria-kit`'s synthesis contract.
//!
//! Each engine is a module behind a Cargo feature, and none is on by default:
//! `kokoro`, `qwen`, and `system` for the platform's own voice. The root
//! re-exports the synthesis prelude.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub use aria_kit::synthesis::prelude;
pub use aria_kit::synthesis::prelude::*;

#[cfg(all(feature = "kokoro", not(target_family = "wasm")))]
pub mod kokoro;
#[cfg(all(feature = "qwen", not(target_family = "wasm")))]
pub mod qwen;
#[cfg(all(feature = "system", any(target_os = "macos", target_os = "linux")))]
pub mod system;
