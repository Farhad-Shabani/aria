use std::error::Error;
use std::path::PathBuf;

use prost::Message;

const PROTOS: &[&str] = &[
    "../../proto/aria/daemon/v1/chat.proto",
    "../../proto/aria/daemon/v1/dictation.proto",
    "../../proto/aria/daemon/v1/errors.proto",
    "../../proto/aria/daemon/v1/events.proto",
    "../../proto/aria/daemon/v1/history.proto",
    "../../proto/aria/daemon/v1/inference.proto",
    "../../proto/aria/daemon/v1/listen.proto",
    "../../proto/aria/daemon/v1/settings.proto",
    "../../proto/aria/daemon/v1/relay.proto",
    "../../proto/aria/daemon/v1/speaker.proto",
    "../../proto/aria/daemon/v1/speech.proto",
    "../../proto/aria/daemon/v1/vocabulary.proto",
];
/// Spoken over stdio by `aria setup`, not by the daemon, so it stays out
/// of the reflection descriptor.
const SETUP_PROTOS: &[&str] = &["../../proto/aria/setup/v1/setup.proto"];

fn main() -> Result<(), Box<dyn Error>> {
    if std::env::var("CARGO_CFG_TARGET_OS")?.as_str() == "macos" {
        // Tap symbols are absent before 14.2; listen checks the OS before use.
        println!("cargo:rustc-link-arg=-Wl,-weak_framework,CoreAudio");
        let plist = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?).join("Info.plist");
        println!("cargo:rerun-if-changed={}", plist.display());
        println!(
            "cargo:rustc-link-arg-bin=aria=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }

    for proto in PROTOS.iter().chain(SETUP_PROTOS) {
        println!("cargo:rerun-if-changed={proto}");
    }

    let descriptors = protox::compile(PROTOS, ["../../proto"])?;
    // The server embeds this set to answer reflection queries.
    let descriptor_path =
        PathBuf::from(std::env::var("OUT_DIR")?).join("aria_daemon_descriptor.bin");
    std::fs::write(&descriptor_path, descriptors.encode_to_vec())?;
    tonic_prost_build::configure().compile_fds(descriptors)?;
    let setup = protox::compile(SETUP_PROTOS, ["../../proto"])?;
    tonic_prost_build::configure().compile_fds(setup)?;
    Ok(())
}
