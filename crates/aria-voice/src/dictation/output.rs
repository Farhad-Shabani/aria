use std::fmt;
use std::time::Instant;

use cidre::core_audio::{self as ca, aggregate_device_keys as aggregate, hardware};
use cidre::{cat, cf, ns, os};

/// Silences every app but this process while it lives, so cues stay audible.
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

#[derive(Debug, thiserror::Error)]
enum MuteError {
    #[error("CoreAudio refused: {0}")]
    Native(#[from] os::Error),
    /// Excluding an unknown process excludes nothing, so the tap would mute
    /// Aria's own cues too.
    #[error("CoreAudio doesn't know this process yet")]
    UnknownProcess,
}

/// A process tap mutes only while something reads it, so a private aggregate
/// device reads the tap and discards the audio. Both belong to this process,
/// so the sound comes back if it dies.
struct OtherAppsMuted {
    // Drop order: stop reading before the tap goes.
    reader: Option<hardware::StartedDevice<ca::AggregateDevice>>,
    _tap: ca::TapGuard,
}

impl OtherAppsMuted {
    fn start() -> Result<Self, MuteError> {
        let tap = tap_all_but_this_process()?;
        let reader = aggregate_reading(&tap)?;
        let discarding = reader.create_io_proc_id(discard, None)?;
        Ok(Self {
            reader: Some(ca::device_start(reader, Some(discarding))?),
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

fn tap_all_but_this_process() -> Result<ca::TapGuard, MuteError> {
    let pid = ns::ProcessInfo::current().process_id();
    let process = ca::Process::with_pid(pid)?;
    if process.0 == ca::Obj::UNKNOWN {
        return Err(MuteError::UnknownProcess);
    }
    let this_process = ns::Number::with_u32(process.0.0);
    let mut description =
        ca::TapDesc::with_stereo_global_tap_excluding_processes(&ns::Array::from_slice(&[
            this_process.as_ref(),
        ]));
    description.set_private(true);
    description.set_mute_behavior(ca::TapMuteBehavior::Muted);
    Ok(description.create_process_tap()?)
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

extern "C" fn discard(
    _device: ca::Device,
    _now: &cat::AudioTimeStamp,
    _input: &cat::AudioBufList<1>,
    _input_time: &cat::AudioTimeStamp,
    _output: &mut cat::AudioBufList<1>,
    _output_time: &cat::AudioTimeStamp,
    _client: Option<&mut std::ffi::c_void>,
) -> os::Status {
    os::Status::NO_ERR
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn dropping_the_mute_restores_the_sound() {
        let restored = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&restored);
        let mute = OutputMute::with_restore(move || observed.store(true, Ordering::SeqCst));
        assert!(!restored.load(Ordering::SeqCst));
        drop(mute);
        assert!(restored.load(Ordering::SeqCst));
    }

    #[test]
    #[ignore = "mutes this Mac's sound; run by hand with music playing"]
    fn other_apps_go_quiet_for_five_seconds() {
        let muted = OtherAppsMuted::start().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(5));
        drop(muted);
    }
}
