//! Which microphone a press records on: the one rule the capture, the
//! microphone list, and the phone stream all read.

use aria_kit::audio::{InputDevice, InputDeviceId, Inputs, is_connected, resolve_input};
use aria_settings::Microphone;

/// Where one recording runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    Mac,
    Phone,
}

/// The one rule for which microphone records. `connected` asks whether a
/// chosen Mac device is connected; it runs only when the answer matters.
pub(super) fn target(
    preference: &Microphone,
    inputs: Inputs,
    attached: bool,
    connected: impl FnOnce(&InputDeviceId) -> bool,
) -> Target {
    if inputs == Inputs::Local || !attached {
        return Target::Mac;
    }
    match preference {
        Microphone::Automatic => Target::Phone,
        Microphone::SystemDefault => Target::Mac,
        Microphone::Device(_) => match input(preference) {
            Some(id) if connected(&id) => Target::Mac,
            _ => Target::Phone,
        },
    }
}

/// Whether a press would record on an attached phone now, by the rule a
/// recording uses. Lists the audio devices when a Mac device is chosen.
pub(in crate::daemon) fn admits_phone(preference: &Microphone) -> bool {
    target(preference, Inputs::Any, true, is_connected) == Target::Phone
}

/// For a capture that never records on a phone, such as a WAV replay.
pub(in crate::daemon) fn admits_no_phone(_preference: &Microphone) -> bool {
    false
}

/// What a press records on, by the rule a recording uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CurrentMicrophone {
    Device(InputDeviceId),
    Phone,
}

/// What a press records on now, among `devices`. `None` when nothing can
/// record.
pub(in crate::daemon) fn current(
    preference: &Microphone,
    attached: bool,
    devices: &[InputDevice],
) -> Option<CurrentMicrophone> {
    let connected = |id: &InputDeviceId| devices.iter().any(|device| device.id == *id);
    match target(preference, Inputs::Any, attached, connected) {
        Target::Phone => Some(CurrentMicrophone::Phone),
        Target::Mac => resolve_input(input(preference).as_ref(), devices)
            .map(|device| CurrentMicrophone::Device(device.id.clone())),
    }
}

/// The device `dictation.microphone` names, or `None` for the system default.
pub(super) fn input(microphone: &Microphone) -> Option<InputDeviceId> {
    match microphone {
        Microphone::Automatic | Microphone::SystemDefault => None,
        Microphone::Device(id) => Some(InputDeviceId::new(id.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, Ordering};

    fn device(id: &str) -> Microphone {
        id.parse().expect("a device id")
    }

    #[test]
    fn the_preference_and_the_caller_pick_the_microphone() {
        let connected = |_: &InputDeviceId| true;
        let missing = |_: &InputDeviceId| false;
        let usb = device("coreaudio:usb");
        let cases = [
            (Microphone::Automatic, Inputs::Any, true, Target::Phone),
            (Microphone::Automatic, Inputs::Any, false, Target::Mac),
            (Microphone::SystemDefault, Inputs::Any, true, Target::Mac),
            (Microphone::Automatic, Inputs::Local, true, Target::Mac),
        ];
        for (preference, inputs, attached, expected) in cases {
            assert_eq!(
                target(&preference, inputs, attached, connected),
                expected,
                "{preference:?} {inputs:?} attached={attached}"
            );
        }
        assert_eq!(target(&usb, Inputs::Any, true, connected), Target::Mac);
        assert_eq!(target(&usb, Inputs::Any, true, missing), Target::Phone);
        assert_eq!(target(&usb, Inputs::Local, true, missing), Target::Mac);
    }

    #[test]
    fn device_presence_is_asked_only_when_it_decides() {
        let asked = AtomicBool::new(false);
        let ask = |_: &InputDeviceId| {
            asked.store(true, Ordering::SeqCst);
            true
        };

        target(&device("coreaudio:usb"), Inputs::Any, false, ask);

        assert!(
            !asked.load(Ordering::SeqCst),
            "no phone, so the Mac records"
        );
    }

    #[test]
    fn the_current_microphone_follows_the_recording_rule() {
        let builtin = InputDevice {
            id: InputDeviceId::new("coreaudio:builtin"),
            name: "MacBook Pro Microphone".to_owned(),
            system_default: true,
        };
        let usb = InputDevice {
            id: InputDeviceId::new("coreaudio:usb"),
            name: "USB Microphone".to_owned(),
            system_default: false,
        };
        let devices = [builtin.clone(), usb.clone()];
        let on = |device: &InputDevice| Some(CurrentMicrophone::Device(device.id.clone()));
        let cases = [
            (
                Microphone::Automatic,
                true,
                &devices[..],
                Some(CurrentMicrophone::Phone),
            ),
            (Microphone::Automatic, false, &devices[..], on(&builtin)),
            (Microphone::SystemDefault, true, &devices[..], on(&builtin)),
            (device("coreaudio:usb"), true, &devices[..], on(&usb)),
            (
                device("coreaudio:usb"),
                true,
                &devices[..1],
                Some(CurrentMicrophone::Phone),
            ),
            (device("coreaudio:usb"), false, &devices[..1], on(&builtin)),
            (Microphone::Automatic, false, &[][..], None),
        ];
        for (preference, attached, devices, expected) in cases {
            assert_eq!(
                current(&preference, attached, devices),
                expected,
                "{preference:?} attached={attached} devices={}",
                devices.len()
            );
        }
    }
}
