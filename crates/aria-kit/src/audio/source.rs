//! What a recording captures from.

use std::fmt;

/// An input device's stable identity, in cpal's `host:device` form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InputDeviceId(String);

impl InputDeviceId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The device's Core Audio UID, for a device cpal found through Core
    /// Audio.
    pub fn core_audio_uid(&self) -> Option<&str> {
        self.0.strip_prefix("coreaudio:")
    }
}

impl fmt::Display for InputDeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What a live recording captures from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingSource {
    /// An input device on this machine.
    Device(InputDeviceId),
    /// Samples a caller pushes in as they arrive, such as a remote
    /// microphone's.
    Pushed,
    /// Audio recorded before: a file played back, or samples handed over
    /// whole.
    Prerecorded,
}

#[cfg(test)]
mod tests {
    use super::InputDeviceId;

    #[test]
    fn only_a_core_audio_device_has_a_core_audio_uid() {
        assert_eq!(
            InputDeviceId::new("coreaudio:C5ADCAFA").core_audio_uid(),
            Some("C5ADCAFA")
        );
        assert_eq!(InputDeviceId::new("alsa:default").core_audio_uid(), None);
    }
}
