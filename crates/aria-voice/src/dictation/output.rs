use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use cidre::core_audio::{self as ca, aggregate_device_keys as aggregate, hardware};
use cidre::{cf, ns, os};

/// Silences other apps while it lives. This process stays audible for its
/// cues. Processes using the microphone when it starts, such as a call, stay
/// audible.
pub(super) struct OutputMute {
    restore: Option<Box<dyn FnOnce() + Send>>,
}

impl fmt::Debug for OutputMute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("OutputMute").finish_non_exhaustive()
    }
}

impl OutputMute {
    pub(super) fn acquire() -> Option<Self> {
        let started = Instant::now();
        match OtherAppsMuted::start() {
            Ok(muted) => {
                tracing::debug!(
                    elapsed_ms = started.elapsed().as_millis(),
                    "muted other apps"
                );
                Some(Self::with_restore(move || drop(muted)))
            }
            Err(error) => {
                tracing::warn!(%error, "could not mute other apps; recording continues");
                None
            }
        }
    }

    pub(super) fn with_restore(restore: impl FnOnce() + Send + 'static) -> Self {
        Self {
            restore: Some(Box::new(restore)),
        }
    }
}

impl Drop for OutputMute {
    fn drop(&mut self) {
        if let Some(restore) = self.restore.take() {
            restore();
        }
    }
}

/// Where a mute lands once it's ready. A released slot stays empty, so a
/// mute that lands after the recording ended restores at once.
#[derive(Debug, Default)]
pub(super) struct OutputSlot {
    mute: Option<OutputMute>,
    is_released: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl OutputSlot {
    /// Puts `mute` in the slot, as a mute that already landed.
    pub(super) fn hold(slot: &Mutex<Self>, mute: OutputMute) {
        lock(slot).mute = Some(mute);
    }

    #[cfg(test)]
    pub(super) fn is_muted(slot: &Mutex<Self>) -> bool {
        lock(slot).mute.is_some()
    }
}

impl OutputSlot {
    /// Mutes other apps on a thread of its own, since CoreAudio can be slow,
    /// and waits up to `wait` for it. Past that, the mute lands when ready.
    pub(super) fn mute(
        slot: &Arc<Mutex<Self>>,
        acquire: fn() -> Option<OutputMute>,
        wait: Option<Duration>,
        recording: super::RecordingId,
    ) {
        let (muted, waiting) = mpsc::channel();
        let landing = Arc::clone(slot);
        let spawned = thread::Builder::new()
            .name("aria-output-mute".into())
            .spawn(move || {
                let mute = acquire();
                let mut slot = lock(&landing);
                if !slot.is_released {
                    slot.mute = mute;
                }
                drop(slot);
                let _ = muted.send(());
            });
        match spawned {
            Ok(_) => {
                if let Some(wait) = wait
                    && waiting.recv_timeout(wait).is_err()
                {
                    tracing::warn!(
                        %recording,
                        "muting other apps is slow; recording starts without waiting"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not mute other apps; recording continues");
            }
        }
    }

    /// Restores the sound, and keeps a mute that lands later out. Holds the
    /// slot until the native restore finishes.
    pub(super) fn release(slot: &Mutex<Self>) {
        let mut slot = lock(slot);
        slot.is_released = true;
        drop(slot.mute.take());
    }
}

fn lock(slot: &Mutex<OutputSlot>) -> MutexGuard<'_, OutputSlot> {
    slot.lock().unwrap_or_else(|poisoned| {
        tracing::error!("the output mute lock was poisoned; recovering");
        poisoned.into_inner()
    })
}

#[derive(Debug, thiserror::Error)]
enum MuteError {
    #[error("CoreAudio refused: {0}")]
    Native(#[from] os::Error),
    /// Excluding an unknown process excludes nothing, so the tap would mute
    /// Aria's own cues too.
    #[error("CoreAudio doesn't know this process yet")]
    UnknownProcess,
}

/// A process tap mutes only while a running device reads it, so a private
/// aggregate device holds the tap and runs. Both belong to this process, so
/// the sound comes back if it dies.
struct OtherAppsMuted {
    // Drop order: stop reading before the tap goes.
    reader: Option<hardware::StartedDevice<ca::AggregateDevice>>,
    _tap: ca::TapGuard,
}

impl OtherAppsMuted {
    fn start() -> Result<Self, MuteError> {
        let tap = tap_all_but_spared()?;
        let reader = aggregate_reading(&tap)?;
        Ok(Self {
            reader: Some(ca::device_start(reader, None)?),
            _tap: tap,
        })
    }
}

impl Drop for OtherAppsMuted {
    fn drop(&mut self) {
        // `stop` reports a failure that the device's own drop would assert on.
        if let Some(reader) = self.reader.take()
            && let Err(error) = reader.stop()
        {
            tracing::warn!(%error, "could not stop reading the muted apps' sound");
        }
    }
}

fn tap_all_but_spared() -> Result<ca::TapGuard, MuteError> {
    let pid = ns::ProcessInfo::current().process_id();
    let process = ca::Process::with_pid(pid)?;
    if process.0 == ca::Obj::UNKNOWN {
        return Err(MuteError::UnknownProcess);
    }
    once_more_if_stale(|| {
        let mut listed = Vec::new();
        for other in processes()? {
            let Some(input) = microphone_use(other.is_running_input()) else {
                continue;
            };
            listed.push(ListedProcess {
                object: other.0.0,
                input,
            });
        }
        let spared: Vec<_> = spared(process.0.0, listed)
            .into_iter()
            .map(ns::Number::with_u32)
            .collect();
        let spared: Vec<_> = spared.iter().map(AsRef::as_ref).collect();
        let mut description = ca::TapDesc::with_stereo_global_tap_excluding_processes(
            &ns::Array::from_slice(&spared),
        );
        description.set_private(true);
        description.set_mute_behavior(ca::TapMuteBehavior::Muted);
        Ok(description.create_process_tap()?)
    })
}

/// A spared process can quit between the listing and the tap, and CoreAudio
/// then refuses the tap for its stale id. A fresh listing leaves it out.
fn once_more_if_stale<T>(
    mut attempt: impl FnMut() -> Result<T, MuteError>,
) -> Result<T, MuteError> {
    match attempt() {
        Err(MuteError::Native(error)) if error == ca::hardware_err::BAD_OBJ => attempt(),
        result => result,
    }
}

/// `ca::Process::list` keeps the length it sized before reading, so a process
/// that quits in between would leave its last entries unwritten.
#[allow(
    unsafe_code,
    reason = "reads CoreAudio's process list with the size it reports back"
)]
fn processes() -> os::Result<Vec<ca::Process>> {
    let address = ca::PropSelector::HW_PROCESS_OBJ_LIST.global_addr();
    let system = *ca::System::OBJ;
    let mut size = system.prop_size(&address)?;
    let mut objects = vec![0_u32; size as usize / size_of::<u32>()];
    // SAFETY: `objects` owns `size` writable bytes. CoreAudio writes at most
    // that many and stores in `size` how many it wrote.
    unsafe {
        AudioObjectGetPropertyData(
            system,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            objects.as_mut_ptr().cast(),
        )
    }
    .result()?;
    objects.truncate(size as usize / size_of::<u32>());
    Ok(objects
        .into_iter()
        .map(|object| ca::Process(ca::Obj(object)))
        .collect())
}

#[allow(unsafe_code, reason = "declares a CoreAudio function")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: ca::Obj,
        address: *const ca::PropAddr,
        qualifier_size: u32,
        qualifier: *const std::ffi::c_void,
        size: *mut u32,
        data: *mut std::ffi::c_void,
    ) -> os::Status;
}

/// None for a process that quit after the listing: it has no sound to spare.
fn microphone_use(answer: os::Result<bool>) -> Option<Input> {
    match answer {
        Ok(true) => Some(Input::Running),
        Ok(false) => Some(Input::Stopped),
        Err(error) if error == ca::hardware_err::BAD_OBJ => None,
        Err(error) => {
            tracing::warn!(%error, "could not tell whether a process uses the microphone; leaving it audible");
            Some(Input::Unknown)
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ListedProcess {
    object: u32,
    input: Input,
}

/// Unknown is spared, so a failed query can't silence a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Input {
    Running,
    Stopped,
    Unknown,
}

fn spared(this_process: u32, listed: impl IntoIterator<Item = ListedProcess>) -> Vec<u32> {
    let mut spared = vec![this_process];
    spared.extend(
        listed
            .into_iter()
            .filter(|listed| listed.input != Input::Stopped && listed.object != this_process)
            .map(|listed| listed.object),
    );
    spared
}

/// The output device drives the IO cycle; a tap alone never runs it.
fn aggregate_reading(tap: &ca::TapGuard) -> os::Result<ca::AggregateDevice> {
    let output = ca::System::default_output_device()?.uid()?;
    let output_entry =
        cf::DictionaryOf::with_keys_values(&[ca::sub_device_keys::uid()], &[output.as_type_ref()]);
    let tap_uid = tap.uid()?;
    let tap_entry = cf::DictionaryOf::with_keys_values(
        &[hardware::sub_tap_keys::uid()],
        &[tap_uid.as_type_ref()],
    );
    let sub_devices = cf::ArrayOf::from_slice(&[output_entry.as_ref()]);
    let taps = cf::ArrayOf::from_slice(&[tap_entry.as_ref()]);
    let uid = cf::Uuid::new().to_cf_string();
    let description = cf::DictionaryOf::with_keys_values(
        &[
            aggregate::uid(),
            aggregate::name(),
            aggregate::is_private(),
            aggregate::main_sub_device(),
            aggregate::sub_device_list(),
            aggregate::tap_list(),
            aggregate::tap_auto_start(),
        ],
        &[
            uid.as_type_ref(),
            cf::str!(c"Aria mute"),
            cf::Boolean::value_true(),
            &output,
            sub_devices.as_type_ref(),
            taps.as_type_ref(),
            cf::Boolean::value_true(),
        ],
    );
    ca::AggregateDevice::with_desc(&description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mute_spares_this_process_and_apps_using_the_microphone() {
        let listed = [
            ListedProcess {
                object: 7,
                input: Input::Running,
            },
            ListedProcess {
                object: 20,
                input: Input::Running,
            },
            ListedProcess {
                object: 30,
                input: Input::Stopped,
            },
        ];

        assert_eq!(spared(7, listed), [7, 20]);
    }

    #[test]
    fn a_failed_microphone_query_leaves_the_process_audible() {
        let input = microphone_use(Err(ca::hardware_err::UNKNOWN_PROP)).unwrap();
        let listed = [ListedProcess { object: 40, input }];

        assert_eq!(spared(7, listed), [7, 40]);
    }

    #[test]
    fn a_tap_refused_for_a_stale_process_is_built_once_more() {
        let mut attempts = 0;

        let outcome = once_more_if_stale(|| {
            attempts += 1;
            match attempts {
                1 => Err(MuteError::Native(ca::hardware_err::BAD_OBJ)),
                _ => Ok(attempts),
            }
        });

        assert_eq!(outcome.ok(), Some(2));
    }

    #[test]
    fn a_tap_refused_twice_for_a_stale_process_gives_up() {
        let mut attempts = 0;

        let outcome: Result<(), _> = once_more_if_stale(|| {
            attempts += 1;
            Err(MuteError::Native(ca::hardware_err::BAD_OBJ))
        });

        assert!(outcome.is_err());
        assert_eq!(attempts, 2);
    }

    #[test]
    fn a_tap_refused_for_another_reason_is_not_retried() {
        let mut attempts = 0;

        let outcome: Result<(), _> = once_more_if_stale(|| {
            attempts += 1;
            Err(MuteError::Native(ca::hardware_err::UNKNOWN_PROP))
        });

        assert!(outcome.is_err());
        assert_eq!(attempts, 1);
    }

    #[test]
    fn a_process_that_quit_after_the_listing_is_left_out() {
        assert_eq!(microphone_use(Err(ca::hardware_err::BAD_OBJ)), None);
    }

    #[test]
    #[ignore = "mutes this Mac's sound; run by hand with music playing"]
    fn other_apps_go_quiet_for_five_seconds() {
        let muted = OtherAppsMuted::start().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(5));
        drop(muted);
    }
}
