//! Which input device a recording uses: the chosen one while it's connected,
//! else the system default.

use std::fmt;

use cpal::traits::{DeviceTrait, HostTrait};

use super::{MicrophoneError, Result};

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

    fn of(device: &cpal::Device) -> Option<Self> {
        device.id().ok().map(|id| Self(id.to_string()))
    }
}

impl fmt::Display for InputDeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A connected device that can record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    pub id: InputDeviceId,
    pub name: String,
    pub system_default: bool,
}

/// Every connected input device, without opening any of them.
pub fn input_devices() -> Result<Vec<InputDevice>> {
    Ok(connected(&cpal::default_host())?
        .into_iter()
        .map(|(_, device)| device)
        .collect())
}

/// The device a recording uses: `wanted` while it's connected, else the
/// system default. `None` when nothing can record.
pub fn resolve_input<'devices>(
    wanted: Option<&InputDeviceId>,
    devices: &'devices [InputDevice],
) -> Option<&'devices InputDevice> {
    wanted
        .and_then(|wanted| devices.iter().find(|device| device.id == *wanted))
        .or_else(|| devices.iter().find(|device| device.system_default))
}

/// The cpal device [`resolve_input`] picks, and its id. Opens nothing.
pub(super) fn open_target(wanted: Option<&InputDeviceId>) -> Result<(cpal::Device, InputDeviceId)> {
    let mut inputs = connected(&cpal::default_host())?;
    let listed: Vec<InputDevice> = inputs.iter().map(|(_, device)| device.clone()).collect();
    let target = resolve_input(wanted, &listed)
        .map(|device| device.id.clone())
        .ok_or(MicrophoneError::NoInputDevice)?;
    let index = inputs
        .iter()
        .position(|(_, device)| device.id == target)
        .ok_or(MicrophoneError::NoInputDevice)?;
    let (device, listed) = inputs.swap_remove(index);
    Ok((device, listed.id))
}

/// Whether [`resolve_input`] would pick `id` now, by the same listing
/// `open_target` reads.
pub(super) fn is_connected(id: &InputDeviceId) -> bool {
    connected(&cpal::default_host())
        .is_ok_and(|inputs| inputs.iter().any(|(_, device)| device.id == *id))
}

pub(super) fn system_default() -> Option<InputDeviceId> {
    cpal::default_host()
        .default_input_device()
        .as_ref()
        .and_then(InputDeviceId::of)
}

// A device that vanishes mid-listing has no identity or name to read. It is
// gone, so it is left out rather than failing the list.
fn connected(host: &cpal::Host) -> Result<Vec<(cpal::Device, InputDevice)>> {
    let default = host
        .default_input_device()
        .as_ref()
        .and_then(InputDeviceId::of);
    let devices = host
        .input_devices()
        .map_err(|source| MicrophoneError::ListDevices(Box::new(source)))?;
    Ok(devices
        .filter_map(|device| {
            let id = InputDeviceId::of(&device)?;
            let name = device.description().ok()?.name().to_owned();
            let system_default = default.as_ref() == Some(&id);
            Some((
                device,
                InputDevice {
                    id,
                    name,
                    system_default,
                },
            ))
        })
        .collect())
}

/// What the idle worker does about the device it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Follow {
    Stay,
    /// Open whatever [`resolve_input`] picks now.
    Reopen,
    /// The chosen device was missing. Reopen if it has come back.
    CheckChosen,
}

pub(super) fn follow(
    wanted: Option<&InputDeviceId>,
    held: &InputDeviceId,
    default: Option<&InputDeviceId>,
) -> Follow {
    if wanted == Some(held) {
        return Follow::Stay;
    }
    if default != Some(held) {
        return Follow::Reopen;
    }
    if wanted.is_some() {
        return Follow::CheckChosen;
    }
    Follow::Stay
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, system_default: bool) -> InputDevice {
        InputDevice {
            id: InputDeviceId::new(id),
            name: id.to_owned(),
            system_default,
        }
    }

    fn id(id: &str) -> InputDeviceId {
        InputDeviceId::new(id)
    }

    #[test]
    fn a_connected_choice_wins_over_the_system_default() {
        let devices = [
            device("coreaudio:airpods", true),
            device("coreaudio:built-in", false),
        ];

        let resolved = resolve_input(Some(&id("coreaudio:built-in")), &devices);

        assert_eq!(resolved, Some(&devices[1]));
    }

    #[test]
    fn a_missing_choice_falls_back_to_the_system_default() {
        let devices = [device("coreaudio:built-in", true)];

        let resolved = resolve_input(Some(&id("coreaudio:airpods")), &devices);

        assert_eq!(resolved, Some(&devices[0]));
    }

    #[test]
    fn no_choice_takes_the_system_default() {
        let devices = [
            device("coreaudio:usb", false),
            device("coreaudio:built-in", true),
        ];

        assert_eq!(resolve_input(None, &devices), Some(&devices[1]));
    }

    #[test]
    fn nothing_resolves_without_a_device() {
        assert_eq!(resolve_input(Some(&id("coreaudio:airpods")), &[]), None);
    }

    #[test]
    fn a_held_choice_stays_when_the_default_moves() {
        let held = id("coreaudio:built-in");

        let next = follow(Some(&held), &held, Some(&id("coreaudio:airpods")));

        assert_eq!(next, Follow::Stay);
    }

    #[test]
    fn a_held_fallback_follows_the_default() {
        let next = follow(
            Some(&id("coreaudio:usb")),
            &id("coreaudio:built-in"),
            Some(&id("coreaudio:airpods")),
        );

        assert_eq!(next, Follow::Reopen);
    }

    #[test]
    fn a_held_fallback_watches_for_the_choice() {
        let held = id("coreaudio:built-in");

        let next = follow(Some(&id("coreaudio:usb")), &held, Some(&held));

        assert_eq!(next, Follow::CheckChosen);
    }

    #[test]
    fn clearing_the_choice_leaves_a_device_that_is_not_the_default() {
        let next = follow(None, &id("coreaudio:usb"), Some(&id("coreaudio:built-in")));

        assert_eq!(next, Follow::Reopen);
    }

    #[test]
    fn no_choice_on_the_default_stays() {
        let held = id("coreaudio:built-in");

        assert_eq!(follow(None, &held, Some(&held)), Follow::Stay);
    }
}
