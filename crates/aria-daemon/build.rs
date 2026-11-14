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
    "../../proto/aria/daemon/v1/microphone.proto",
    "../../proto/aria/daemon/v1/relay.proto",
    "../../proto/aria/daemon/v1/settings.proto",
    "../../proto/aria/daemon/v1/speaker.proto",
    "../../proto/aria/daemon/v1/speech.proto",
    "../../proto/aria/daemon/v1/transcription.proto",
    "../../proto/aria/daemon/v1/vocabulary.proto",
];
fn main() -> Result<(), Box<dyn Error>> {
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={proto}");
    }

    let descriptors = protox::compile(PROTOS, ["../../proto"])?;
    // The server embeds this set to answer reflection queries.
    let descriptor_path =
        PathBuf::from(std::env::var("OUT_DIR")?).join("aria_daemon_descriptor.bin");
    std::fs::write(&descriptor_path, descriptors.encode_to_vec())?;
    tonic_prost_build::configure().compile_fds(descriptors)?;
    Ok(())
}
