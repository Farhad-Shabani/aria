//! Which input device a recording uses: the chosen one while it's connected,
//! else the fallback, which is the system default.
//!
//! An iPhone's Continuity microphone is never listed, chosen, or opened.
//! Holding it open wakes the iPhone's microphone whenever the phone comes
//! near, with no app on the phone in front. While one is the system default,
//! a built-in microphone stands in for it.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(target_os = "macos"))]
mod other;

use cpal::traits::{DeviceTrait, HostTrait};

#[cfg(target_os = "macos")]
use macos::transport;
#[cfg(not(target_os = "macos"))]
use other::transport;

use super::{MicrophoneError, Result};
use crate::audio::InputDeviceId;

fn device_id(device: &cpal::Device) -> Option<InputDeviceId> {
    device
        .id()
        .ok()
        .map(|id| InputDeviceId::new(id.to_string()))
}

/// A connected device that can record, other than an iPhone's microphone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDevice {
    pub id: InputDeviceId,
    pub name: String,
    /// The device a recording uses while no chosen one is available: the
    /// system default, or the one standing in for an iPhone default.
    pub fallback: bool,
}

/// Every connected input device but an iPhone's microphone, without opening
/// any of them.
pub fn input_devices() -> Result<Vec<InputDevice>> {
    Ok(connected(&cpal::default_host())?
        .into_iter()
        .map(|(_, device)| device)
        .collect())
}

/// The device a recording uses: `wanted` while it's available, else the
/// fallback. `None` when nothing can record.
pub fn resolve_input<'devices>(
    wanted: Option<&InputDeviceId>,
    devices: &'devices [InputDevice],
) -> Option<&'devices InputDevice> {
    wanted
        .and_then(|wanted| devices.iter().find(|device| device.id == *wanted))
        .or_else(|| devices.iter().find(|device| device.fallback))
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

/// Whether the device `id` names is connected and Aria may record on it, as
/// it never may on an iPhone's microphone. Opens nothing.
pub fn is_available(id: &InputDeviceId) -> bool {
    connected(&cpal::default_host())
        .is_ok_and(|inputs| inputs.iter().any(|(_, device)| device.id == *id))
}

/// The id [`resolve_input`] falls back to. The idle worker asks every 100 ms,
/// so the full listing runs only while an iPhone is the OS default.
pub(super) fn fallback() -> Option<InputDeviceId> {
    let host = cpal::default_host();
    let id = host.default_input_device().as_ref().and_then(device_id)?;
    if transport(&id) != Transport::ContinuityIphone {
        return Some(id);
    }
    listed_fallback(&host).map(|(_, device)| device.id)
}

/// The device [`resolve_input`] falls back to.
pub(super) fn fallback_device() -> Option<cpal::Device> {
    listed_fallback(&cpal::default_host()).map(|(device, _)| device)
}

fn listed_fallback(host: &cpal::Host) -> Option<(cpal::Device, InputDevice)> {
    connected(host)
        .ok()?
        .into_iter()
        .find(|(_, device)| device.fallback)
}

/// How a device reaches the computer, as far as choosing one cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    ContinuityIphone,
    BuiltIn,
    Other,
}

struct Found<D> {
    device: D,
    id: InputDeviceId,
    name: String,
    transport: Transport,
}

// A device that vanishes mid-listing has no identity or name to read. It is
// gone, so it is left out rather than failing the list.
fn connected(host: &cpal::Host) -> Result<Vec<(cpal::Device, InputDevice)>> {
    let os_default = host.default_input_device().as_ref().and_then(device_id);
    let devices = host
        .input_devices()
        .map_err(|source| MicrophoneError::ListDevices(Box::new(source)))?;
    let found = devices.filter_map(|device| {
        let id = device_id(&device)?;
        let name = device.description().ok()?.name().to_owned();
        let transport = transport(&id);
        Some(Found {
            device,
            id,
            name,
            transport,
        })
    });
    Ok(listed(found, os_default.as_ref()))
}

/// Leaves out iPhone microphones. When the OS default isn't listed, as when
/// it is an iPhone, a built-in microphone stands in, else the first listed.
fn listed<D>(
    found: impl IntoIterator<Item = Found<D>>,
    os_default: Option<&InputDeviceId>,
) -> Vec<(D, InputDevice)> {
    let kept: Vec<Found<D>> = found
        .into_iter()
        .filter(|found| found.transport != Transport::ContinuityIphone)
        .collect();
    let fallback = os_default.and_then(|os_default| {
        kept.iter()
            .position(|found| found.id == *os_default)
            .or_else(|| {
                kept.iter()
                    .position(|found| found.transport == Transport::BuiltIn)
            })
            .or_else(|| (!kept.is_empty()).then_some(0))
    });
    kept.into_iter()
        .enumerate()
        .map(|(index, found)| {
            (
                found.device,
                InputDevice {
                    id: found.id,
                    name: found.name,
                    fallback: fallback == Some(index),
                },
            )
        })
        .collect()
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
    fallback: Option<&InputDeviceId>,
) -> Follow {
    if wanted == Some(held) {
        return Follow::Stay;
    }
    if fallback != Some(held) {
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

    fn device(id: &str, fallback: bool) -> InputDevice {
        InputDevice {
            id: InputDeviceId::new(id),
            name: id.to_owned(),
            fallback,
        }
    }

    fn id(id: &str) -> InputDeviceId {
        InputDeviceId::new(id)
    }

    #[test]
    fn a_connected_choice_wins_over_the_fallback() {
        let devices = [
            device("coreaudio:airpods", true),
            device("coreaudio:built-in", false),
        ];

        let resolved = resolve_input(Some(&id("coreaudio:built-in")), &devices);

        assert_eq!(resolved, Some(&devices[1]));
    }

    #[test]
    fn a_missing_choice_takes_the_fallback() {
        let devices = [device("coreaudio:built-in", true)];

        let resolved = resolve_input(Some(&id("coreaudio:airpods")), &devices);

        assert_eq!(resolved, Some(&devices[0]));
    }

    #[test]
    fn no_choice_takes_the_fallback() {
        let devices = [
            device("coreaudio:usb", false),
            device("coreaudio:built-in", true),
        ];

        assert_eq!(resolve_input(None, &devices), Some(&devices[1]));
    }

    fn found(id: &str, transport: Transport) -> Found<()> {
        Found {
            device: (),
            id: InputDeviceId::new(id),
            name: id.to_owned(),
            transport,
        }
    }

    fn listed_devices(found: Vec<Found<()>>, os_default: &str) -> Vec<InputDevice> {
        listed(found, Some(&id(os_default)))
            .into_iter()
            .map(|(_, device)| device)
            .collect()
    }

    #[test]
    fn an_iphone_microphone_is_never_listed() {
        let listed = listed_devices(
            vec![
                found("coreaudio:iphone", Transport::ContinuityIphone),
                found("coreaudio:built-in", Transport::BuiltIn),
            ],
            "coreaudio:built-in",
        );

        assert_eq!(listed, [device("coreaudio:built-in", true)]);
    }

    #[test]
    fn a_built_in_microphone_stands_in_for_an_iphone_default() {
        let listed = listed_devices(
            vec![
                found("coreaudio:usb", Transport::Other),
                found("coreaudio:iphone", Transport::ContinuityIphone),
                found("coreaudio:built-in", Transport::BuiltIn),
            ],
            "coreaudio:iphone",
        );

        assert_eq!(
            listed,
            [
                device("coreaudio:usb", false),
                device("coreaudio:built-in", true)
            ]
        );
    }

    #[test]
    fn without_a_built_in_microphone_the_first_stands_in() {
        let listed = listed_devices(
            vec![
                found("coreaudio:iphone", Transport::ContinuityIphone),
                found("coreaudio:usb", Transport::Other),
            ],
            "coreaudio:iphone",
        );

        assert_eq!(listed, [device("coreaudio:usb", true)]);
    }

    #[test]
    fn nothing_resolves_without_a_device() {
        assert_eq!(resolve_input(Some(&id("coreaudio:airpods")), &[]), None);
    }

    #[test]
    fn a_held_choice_stays_when_the_fallback_moves() {
        let held = id("coreaudio:built-in");

        let next = follow(Some(&held), &held, Some(&id("coreaudio:airpods")));

        assert_eq!(next, Follow::Stay);
    }

    #[test]
    fn a_held_fallback_moves_with_the_fallback() {
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
    fn clearing_the_choice_leaves_a_device_that_is_not_the_fallback() {
        let next = follow(None, &id("coreaudio:usb"), Some(&id("coreaudio:built-in")));

        assert_eq!(next, Follow::Reopen);
    }

    #[test]
    fn no_choice_on_the_fallback_stays() {
        let held = id("coreaudio:built-in");

        assert_eq!(follow(None, &held, Some(&held)), Follow::Stay);
    }
}
