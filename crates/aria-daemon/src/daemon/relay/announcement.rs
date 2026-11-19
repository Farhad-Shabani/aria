//! What the status pill shows while a paired phone or watch makes a remote
//! recording, before its relay brings the audio.
//!
//! One announcement shows at a time, and it claims nothing, so a press on the
//! Mac still starts. A second replaces the first silently: its own
//! `Recording` already took the pill. `EndRemoteRecording` or [`DEADLINE`]
//! ends one with `Cancelled`, unless a newer recording started since, whose
//! stages the pill shows instead. A relay naming its id continues it.
//!
//! Other apps stay muted while one is on show, as for a press. However it
//! leaves show, their sound comes back: the device stopped recording.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use aria_voice::dictation::{Announced, CapturedMicrophone, RecordingId, Relayed, RemoteDevice};

use super::super::dictation::{DictationRuntime, DictationRuntimeError};
use super::{IDLE_TIMEOUT, Opening, decode::MAX_RECORDING_SECONDS};

/// The longest recording a device makes, then the wait for its relay to
/// open.
const DEADLINE: Duration = Duration::from_secs(MAX_RECORDING_SECONDS + IDLE_TIMEOUT.as_secs());

pub(in crate::daemon) struct Announcements {
    dictation: Arc<DictationRuntime>,
    shown: Mutex<Option<Shown>>,
}

/// The announcement on show, and the timer that ends it.
struct Shown {
    id: RecordingId,
    /// Holds other apps muted. `None` only once dropped.
    muted: Option<Announced>,
    deadline: tokio::task::AbortHandle,
}

impl Drop for Shown {
    /// However it ends, its timer goes with it, and other apps get their
    /// sound back. That restore is CoreAudio work, so it runs off the async
    /// workers that drop this.
    fn drop(&mut self) {
        self.deadline.abort();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if let Some(muted) = self.muted.take() {
            runtime.spawn_blocking(move || drop(muted));
        }
    }
}

impl Announcements {
    pub(in crate::daemon) fn new(dictation: Arc<DictationRuntime>) -> Arc<Self> {
        Arc::new(Self {
            dictation,
            shown: Mutex::new(None),
        })
    }

    /// Shows a remote recording from `device` and returns its id.
    pub(in crate::daemon) fn start(
        self: &Arc<Self>,
        device: RemoteDevice,
    ) -> Result<RecordingId, DictationRuntimeError> {
        // Held while announcing, so the one on show is always the newest.
        let mut shown = self.lock();
        let announced = self
            .dictation
            .announce_recording(CapturedMicrophone::Remote(device))?;
        let id = announced.id();
        let deadline = tokio::spawn(lapse(Arc::downgrade(self), id)).abort_handle();
        let replaced = shown
            .replace(Shown {
                id,
                muted: Some(announced),
                deadline,
            })
            .map(|shown| shown.id);
        tracing::info!(
            recording = %id,
            ?device,
            replaced = ?replaced,
            "remote recording announced"
        );
        Ok(id)
    }

    /// Ends the announcement on show, if `id` names it.
    pub(in crate::daemon) fn end(&self, id: RecordingId) {
        self.cancel(id, "ended");
    }

    /// How the relay `opening` shows: continuing the announcement on show
    /// when it names it, which takes that announcement off show.
    pub(in crate::daemon) fn showing(&self, opening: &Opening) -> Showing {
        let Some(shown) = opening.recording.and_then(|id| self.take(id)) else {
            return Showing::Unannounced(opening.device);
        };
        tracing::info!(recording = %shown.id, "remote recording relayed");
        Showing::Continued(Continued {
            id: shown.id,
            ending: Some(Arc::clone(&self.dictation)),
        })
    }

    fn cancel(&self, id: RecordingId, reason: &'static str) {
        let Some(shown) = self.take(id) else {
            return;
        };
        let cancelled = self.dictation.cancel_announced(shown.id);
        tracing::info!(recording = %id, reason, cancelled, "remote recording ended");
    }

    fn take(&self, id: RecordingId) -> Option<Shown> {
        let mut shown = self.lock();
        shown
            .as_ref()
            .is_some_and(|shown| shown.id == id)
            .then(|| shown.take())
            .flatten()
    }

    fn lock(&self) -> MutexGuard<'_, Option<Shown>> {
        self.shown.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

async fn lapse(announcements: Weak<Announcements>, id: RecordingId) {
    tokio::time::sleep(DEADLINE).await;
    if let Some(announcements) = announcements.upgrade() {
        announcements.cancel(id, "lapsed");
    }
}

/// How a relayed recording shows on the Mac.
pub(in crate::daemon) enum Showing {
    Continued(Continued),
    /// No announcement shows it, so its dictation announces itself from the
    /// device the relay named, if any.
    Unannounced(Option<RemoteDevice>),
}

impl Showing {
    /// Hands the announcement to the dictation, whose own stages end it.
    pub(in crate::daemon) fn into_relayed(self) -> Relayed {
        match self {
            Self::Continued(mut continued) => {
                continued.ending = None;
                Relayed::Announced(continued.id)
            }
            Self::Unannounced(device) => Relayed::Unannounced(
                device.map_or(CapturedMicrophone::Prerecorded, CapturedMicrophone::Remote),
            ),
        }
    }
}

/// An announcement a relay took off show to continue. Dropped before
/// [`Showing::into_relayed`] hands it on, as when the upload fails, it ends
/// as `EndRemoteRecording` would.
pub(in crate::daemon) struct Continued {
    id: RecordingId,
    /// `None` once handed on.
    ending: Option<Arc<DictationRuntime>>,
}

impl Drop for Continued {
    fn drop(&mut self) {
        let Some(dictation) = self.ending.take() else {
            return;
        };
        let cancelled = dictation.cancel_announced(self.id);
        tracing::info!(
            recording = %self.id,
            reason = "relay_failed",
            cancelled,
            "remote recording ended"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{Receiver, channel};

    use aria_kit::audio::RecordingReader;
    use aria_kit::cancel::CancelToken;
    use aria_platform::delivery::OutputMode;
    use aria_settings::event::EventEmitter;
    use aria_settings::{DeliveryMode, SettingsStore};
    use aria_voice::dictation::testing::{Edge, RecordsUntilStopped, recording_started};
    use aria_voice::dictation::{
        DictationEvent, DictationRequest, DictationStage, EscapeRule, StopMode, TranscriptRoute,
    };

    use super::super::super::dictation::AutoStopRule;
    use super::*;
    use crate::ipc::wire;

    /// Announcements over a runtime that records until stopped, with every
    /// stage the daemon announces.
    struct Harness {
        _directory: tempfile::TempDir,
        dictation: Arc<DictationRuntime>,
        announcements: Arc<Announcements>,
        stages: Receiver<DictationStage>,
        edges: Receiver<Edge>,
    }

    impl Harness {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let settings =
                Arc::new(SettingsStore::open(directory.path().join("settings.toml")).unwrap());
            let mut dictation = DictationRuntime::detached(settings);
            let (executor, edges) = RecordsUntilStopped::new();
            dictation.execute_with(Box::new(executor));
            let (sent, stages) = channel();
            dictation.set_emitter(EventEmitter::forwarded_by(move |event| {
                if let DictationEvent::Stage(stage) = event {
                    sent.send(stage).expect("the test is listening");
                }
            }));
            let dictation = Arc::new(dictation);
            Self {
                _directory: directory,
                announcements: Announcements::new(Arc::clone(&dictation)),
                dictation,
                stages,
                edges,
            }
        }

        fn heard(&self) -> Vec<DictationStage> {
            self.stages.try_iter().collect()
        }

        /// A relay that opens naming `recording`.
        fn relay(&self, recording: RecordingId) -> Showing {
            self.announcements.showing(&Opening {
                format: wire::AudioFormat::PcmS16le,
                submit: false,
                recording: Some(recording),
                device: Some(RemoteDevice::Phone),
            })
        }

        fn dictate(&self, showing: Showing) {
            let _no_speech = self.dictation.dictate_recording(
                &mut RecordingReader::from_samples(&[0.0; 16]),
                showing,
                aria_voice::dictation::testing::settings(
                    DeliveryMode::Complete,
                    OutputMode::Insert,
                ),
                CancelToken::never(),
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_dropped_announcement_restores_the_sound_off_the_async_workers() {
        let (sent, restored) = channel();
        let id = RecordingId(1);
        let shown = Shown {
            id,
            muted: Some(Announced::for_test(id, move || {
                sent.send(std::thread::current().id())
                    .expect("the test is listening");
            })),
            deadline: tokio::spawn(async {}).abort_handle(),
        };

        drop(shown);

        let restored_on = tokio::task::spawn_blocking(move || {
            restored
                .recv_timeout(Duration::from_secs(2))
                .expect("the sound comes back")
        })
        .await
        .unwrap();
        assert_ne!(restored_on, std::thread::current().id());
    }

    #[tokio::test]
    async fn a_start_shows_the_device_recording() {
        let harness = Harness::new();

        let id = harness.announcements.start(RemoteDevice::Watch).unwrap();

        assert_eq!(
            harness.heard(),
            [
                DictationStage::Recording {
                    id,
                    origin: aria_voice::dictation::RecordingOrigin::Remote,
                    delivery: DeliveryMode::Complete,
                    route: TranscriptRoute::Deliver,
                },
                DictationStage::Capturing {
                    id,
                    microphone: CapturedMicrophone::Remote(RemoteDevice::Watch),
                },
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_end_cancels_the_announcement_once() {
        let harness = Harness::new();
        let id = harness.announcements.start(RemoteDevice::Phone).unwrap();
        harness.heard();

        harness.announcements.end(id);
        tokio::time::sleep(Duration::from_secs(611)).await;

        assert_eq!(harness.heard(), [DictationStage::Cancelled]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_announcement_nobody_ends_is_cancelled_after_610_seconds() {
        let harness = Harness::new();
        harness.announcements.start(RemoteDevice::Phone).unwrap();
        harness.heard();

        tokio::time::sleep(Duration::from_millis(609_999)).await;
        assert_eq!(harness.heard(), []);
        tokio::time::sleep(Duration::from_millis(2)).await;

        assert_eq!(harness.heard(), [DictationStage::Cancelled]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_second_start_replaces_the_first_silently() {
        let harness = Harness::new();
        let first = harness.announcements.start(RemoteDevice::Phone).unwrap();
        tokio::time::sleep(Duration::from_secs(100)).await;
        let second = harness.announcements.start(RemoteDevice::Watch).unwrap();
        harness.heard();

        harness.announcements.end(first);
        tokio::time::sleep(Duration::from_secs(511)).await;
        assert_eq!(harness.heard(), [], "the first one ended with no word");
        harness.announcements.end(second);

        assert_eq!(harness.heard(), [DictationStage::Cancelled]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_press_started_meanwhile_silences_a_stale_end() {
        let harness = Harness::new();
        let announced = harness.announcements.start(RemoteDevice::Phone).unwrap();
        let pressing = std::thread::spawn({
            let dictation = Arc::clone(&harness.dictation);
            move || {
                dictation.execute(
                    DictationRequest::new(StopMode::Manual),
                    AutoStopRule::Setting,
                    EscapeRule::Passes,
                    CancelToken::never(),
                    None,
                )
            }
        });
        let pressed = recording_started(&harness.edges);
        harness.heard();

        harness.announcements.end(announced);
        assert!(harness.dictation.stop_recording(pressed));
        pressing.join().unwrap().unwrap();

        assert!(
            !harness.heard().contains(&DictationStage::Cancelled),
            "the press's pill stays its own"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_relay_naming_the_announcement_continues_it_under_its_id() {
        let harness = Harness::new();
        let id = harness.announcements.start(RemoteDevice::Phone).unwrap();
        harness.heard();

        harness.dictate(harness.relay(id));
        harness.announcements.end(id);
        tokio::time::sleep(Duration::from_secs(611)).await;

        assert_eq!(harness.edges.try_recv(), Ok(Edge::Relayed(id)));
        assert_eq!(harness.heard(), [], "no second Recording and no Cancelled");
    }

    #[tokio::test]
    async fn a_relay_that_fails_before_its_dictation_ends_the_announcement() {
        let harness = Harness::new();
        let id = harness.announcements.start(RemoteDevice::Phone).unwrap();
        harness.heard();

        drop(harness.relay(id));

        assert_eq!(harness.heard(), [DictationStage::Cancelled]);
    }

    #[tokio::test]
    async fn an_id_not_on_show_changes_nothing() {
        let harness = Harness::new();
        let id = harness.announcements.start(RemoteDevice::Phone).unwrap();
        let other = RecordingId(id.0 + 1);
        harness.heard();

        harness.announcements.end(other);
        let relay = harness.relay(other);
        assert!(matches!(
            relay,
            Showing::Unannounced(Some(RemoteDevice::Phone))
        ));
        assert_eq!(harness.heard(), []);

        harness.announcements.end(id);
        assert_eq!(
            harness.heard(),
            [DictationStage::Cancelled],
            "still on show"
        );
    }
}
