use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use aria_primitives::error_chain;
use aria_primitives::private_document::{DocumentError, PrivateDocument};
use cidre::{cf, core_audio as ca, os};
use serde::{Deserialize, Serialize};

const CONFIRM_TIMEOUT: Duration = Duration::from_millis(100);
const CONFIRM_POLL: Duration = Duration::from_millis(5);
const RESTORE_ATTEMPTS: usize = 2;
const UNMUTED: u32 = 0;
const MUTED: u32 = 1;
const LEDGER_MAXIMUM_BYTES: u64 = 16 * 1024;

/// The output devices Aria muted and hasn't unmuted yet. It lives on disk
/// because a killed daemon never runs `OutputMute`'s `Drop`, and an unplugged
/// device can't be unmuted until it returns.
pub struct MutedOutputs {
    document: PrivateDocument,
}

#[derive(Serialize, Deserialize)]
struct Ledger {
    devices: Vec<String>,
}

impl fmt::Debug for MutedOutputs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MutedOutputs")
            .finish_non_exhaustive()
    }
}

impl MutedOutputs {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            document: PrivateDocument::new(path, LEDGER_MAXIMUM_BYTES),
        }
    }

    /// Unmutes what an earlier recording left muted. A device that can't be
    /// unmuted yet stays listed for the next try.
    pub fn restore(&self) {
        self.restore_with(|uid| set_mute(uid, UNMUTED));
    }

    fn restore_with(&self, mut unmute: impl FnMut(&str) -> Result<(), OutputError>) {
        let owed = match self.devices() {
            Ok(owed) => owed,
            Err(error) => {
                tracing::error!(error = %error_chain(&error), "could not read the muted output devices");
                return;
            }
        };
        if owed.is_empty() {
            return;
        }
        let remaining: Vec<String> = owed
            .iter()
            .filter(|uid| match unmute(uid) {
                Ok(()) => false,
                Err(error) => {
                    tracing::warn!(%error, "could not unmute an output device left muted");
                    true
                }
            })
            .cloned()
            .collect();
        tracing::info!(
            restored = owed.len() - remaining.len(),
            remaining = remaining.len(),
            "restored output devices left muted"
        );
        self.replace(remaining);
    }

    fn record(&self, uid: &str) -> Result<(), DocumentError> {
        let mut devices = self.devices()?;
        if !devices.iter().any(|device| device == uid) {
            devices.push(uid.to_owned());
        }
        self.write(devices)
    }

    fn forget(&self, uid: &str) {
        match self.devices() {
            Ok(devices) => {
                self.replace(devices.into_iter().filter(|device| device != uid).collect());
            }
            Err(error) => {
                tracing::error!(error = %error_chain(&error), "could not read the muted output devices");
            }
        }
    }

    fn devices(&self) -> Result<Vec<String>, DocumentError> {
        let ledger = self.document.read::<Ledger>()?;
        Ok(ledger.map_or_else(Vec::new, |ledger| ledger.devices))
    }

    fn replace(&self, devices: Vec<String>) {
        if let Err(error) = self.write(devices) {
            tracing::error!(error = %error_chain(&error), "could not update the muted output devices");
        }
    }

    fn write(&self, devices: Vec<String>) -> Result<(), DocumentError> {
        if devices.is_empty() {
            self.document.remove()?;
            return Ok(());
        }
        self.document
            .prepare(&Ledger { devices })?
            .commit()?
            .confirm_durability()
    }
}

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
    pub(super) fn acquire(muted_outputs: &Arc<MutedOutputs>) -> Option<Self> {
        muted_outputs.restore();
        match snapshot() {
            Ok((uid, original)) => Self::tracked(muted_outputs, uid, original, set_mute),
            Err(error) => {
                tracing::warn!(%error, "could not mute output; recording continues");
                None
            }
        }
    }

    /// Lists the device before muting it, so no exit can leave a device muted
    /// without a record. A device that can't be listed stays unmuted.
    fn tracked(
        muted_outputs: &Arc<MutedOutputs>,
        uid: String,
        original: u32,
        write_device: impl Fn(&str, u32) -> Result<(), OutputError> + Send + 'static,
    ) -> Option<Self> {
        if original == UNMUTED
            && let Err(error) = muted_outputs.record(&uid)
        {
            tracing::warn!(error = %error_chain(&error), "could not list the output device; recording continues unmuted");
            return None;
        }
        let ledger = Arc::clone(muted_outputs);
        Some(Self::with_state(original, move |value| {
            write_device(&uid, value)?;
            if value == UNMUTED {
                ledger.forget(&uid);
            }
            Ok(())
        }))
    }

    pub(super) fn with_state(
        original: u32,
        write: impl FnMut(u32) -> Result<(), OutputError> + Send + 'static,
    ) -> Self {
        let mut output = Self {
            original,
            write: Box::new(write),
        };
        if original == UNMUTED
            && let Err(error) = (output.write)(MUTED)
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

    fn ledger() -> (tempfile::TempDir, Arc<MutedOutputs>) {
        let directory = tempfile::tempdir().unwrap();
        let muted_outputs = Arc::new(MutedOutputs::new(directory.path().join("muted.json")));
        (directory, muted_outputs)
    }

    fn succeeds(_uid: &str, _value: u32) -> Result<(), OutputError> {
        Ok(())
    }

    #[test]
    fn a_device_is_listed_while_muted_and_gone_once_restored() {
        let (_directory, muted_outputs) = ledger();
        let output = OutputMute::tracked(&muted_outputs, "speakers".into(), UNMUTED, succeeds);
        assert_eq!(muted_outputs.devices().unwrap(), ["speakers"]);
        drop(output);
        assert!(muted_outputs.devices().unwrap().is_empty());
    }

    #[test]
    fn a_killed_daemon_is_unmuted_by_the_next_one() {
        let (directory, muted_outputs) = ledger();
        std::mem::forget(OutputMute::tracked(
            &muted_outputs,
            "speakers".into(),
            UNMUTED,
            succeeds,
        ));

        let next_daemon = MutedOutputs::new(directory.path().join("muted.json"));
        let mut unmuted = Vec::new();
        next_daemon.restore_with(|uid| {
            unmuted.push(uid.to_owned());
            Ok(())
        });

        assert_eq!(unmuted, ["speakers"]);
        assert!(next_daemon.devices().unwrap().is_empty());
    }

    #[test]
    fn an_unplugged_device_stays_listed_until_it_can_be_unmuted() {
        let (_directory, muted_outputs) = ledger();
        drop(OutputMute::tracked(
            &muted_outputs,
            "headphones".into(),
            UNMUTED,
            |_uid, value| {
                if value == UNMUTED {
                    Err(OutputError::Unavailable)
                } else {
                    Ok(())
                }
            },
        ));
        assert_eq!(muted_outputs.devices().unwrap(), ["headphones"]);

        muted_outputs.restore_with(|_uid| Err(OutputError::Unavailable));
        assert_eq!(muted_outputs.devices().unwrap(), ["headphones"]);

        muted_outputs.restore_with(|_uid| Ok(()));
        assert!(muted_outputs.devices().unwrap().is_empty());
    }

    #[test]
    fn a_device_the_user_already_muted_is_never_listed() {
        let (_directory, muted_outputs) = ledger();
        let output = OutputMute::tracked(&muted_outputs, "speakers".into(), MUTED, succeeds);
        assert!(muted_outputs.devices().unwrap().is_empty());
        drop(output);
        assert!(muted_outputs.devices().unwrap().is_empty());
    }

    #[test]
    fn a_device_that_cannot_be_listed_is_not_muted() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("blocker");
        std::fs::write(&blocker, "").unwrap();
        let muted_outputs = Arc::new(MutedOutputs::new(blocker.join("muted.json")));
        let writes = Arc::new(Mutex::new(Vec::new()));
        let received = Arc::clone(&writes);

        let output = OutputMute::tracked(
            &muted_outputs,
            "speakers".into(),
            UNMUTED,
            move |_uid, value| {
                received.lock().unwrap().push(value);
                Ok(())
            },
        );

        assert!(output.is_none());
        assert!(writes.lock().unwrap().is_empty());
    }
}
