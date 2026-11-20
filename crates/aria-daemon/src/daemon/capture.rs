//! The capture source the daemon's settings select.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::daemon::config::WakeConfig;
use aria_kit::audio::{
    AudioCapture, CaptureError, IdleStream, InputDevice, InputDeviceId, Inputs, Microphone,
    MicrophoneError, PreparedMicrophone, PushedCapture, RecordingMode, RecordingReader, StopSignal,
    WakeWordMonitor, WavReplayCapture, is_available,
};
use aria_kit::voice_activity::SileroVad;
use aria_primitives::error_chain;
use aria_settings::config::ConfigError;
use aria_settings::rpc::RpcErrorCode;
use aria_voice::config::CaptureConfig;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::DictationInitializationError;
use super::microphone::{Target, admits_no_phone, admits_phone, input, target};
use super::phone::{AdmitsPhone, PhoneMicrophone};
use super::tasks::Supervised;
use super::wake::WakeDetector;

/// How long a press waits for the phone's first audio before it records on
/// the Mac. While the Mac's stream is held, it keeps its last second, so
/// falling back loses nothing.
const PHONE_FIRST_AUDIO: Duration = Duration::from_millis(600);
/// How often the wait for the phone's first audio checks for a stop.
const STOP_POLL: Duration = Duration::from_millis(10);
/// A phone that goes this quiet mid-recording has dropped. The recording
/// keeps what it sent.
const PHONE_STALL: Duration = Duration::from_secs(2);

pub(in crate::daemon) enum CaptureBackend {
    Microphone {
        microphone: Microphone,
        /// `dictation.microphone`, read once per press for both the phone
        /// and the Mac device.
        preference: watch::Receiver<aria_settings::Microphone>,
        /// `dictation.hold_microphone`, which the follower applies between
        /// recordings.
        hold: watch::Receiver<bool>,
        /// `None` for a daemon that serves no phone.
        phone: Option<PhoneInput>,
    },
    WavReplay(WavReplayCapture),
}

pub(in crate::daemon) struct PhoneInput {
    phones: Arc<PhoneMicrophone>,
    /// Whether a chosen Mac device is available, which decides the phone.
    available: fn(&InputDeviceId) -> bool,
    capture: PushedCapture<SileroVad>,
}

/// How a phone recording started, or the stop it hands back for the Mac.
enum Started {
    Phone(Result<RecordingReader, CaptureError>),
    Mac(StopSignal),
}

/// How the wait for a phone's first samples ended.
enum FirstAudio {
    Arrived(Vec<i16>),
    Stopped,
    /// None in time, or the phone went away.
    Missed,
}

/// Waits for the phone's first samples, checking `stop` as it waits.
fn first_audio(feed: &Receiver<Vec<i16>>, stop: &StopSignal) -> FirstAudio {
    let deadline = Instant::now() + PHONE_FIRST_AUDIO;
    loop {
        if stop.should_stop() {
            return FirstAudio::Stopped;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match feed.recv_timeout(left.min(STOP_POLL)) {
            Ok(first) => return FirstAudio::Arrived(first),
            Err(RecvTimeoutError::Timeout) if !left.is_zero() => {}
            Err(_missed_or_gone) => return FirstAudio::Missed,
        }
    }
}

impl PhoneInput {
    fn load(
        phones: Arc<PhoneMicrophone>,
        spool_dir: &Path,
        recording_timeout: Duration,
    ) -> Result<Self, CaptureError> {
        let vad = SileroVad::load().map_err(MicrophoneError::VoiceActivity)?;
        Ok(Self {
            phones,
            available: is_available,
            capture: PushedCapture::new(spool_dir, recording_timeout, vad),
        })
    }

    fn start(
        &self,
        preference: &aria_settings::Microphone,
        mode: RecordingMode,
        inputs: Inputs,
        stop: StopSignal,
    ) -> Started {
        let attached = self.phones.is_attached();
        if target(preference, inputs, attached, self.available) == Target::Mac {
            // An available chosen device outranks the phone, which then
            // waits awake for nothing.
            if attached && inputs == Inputs::Any {
                self.phones.dismiss();
            }
            return Started::Mac(stop);
        }
        if stop.should_stop() {
            return Started::Mac(stop);
        }
        let Some(recording) = self.phones.start() else {
            return Started::Mac(stop);
        };
        let first = match first_audio(&recording.feed, &stop) {
            FirstAudio::Arrived(first) => first,
            // The phone answered nothing yet but did nothing wrong, so it
            // stays attached.
            FirstAudio::Stopped => {
                self.phones.stop(recording.id);
                return Started::Mac(stop);
            }
            FirstAudio::Missed => {
                tracing::warn!(
                    wait_ms = PHONE_FIRST_AUDIO.as_millis() as u64,
                    "the phone sent no audio in time; recording on the Mac"
                );
                self.phones.abandon(recording.generation);
                return Started::Mac(stop);
            }
        };
        tracing::info!(stage = "recording", "recording on the phone");
        let phones = Arc::clone(&self.phones);
        let id = recording.id;
        Started::Phone(self.capture.start(
            mode,
            first,
            recording.feed,
            PHONE_STALL,
            stop,
            move || phones.stop(id),
        ))
    }

    /// Ends an attached phone's stream once `preference` rules it out, so it
    /// doesn't wait awake for nothing.
    fn apply(&self, preference: &aria_settings::Microphone) {
        if self.phones.is_attached()
            && target(preference, Inputs::Any, true, self.available) == Target::Mac
        {
            self.phones.dismiss();
        }
    }
}

impl CaptureBackend {
    /// The rule for whether a press would record on an attached phone.
    pub(in crate::daemon) fn admits_phone(&self) -> AdmitsPhone {
        match self {
            Self::Microphone { phone: Some(_), .. } => admits_phone,
            Self::Microphone { phone: None, .. } | Self::WavReplay(_) => admits_no_phone,
        }
    }

    /// Applies a changed `preference`: the Mac device it names, and the end
    /// of an attached phone it rules out.
    fn apply(&self, preference: &aria_settings::Microphone) -> Result<(), CaptureError> {
        match self {
            Self::Microphone {
                microphone, phone, ..
            } => {
                // First, and whatever the Mac device does: a phone the
                // setting rules out must not stay attached.
                if let Some(phone) = phone {
                    phone.apply(preference);
                }
                microphone.use_input(input(preference))
            }
            Self::WavReplay(_) => Ok(()),
        }
    }

    fn hold(&self, hold: bool) -> Result<(), CaptureError> {
        match self {
            Self::Microphone { microphone, .. } => microphone.use_idle(idle(hold)),
            Self::WavReplay(_) => Ok(()),
        }
    }
}

fn idle(hold: bool) -> IdleStream {
    if hold {
        IdleStream::Held
    } else {
        IdleStream::Released
    }
}

#[derive(Debug, thiserror::Error)]
#[error("couldn't list the microphones")]
pub(in crate::daemon) struct ListMicrophonesError(#[source] CaptureError);

impl From<&ListMicrophonesError> for RpcErrorCode {
    fn from(_error: &ListMicrophonesError) -> Self {
        Self::CaptureFailed
    }
}

/// Every input device Aria may record on. Opens none of them.
pub(in crate::daemon) fn list_microphones() -> Result<Vec<InputDevice>, ListMicrophonesError> {
    aria_kit::audio::input_devices().map_err(ListMicrophonesError)
}

/// Hands each change of `dictation.microphone` and
/// `dictation.hold_microphone` to the capture, which applies it before the
/// next recording.
pub(in crate::daemon) struct MicrophoneFollower {
    capture: Arc<CaptureBackend>,
    // Clones of the capture's watches. The capture never marks its own seen,
    // so a change made after it opened still reaches the follower.
    preference: watch::Receiver<aria_settings::Microphone>,
    hold: watch::Receiver<bool>,
}

impl MicrophoneFollower {
    /// `None` for a capture with no device to choose.
    pub(in crate::daemon) fn new(capture: Arc<CaptureBackend>) -> Option<Self> {
        let CaptureBackend::Microphone {
            preference, hold, ..
        } = capture.as_ref()
        else {
            return None;
        };
        let preference = preference.clone();
        let hold = hold.clone();
        Some(Self {
            capture,
            preference,
            hold,
        })
    }
}

impl Supervised for MicrophoneFollower {
    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = self.preference.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let preference = self.preference.borrow_and_update().clone();
                    let capture = Arc::clone(&self.capture);
                    // Ruling a phone out can list the audio devices.
                    let applied =
                        tokio::task::spawn_blocking(move || capture.apply(&preference)).await;
                    match applied {
                        Ok(Ok(())) => {}
                        // Keeps following, so a later change still ends a
                        // phone it rules out.
                        Ok(Err(error)) => {
                            tracing::error!(
                                error = %error_chain(&error),
                                "the capture stopped; the Mac device didn't change"
                            );
                        }
                        Err(_panicked) => {
                            tracing::error!(
                                "applying a microphone change panicked; changes no longer apply"
                            );
                            return;
                        }
                    }
                }
                changed = self.hold.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let hold = *self.hold.borrow_and_update();
                    // Only queues a command for the worker, so it doesn't block.
                    if let Err(error) = self.capture.hold(hold) {
                        tracing::error!(
                            error = %error_chain(&error),
                            "the capture stopped; holding the microphone didn't change"
                        );
                    }
                }
            }
        }
    }
}

impl AudioCapture for CaptureBackend {
    fn start(
        &self,
        mode: RecordingMode,
        inputs: Inputs,
        stop: StopSignal,
    ) -> Result<RecordingReader, CaptureError> {
        match self {
            Self::Microphone {
                microphone,
                preference,
                phone,
                ..
            } => {
                let preference = preference.borrow().clone();
                // The follower applies a change soon after it, but this press
                // must not record on a device it didn't choose. An unchanged
                // input is a no-op.
                microphone.use_input(input(&preference))?;
                let stop = match phone {
                    Some(phone) => match phone.start(&preference, mode, inputs, stop) {
                        Started::Phone(recording) => return recording,
                        Started::Mac(stop) => stop,
                    },
                    None => stop,
                };
                microphone.start(mode, inputs, stop)
            }
            Self::WavReplay(capture) => capture.start(mode, inputs, stop),
        }
    }
}

/// Capture with every model it needs loaded and the input device still
/// closed.
pub(in crate::daemon) enum PreparedCapture {
    Replay(WavReplayCapture),
    Microphone {
        microphone: Box<PreparedMicrophone>,
        wake: Option<WakeDetector>,
        preference: watch::Receiver<aria_settings::Microphone>,
        hold: watch::Receiver<bool>,
        phone: Option<PhoneInput>,
    },
}

impl PreparedCapture {
    /// `phones` is `None` for a daemon that serves no phone.
    pub(in crate::daemon) fn load(
        capture: CaptureConfig,
        wake: WakeConfig,
        preference: watch::Receiver<aria_settings::Microphone>,
        hold: watch::Receiver<bool>,
        phones: Option<Arc<PhoneMicrophone>>,
        spool_dir: &Path,
        recording_timeout: Duration,
    ) -> Result<Self, DictationInitializationError> {
        match capture {
            CaptureConfig::WavFile { path, pace } => {
                if matches!(wake, WakeConfig::Enabled { .. }) {
                    return Err(DictationInitializationError::Config(
                        ConfigError::WakeRequiresMicrophone,
                    ));
                }
                WavReplayCapture::new(spool_dir, recording_timeout, &path, pace)
                    .map(Self::Replay)
                    .map_err(DictationInitializationError::Capture)
            }
            CaptureConfig::Microphone => {
                let wake = match wake {
                    WakeConfig::Disabled => None,
                    WakeConfig::Enabled {
                        backend,
                        classifier,
                        threshold,
                    } => Some(WakeDetector::load(backend, classifier, threshold)?),
                };
                let started = Instant::now();
                let microphone = Microphone::builder()
                    .spool_dir(spool_dir)
                    .maximum_duration(recording_timeout)
                    .input(input(&preference.borrow()))
                    .idle(idle(*hold.borrow()))
                    .load()
                    .map_err(DictationInitializationError::Capture)?;
                tracing::info!(
                    load_ms = started.elapsed().as_millis(),
                    "silero speech detector loaded"
                );
                let phone = phones
                    .map(|phones| PhoneInput::load(phones, spool_dir, recording_timeout))
                    .transpose()
                    .map_err(DictationInitializationError::Capture)?;
                Ok(Self::Microphone {
                    microphone: Box::new(microphone),
                    wake,
                    preference,
                    hold,
                    phone,
                })
            }
        }
    }

    pub(in crate::daemon) fn open(
        self,
    ) -> Result<(CaptureBackend, Option<WakeWordMonitor>), CaptureError> {
        let (microphone, wake, preference, hold, phone) = match self {
            Self::Replay(replay) => return Ok((CaptureBackend::WavReplay(replay), None)),
            Self::Microphone {
                microphone,
                wake,
                preference,
                hold,
                phone,
            } => (microphone, wake, preference, hold, phone),
        };
        let (microphone, monitor) = match wake {
            None => ((*microphone).open()?, None),
            Some(detector) => {
                let (microphone, monitor) = detector.open_microphone(*microphone)?;
                (microphone, Some(monitor))
            }
        };
        Ok((
            CaptureBackend::Microphone {
                microphone,
                preference,
                hold,
                phone,
            },
            monitor,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use aria_kit::audio::Stopper;
    use aria_settings::Microphone as Preference;

    use super::super::phone::testing::FakePhone;
    use super::super::phone::{Detached, PhoneCommand, PhoneRequest};

    fn device(id: &str) -> Preference {
        id.parse().expect("a device id")
    }

    struct Phone {
        _directory: tempfile::TempDir,
        input: Arc<PhoneInput>,
        phones: Arc<PhoneMicrophone>,
        setting: watch::Sender<Preference>,
    }

    impl Phone {
        fn load() -> Self {
            Self::with_devices(|_| false)
        }

        /// `available` stands in for the Mac's devices.
        fn with_devices(available: fn(&InputDeviceId) -> bool) -> Self {
            let directory = tempfile::tempdir().expect("a temp dir");
            let phones = PhoneMicrophone::new();
            let (setting, _) = watch::channel(Preference::Automatic);
            let mut input = PhoneInput::load(
                Arc::clone(&phones),
                directory.path(),
                Duration::from_secs(60),
            )
            .expect("silero loads");
            input.available = available;
            Self {
                _directory: directory,
                input: Arc::new(input),
                phones,
                setting,
            }
        }

        async fn start(&self, inputs: Inputs, stop: StopSignal) -> Started {
            let input = Arc::clone(&self.input);
            let preference = self.setting.borrow().clone();
            tokio::task::spawn_blocking(move || {
                input.start(&preference, RecordingMode::UntilStopped, inputs, stop)
            })
            .await
            .expect("starting doesn't panic")
        }
    }

    fn speech() -> Vec<i16> {
        vec![1_000; 1_600]
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dictation_records_on_the_attached_phone() {
        let phone = Phone::load();
        let mut fake = FakePhone::attach(&phone.phones);
        let stopper = Stopper::new();
        let starting = phone.start(Inputs::Any, StopSignal::from(stopper.clone()));

        let (started, ()) = tokio::join!(starting, async {
            assert!(matches!(
                fake.commands.recv().await,
                Some(PhoneCommand::Start(_))
            ));
            fake.send(PhoneRequest::Audio(speech())).unwrap();
            fake.send(PhoneRequest::Audio(speech())).unwrap();
        });
        let Started::Phone(recording) = started else {
            panic!("the phone records");
        };
        let mut recording = recording.expect("audio arrived");
        tokio::time::sleep(Duration::from_millis(100)).await;
        stopper.stop();
        let audio = tokio::task::spawn_blocking(move || {
            recording.fill().expect("the recording finishes");
            recording.finished_audio().expect("audio").duration()
        })
        .await
        .unwrap();

        assert_eq!(audio, Duration::from_millis(200));
        assert!(matches!(
            fake.commands.recv().await,
            Some(PhoneCommand::Stop(_))
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_silent_phone_hands_the_recording_to_the_mac() {
        let phone = Phone::load();
        let mut fake = FakePhone::attach(&phone.phones);

        let started = phone.start(Inputs::Any, StopSignal::never()).await;

        assert!(matches!(started, Started::Mac(_)));
        assert_eq!(fake.detached(), Some(Detached::Silent));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_press_on_a_connected_chosen_device_dismisses_the_phone() {
        let phone = Phone::with_devices(|_| true);
        let mut fake = FakePhone::attach(&phone.phones);
        phone.setting.send_replace(device("coreaudio:usb"));

        let started = phone.start(Inputs::Any, StopSignal::never()).await;

        assert!(matches!(started, Started::Mac(_)));
        assert_eq!(fake.detached(), Some(Detached::Dismissed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_setting_change_to_a_connected_device_dismisses_the_phone() {
        let phone = Phone::with_devices(|_| true);
        let mut fake = FakePhone::attach(&phone.phones);

        phone.input.apply(&Preference::Automatic);
        assert_eq!(fake.detached(), None, "Automatic keeps the phone");
        phone.input.apply(&device("coreaudio:usb"));

        assert_eq!(fake.detached(), Some(Detached::Dismissed));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_press_never_asks_the_phone() {
        let phone = Phone::load();
        let mut fake = FakePhone::attach(&phone.phones);
        let stopper = Stopper::new();
        stopper.stop();

        let started = phone.start(Inputs::Any, StopSignal::from(stopper)).await;

        assert!(matches!(started, Started::Mac(_)));
        assert!(fake.commands.try_recv().is_err(), "no Start went out");
        assert_eq!(fake.detached(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stop_while_waiting_for_the_phone_ends_the_wait() {
        let phone = Phone::load();
        let mut fake = FakePhone::attach(&phone.phones);
        let stopper = Stopper::new();
        let starting = phone.start(Inputs::Any, StopSignal::from(stopper.clone()));

        let (started, ()) = tokio::join!(starting, async {
            assert!(matches!(
                fake.commands.recv().await,
                Some(PhoneCommand::Start(_))
            ));
            stopper.stop();
        });

        assert!(matches!(started, Started::Mac(_)));
        assert!(matches!(
            fake.commands.recv().await,
            Some(PhoneCommand::Stop(_))
        ));
        assert_eq!(fake.detached(), None, "the phone did nothing wrong");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_local_recording_never_asks_the_phone() {
        let phone = Phone::load();
        let mut fake = FakePhone::attach(&phone.phones);

        let started = phone.start(Inputs::Local, StopSignal::never()).await;

        assert!(matches!(started, Started::Mac(_)));
        assert!(fake.commands.try_recv().is_err(), "no Start went out");
    }

    use aria_kit::audio::ReplayPace;
    use aria_settings::WakeEngine;

    #[test]
    fn a_wake_word_on_a_replayed_capture_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let wake = WakeConfig::Enabled {
            backend: WakeEngine::OpenWakeWord,
            classifier: home.path().join("wake.onnx"),
            threshold: 0.5,
        };
        let replay = CaptureConfig::WavFile {
            path: aria_kit::audio::canonical_fixture(home.path()),
            pace: ReplayPace::Immediate,
        };

        let outcome = PreparedCapture::load(
            replay,
            wake,
            watch::channel(Preference::Automatic).1,
            watch::channel(true).1,
            None,
            home.path(),
            Duration::from_secs(60),
        );

        assert!(matches!(
            outcome,
            Err(DictationInitializationError::Config(
                ConfigError::WakeRequiresMicrophone
            ))
        ));
    }
}
