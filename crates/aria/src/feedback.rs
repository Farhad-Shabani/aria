//! Sounds for a person at the machine.
//!
//! One player per platform: `afplay` with the system sounds on macOS,
//! `paplay` with the freedesktop theme on Linux. A desktop without the
//! player just logs.

use std::process::{Command, Stdio};

mod detached;

pub(crate) use detached::run_detached;

#[cfg(target_os = "macos")]
const PLAYER: &str = "/usr/bin/afplay";
#[cfg(target_os = "linux")]
const PLAYER: &str = "/usr/bin/paplay";

#[cfg(target_os = "macos")]
pub(crate) const SYSTEM_SOUNDS: &str = "/System/Library/Components/CoreAudio.component\
/Contents/SharedSupport/SystemSounds/system";
#[cfg(target_os = "linux")]
pub(crate) const SYSTEM_SOUNDS: &str = "/usr/share/sounds/freedesktop/stereo";

pub(crate) fn play(purpose: &'static str, sound_path: &str) {
    run_detached(purpose, player(sound_path));
}

fn player(sound_path: &str) -> Command {
    let mut command = Command::new(PLAYER);
    command
        .arg(sound_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Only macOS clears the environment: `paplay` needs `XDG_RUNTIME_DIR`
    // to reach its daemon.
    #[cfg(target_os = "macos")]
    command.env_clear();
    command
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn the_player_gets_the_sound_path_as_its_only_argument() {
        let command = player("/sounds/ping.caf");

        assert_eq!(command.get_program(), Path::new(PLAYER));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [Path::new("/sounds/ping.caf").as_os_str()]
        );
    }
}
