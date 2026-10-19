//! Foundational capabilities shared by Aria subsystems.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod cancel;
#[cfg(not(target_family = "wasm"))]
pub mod file_inspection;
pub mod lazy;
#[cfg(not(target_family = "wasm"))]
pub mod local_socket;
#[cfg(not(target_family = "wasm"))]
pub mod private_document;
#[cfg(not(target_family = "wasm"))]
pub mod private_fs;
// iOS forbids spawning a child process, so nothing built on one exists there.
#[cfg(not(any(target_os = "ios", target_family = "wasm")))]
pub mod process;
pub mod slot;
pub mod user_home;
#[cfg(not(target_family = "wasm"))]
pub mod wakeup;

/// An error and every cause under it, joined into one line. A cause the
/// message already ends with is not repeated.
pub fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut cause = error.source();
    while let Some(error) = cause {
        let text = error.to_string();
        if !message.ends_with(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        cause = error.source();
    }
    message
}
