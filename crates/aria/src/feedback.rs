//! Sounds for a person at the machine.
//!
//! macOS dictation cues play in-process from clips read at startup. A player
//! process per cue would pay a CoreAudio client setup each time, which stalls
//! under load. Everything else goes through one player per platform:
//! `afplay` with the system sounds on macOS, `paplay` with the freedesktop
//! theme on Linux. A desktop without the player just logs.

use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
use aria_kit::cancel::CancelToken;
#[cfg(target_os = "macos")]
use aria_kit::synthesis::{PlaybackError, SpeakerOutput, StreamClosed, stream};

#[cfg(target_os = "macos")]
use crate::audio::Clip;

#[cfg(target_os = "macos")]
mod caf;
mod detached;

#[cfg(target_os = "macos")]
pub(crate) use caf::read_caf;
pub(crate) use detached::run_detached;

/// Plays the clip on the default output device from its own thread. The
/// player waits a drain grace after the last sample, so the thread outlives
/// the sound by over half a second.
#[cfg(target_os = "macos")]
pub(crate) fn play_clip(purpose: &'static str, clip: Arc<Clip>) {
    detached::spawn_detached(purpose, move || {
        if let Err(error) = play_clip_now(&clip) {
            tracing::warn!(%error, purpose, "the sound did not play");
        }
    });
}

#[cfg(target_os = "macos")]
#[derive(Debug, thiserror::Error)]
enum ClipPlaybackError {
    #[error(transparent)]
    Stream(#[from] StreamClosed),
    #[error(transparent)]
    Playback(#[from] PlaybackError),
}

#[cfg(target_os = "macos")]
fn play_clip_now(clip: &Clip) -> Result<(), ClipPlaybackError> {
    // The stream buffers seconds of audio, so a cue fits in one write.
    let (mut writer, speech) = stream(clip.sample_rate());
    writer.write(clip.samples())?;
    writer.finish();
    SpeakerOutput::new()?.play_with(speech, &CancelToken::new(), || {})?;
    Ok(())
}

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
