use std::error::Error;
use std::path::PathBuf;

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

    for proto in SETUP_PROTOS {
        println!("cargo:rerun-if-changed={proto}");
    }

    let setup = protox::compile(SETUP_PROTOS, ["../../proto"])?;
    tonic_prost_build::configure().compile_fds(setup)?;
    Ok(())
}
