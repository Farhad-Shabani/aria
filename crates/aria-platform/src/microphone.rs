//! What kind of microphone an input device is, as the OS reports it.

use aria_kit::audio::InputDeviceId;

/// Whether `device` is an iPhone's microphone, which the Mac uses through
/// Continuity. False for a device the OS can't find.
pub fn is_continuity_iphone(device: &InputDeviceId) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::is_continuity_iphone(device)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = device;
        false
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use aria_kit::audio::InputDeviceId;
    use cidre::{cf, core_audio as ca};

    pub(super) fn is_continuity_iphone(device: &InputDeviceId) -> bool {
        let Some(uid) = device.core_audio_uid() else {
            return false;
        };
        let Ok(device) = ca::Device::with_uid(&cf::String::from_str(uid)) else {
            return false;
        };
        if device.is_unknown() {
            return false;
        }
        device.transport_type().is_ok_and(|transport| {
            transport == ca::DeviceTransportType::CONTINUITY_CAPTURE_WIRED
                || transport == ca::DeviceTransportType::CONTINUITY_CAPTURE_WIRELESS
        })
    }
}
