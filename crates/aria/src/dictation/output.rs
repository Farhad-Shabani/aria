use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

use cidre::{cf, core_audio as ca, os};

const CONFIRM_TIMEOUT: Duration = Duration::from_millis(100);
const CONFIRM_POLL: Duration = Duration::from_millis(5);
const RESTORE_ATTEMPTS: usize = 2;

pub(super) struct OutputMute {
    original: u32,
    write: Box<dyn FnMut(u32) -> Result<(), OutputError> + Send>,
}

impl fmt::Debug for OutputMute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OutputMute")
            .field("original", &self.original)
            .finish_non_exhaustive()
    }
}

impl OutputMute {
    pub(super) fn acquire() -> Option<Self> {
        let result = snapshot()
            .map(|(uid, original)| Self::with_state(original, move |value| set_mute(&uid, value)));
        match result {
            Ok(output) => Some(output),
            Err(error) => {
                tracing::warn!(%error, "could not mute output; recording continues");
                None
            }
        }
    }

    pub(super) fn with_state(
        original: u32,
        write: impl FnMut(u32) -> Result<(), OutputError> + Send + 'static,
    ) -> Self {
        let mut output = Self {
            original,
            write: Box::new(write),
        };
        if original == 0
            && let Err(error) = (output.write)(1)
        {
            tracing::warn!(%error, "could not confirm output mute; recording continues");
        }
        output
    }
}

impl Drop for OutputMute {
    fn drop(&mut self) {
        for attempt in 1..=RESTORE_ATTEMPTS {
            match (self.write)(self.original) {
                Ok(()) => return,
                Err(error) => {
                    tracing::error!(%error, attempt, "could not restore output mute state");
                }
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum OutputError {
    #[error("CoreAudio output operation failed: {0}")]
    Native(#[from] os::Error),
    #[error("the output device is unavailable")]
    Unavailable,
    #[error("the output device has no writable main mute control")]
    Unsupported,
    #[error("the output device returned an invalid mute value")]
    InvalidMute,
    #[error("the output device did not confirm its mute state")]
    Unconfirmed,
}

fn snapshot() -> Result<(String, u32), OutputError> {
    let device = ca::System::default_output_device()?;
    check_device(device)?;
    let original: u32 = device.prop(&ca::PropSelector::DEVICE_MUTE.output_addr())?;
    if original > 1 {
        return Err(OutputError::InvalidMute);
    }
    Ok((device.uid()?.to_string(), original))
}

fn check_device(device: ca::Device) -> Result<(), OutputError> {
    if device.is_unknown() || !device.is_alive()? {
        return Err(OutputError::Unavailable);
    }
    let address = ca::PropSelector::DEVICE_MUTE.output_addr();
    if !device.has_prop(&address) || !device.is_prop_settable(&address)? {
        return Err(OutputError::Unsupported);
    }
    Ok(())
}

fn set_mute(uid: &str, value: u32) -> Result<(), OutputError> {
    // The original device stays pinned. Following route changes would need
    // per-device restoration.
    let device = ca::Device::with_uid(&cf::String::from_str(uid))?;
    check_device(device)?;
    let address = ca::PropSelector::DEVICE_MUTE.output_addr();
    device.set_prop(&address, &value)?;
    let deadline = Instant::now() + CONFIRM_TIMEOUT;
    loop {
        if device.prop::<u32>(&address)? == value {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(OutputError::Unconfirmed);
        }
        thread::sleep(CONFIRM_POLL);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn mute_restores_the_saved_state_including_already_muted() {
        for (original, expected) in [(0, vec![1, 0]), (1, vec![1])] {
            let writes = Arc::new(Mutex::new(Vec::new()));
            let received = Arc::clone(&writes);
            let output = OutputMute::with_state(original, move |value| {
                received.lock().unwrap().push(value);
                Ok(())
            });
            drop(output);
            assert_eq!(*writes.lock().unwrap(), expected);
        }
    }

    #[test]
    fn failed_mute_keeps_the_saved_state_until_recording_ends() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&writes);
        let output = OutputMute::with_state(0, move |value| {
            received.lock().unwrap().push(value);
            if value == 1 {
                Err(OutputError::Unconfirmed)
            } else {
                Ok(())
            }
        });
        assert_eq!(*writes.lock().unwrap(), [1]);
        drop(output);
        assert_eq!(*writes.lock().unwrap(), [1, 0]);
    }

    #[test]
    fn restore_retries_are_bounded() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&writes);
        let output = OutputMute::with_state(0, move |value| {
            received.lock().unwrap().push(value);
            if value == 0 {
                Err(OutputError::Unavailable)
            } else {
                Ok(())
            }
        });
        drop(output);
        assert_eq!(*writes.lock().unwrap(), [1, 0, 0]);
    }
}
