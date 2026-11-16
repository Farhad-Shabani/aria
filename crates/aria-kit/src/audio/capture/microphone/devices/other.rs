use super::Transport;
use crate::audio::InputDeviceId;

// Continuity exists only on macOS, and no caller here needs a built-in
// microphone told apart.
pub(super) fn transport(_device: &InputDeviceId) -> Transport {
    Transport::Other
}
