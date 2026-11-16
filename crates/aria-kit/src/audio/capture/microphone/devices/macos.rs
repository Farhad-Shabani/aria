use cidre::{cf, core_audio as ca};

use super::Transport;
use crate::audio::InputDeviceId;

/// `Other` for a device CoreAudio can't find.
pub(super) fn transport(device: &InputDeviceId) -> Transport {
    let Some(uid) = device.core_audio_uid() else {
        return Transport::Other;
    };
    let Ok(device) = ca::Device::with_uid(&cf::String::from_str(uid)) else {
        return Transport::Other;
    };
    if device.is_unknown() {
        return Transport::Other;
    }
    match device.transport_type() {
        Ok(ca::DeviceTransportType::CONTINUITY_CAPTURE_WIRED)
        | Ok(ca::DeviceTransportType::CONTINUITY_CAPTURE_WIRELESS) => Transport::ContinuityIphone,
        Ok(ca::DeviceTransportType::BUILT_IN) => Transport::BuiltIn,
        _ => Transport::Other,
    }
}
