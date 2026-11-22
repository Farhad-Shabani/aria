mod silence;

#[cfg(target_family = "wasm")]
mod dictation;

#[cfg(target_family = "wasm")]
pub use dictation::Dictation;
