//! Recording a voice into a stored profile.
//!
//! Records several utterances, averages their voiceprints, and saves a profile.
//! Enrollment holds the microphone and suspends wake-word detection.

use std::sync::Arc;
use std::time::Duration;

use aria_kit::cancel::{CancelToken, Deadline};
use aria_speaker::wespeaker::WeSpeaker;

use crate::audio::{
    AudioCapture, CaptureError, RecordingMode, SilenceDuration, StopSignal, WakeWordControl,
};
use crate::config::SpeakerConfig;
use crate::event::EventEmitter;
use crate::microphone::Gate;
use crate::rpc::RpcErrorCode;
use crate::speaker::{
    Enrollment, EnrollmentError, Profile, ProfileError, ProfileStore, Replace, Role, SpeakerName,
};

pub(crate) type EnrollmentEventEmitter = EventEmitter<EnrollmentStage>;

pub(crate) const EVENT_THREAD: &str = "aria-enrollment-events";

/// Require multiple samples while limiting enrollment length.
const SAMPLE_RANGE: std::ops::RangeInclusive<u32> = aria_kit::speaker::MIN_SAMPLES as u32..=10;
/// Bounds the time enrollment can keep other recording operations waiting.
const ENROLLMENT_TIMEOUT: Duration = Duration::from_secs(300);

/// What to record, and for whom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrollmentRequest {
    pub name: SpeakerName,
    /// Absent takes the role that fits: the first voice on a machine owns it,
    /// and every voice after is a guest.
    pub role: Option<Role>,
    pub samples: u32,
    /// Whether an existing profile of this name may be replaced.
    pub replace: bool,
}

/// What an enrollment leaves behind.
#[derive(Debug, Clone, PartialEq)]
pub struct EnrollmentResult {
    pub name: SpeakerName,
    pub role: Role,
    /// How strongly the samples agreed, 0.0 to 1.0.
    pub consistency: f32,
}

/// One enrolled voice, as a client sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnrolledSpeaker {
    pub name: SpeakerName,
    pub role: Role,
    /// Seconds since the Unix epoch.
    pub enrolled_at: u64,
}

impl From<&Profile> for EnrolledSpeaker {
    fn from(profile: &Profile) -> Self {
        Self {
            name: profile.name.clone(),
            role: profile.role,
            enrolled_at: profile
                .enrolled_at
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        }
    }
}

/// Where an enrollment has got to. `index` counts from one, so it reads the
/// way it is printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnrollmentStage {
    /// Audio is flowing for this sample. Say something now.
    Recording {
        index: u32,
        of: u32,
    },
    /// That sample was heard and measured.
    SampleTaken {
        index: u32,
        of: u32,
    },
    Enrolled,
    Cancelled,
    Failed(RpcErrorCode),
}

impl EnrollmentStage {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Recording { .. } => "recording",
            Self::SampleTaken { .. } => "sample_taken",
            Self::Enrolled => "enrolled",
            Self::Cancelled => "cancelled",
            Self::Failed(_) => "failed",
        }
    }
}

/// Records enrollments.
///
/// Holding one is not proof the embedding model is installed: the model is
/// loaded per enrollment and released after, so a machine that never enrolls
/// never pays for it, and a model installed after the daemon started is
/// picked up without a restart.
pub(crate) struct Enroller<C> {
    capture: Arc<C>,
    microphone: Arc<Gate>,
    wake: Option<WakeWordControl>,
    store: ProfileStore,
    model: std::path::PathBuf,
}

/// Reading and forgetting enrolled voices, which need no microphone. Every
/// daemon serves these, however it was started.
pub(crate) struct Speakers {
    store: ProfileStore,
}

impl Speakers {
    pub(crate) fn open(speaker: &SpeakerConfig) -> Self {
        Self {
            store: ProfileStore::open(&speaker.speakers_dir),
        }
    }

    pub(crate) fn list(&self) -> Vec<EnrolledSpeaker> {
        crate::speaker::usable_profiles(self.store.read_all())
            .iter()
            .map(EnrolledSpeaker::from)
            .collect()
    }

    pub(crate) fn remove(&self, name: &SpeakerName) -> Result<(), EnrollError> {
        if self.store.remove(name)? {
            tracing::info!("a speaker profile was removed");
            Ok(())
        } else {
            Err(EnrollError::NotEnrolled)
        }
    }
}

impl<C: AudioCapture> Enroller<C> {
    pub(crate) fn new(
        capture: Arc<C>,
        microphone: Arc<Gate>,
        wake: Option<WakeWordControl>,
        speaker: &SpeakerConfig,
    ) -> Self {
        Self {
            capture,
            microphone,
            wake,
            store: ProfileStore::open(&speaker.speakers_dir),
            model: speaker.model.clone(),
        }
    }

    /// Records the samples and writes the profile.
    pub(crate) fn enroll(
        &self,
        request: EnrollmentRequest,
        cancellation: &CancelToken,
        events: &EnrollmentEventEmitter,
    ) -> Result<EnrollmentResult, EnrollError> {
        let result = self.run(request, cancellation, events);
        match &result {
            Ok(_) => events.emit(EnrollmentStage::Enrolled),
            Err(EnrollError::Cancelled) => events.emit(EnrollmentStage::Cancelled),
            Err(error) => events.emit(EnrollmentStage::Failed(error.code())),
        }
        result
    }

    fn run(
        &self,
        request: EnrollmentRequest,
        cancellation: &CancelToken,
        events: &EnrollmentEventEmitter,
    ) -> Result<EnrollmentResult, EnrollError> {
        if !SAMPLE_RANGE.contains(&request.samples) {
            return Err(EnrollError::SampleCount {
                requested: request.samples,
                minimum: *SAMPLE_RANGE.start(),
                maximum: *SAMPLE_RANGE.end(),
            });
        }

        let enrolled = crate::speaker::usable_profiles(self.store.read_all());
        if !request.replace && enrolled.iter().any(|profile| profile.name == request.name) {
            return Err(EnrollError::AlreadyEnrolled);
        }
        // The first voice on a machine is the person setting it up. Every
        // voice after belongs to someone visiting it. Re-recording the only
        // profile leaves it the owner's: the machine did not change hands.
        let role = request.role.unwrap_or_else(|| {
            if enrolled.iter().all(|profile| profile.name == request.name) {
                Role::Owner
            } else {
                Role::Guest
            }
        });

        // Nothing irreversible has happened yet, so a missing model is
        // refused before the microphone is taken.
        let embedder = WeSpeaker::load(&self.model).map_err(EnrollError::Model)?;
        let stop = CancelToken::new();
        let Some(_claim) = self.microphone.admit_enrollment() else {
            return Err(EnrollError::Busy);
        };
        let _wake_suspension = self
            .wake
            .as_ref()
            .map(WakeWordControl::suspend)
            .transpose()
            .map_err(EnrollError::Capture)?;

        // One budget over the whole run, folded into the stop signal each
        // sample polls, so a room that never falls silent still ends.
        let giving_up = Ending {
            cancellation: cancellation.clone(),
            stop,
            deadline: Deadline::after(ENROLLMENT_TIMEOUT),
        };

        let mut enrollment = Enrollment::new(embedder);
        for index in 1..=request.samples {
            giving_up.check()?;
            let samples = self.record(index, request.samples, &giving_up, events)?;
            giving_up.check()?;
            enrollment.add(&samples)?;
            events.emit(EnrollmentStage::SampleTaken {
                index,
                of: request.samples,
            });
        }

        let enrolled = enrollment.finish(request.name.clone(), role)?;
        let replace = if request.replace {
            Replace::Allowed
        } else {
            Replace::Never
        };
        match self.store.save(&enrolled.profile, replace) {
            Ok(()) => {}
            // The profile is stored and the next press will use it; only
            // whether it survives a power cut is unknown. Reporting failure
            // would send the person back to re-record what is already there.
            Err(error @ ProfileError::DurabilityUnconfirmed(_)) => {
                tracing::warn!(%error, "the speaker profile may not survive a power cut");
            }
            Err(error) => return Err(error.into()),
        }
        tracing::info!(%role, samples = request.samples, "a speaker profile was enrolled");
        Ok(EnrollmentResult {
            name: request.name,
            role,
            consistency: enrolled.consistency,
        })
    }

    /// One utterance, as 16 kHz samples.
    fn record(
        &self,
        index: u32,
        of: u32,
        giving_up: &Ending,
        events: &EnrollmentEventEmitter,
    ) -> Result<Vec<f32>, EnrollError> {
        let mut recording = self.capture.start(
            RecordingMode::StopOnSilence {
                trailing_silence: SilenceDuration::default(),
            },
            giving_up.signal(),
        )?;
        events.emit(EnrollmentStage::Recording { index, of });
        recording.fill()?;
        Ok(recording.samples().to_vec())
    }
}

/// Every reason an enrollment stops early, in one place: the client gave up,
/// or it ran past its budget.
struct Ending {
    cancellation: CancelToken,
    stop: CancelToken,
    deadline: Deadline,
}

impl Ending {
    fn check(&self) -> Result<(), EnrollError> {
        if self.cancellation.is_cancelled() || self.stop.is_cancelled() {
            return Err(EnrollError::Cancelled);
        }
        if self.deadline.expired() {
            return Err(EnrollError::Timeout(self.deadline.timeout()));
        }
        Ok(())
    }

    /// The same reasons, as the capture thread polls them.
    fn signal(&self) -> StopSignal {
        let ending = Self {
            cancellation: self.cancellation.clone(),
            stop: self.stop.clone(),
            deadline: self.deadline,
        };
        StopSignal::from_fn(move || ending.check().is_err())
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum EnrollError {
    #[error("the microphone is already in use")]
    Busy,

    #[error("this daemon does not enroll; install the service with `aria service install`")]
    Unsupported,

    #[error("enrollment was cancelled")]
    Cancelled,

    #[error("enrollment ran past its time limit of {0:?}")]
    Timeout(Duration),

    #[error("no voice-embedding model is installed; run `aria setup` to fetch one")]
    Model(#[source] aria_kit::speaker::SpeakerError),

    #[error("that name is already enrolled")]
    AlreadyEnrolled,

    #[error("that name is not enrolled")]
    NotEnrolled,

    #[error("enrolling takes {minimum} to {maximum} samples; {requested} were asked for")]
    SampleCount {
        requested: u32,
        minimum: u32,
        maximum: u32,
    },

    #[error("no speech was recorded")]
    NoSpeech,

    #[error(transparent)]
    Enrollment(EnrollmentError),

    #[error("audio capture failed")]
    Capture(#[source] CaptureError),

    #[error("the speaker profile could not be written")]
    Profile(#[from] ProfileError),
}

impl From<CaptureError> for EnrollError {
    fn from(error: CaptureError) -> Self {
        match error {
            CaptureError::NoSpeech(_timeout) => Self::NoSpeech,
            error => Self::Capture(error),
        }
    }
}

impl From<EnrollmentError> for EnrollError {
    fn from(error: EnrollmentError) -> Self {
        match error {
            EnrollmentError::NoVoice => Self::NoSpeech,
            error => Self::Enrollment(error),
        }
    }
}

impl EnrollError {
    pub(crate) fn code(&self) -> RpcErrorCode {
        match self {
            Self::Busy => RpcErrorCode::Busy,
            Self::Unsupported => RpcErrorCode::Unsupported,
            Self::Cancelled => RpcErrorCode::Cancelled,
            Self::Timeout(_) => RpcErrorCode::OverallTimeout,
            Self::Model(_) => RpcErrorCode::SpeakerModelMissing,
            Self::AlreadyEnrolled | Self::Profile(ProfileError::AlreadyStored) => {
                RpcErrorCode::SpeakerAlreadyEnrolled
            }
            Self::NotEnrolled => RpcErrorCode::SpeakerNotEnrolled,
            Self::SampleCount { .. } => RpcErrorCode::InvalidRequest,
            Self::NoSpeech => RpcErrorCode::SpeakerNoSpeech,
            Self::Enrollment(EnrollmentError::Inconsistent { .. }) => {
                RpcErrorCode::SpeakerInconsistentSamples
            }
            Self::Enrollment(_) => RpcErrorCode::Internal,
            Self::Capture(_) => RpcErrorCode::CaptureFailed,
            Self::Profile(_) => RpcErrorCode::SpeakerProfileWriteFailed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{RecordingReader, StopSignal};
    use crate::config::SpeakerConfig;
    use aria_kit::speaker::{Role, enrolled_voice};

    /// Stands in for the microphone in the checks that run before one is
    /// ever opened.
    struct Silent;

    impl AudioCapture for Silent {
        fn start(
            &self,
            _mode: RecordingMode,
            _stop: StopSignal,
        ) -> Result<RecordingReader, CaptureError> {
            unreachable!("these checks refuse before anything records")
        }
    }

    fn config(directory: &std::path::Path) -> SpeakerConfig {
        SpeakerConfig {
            model: directory.join("absent.onnx"),
            segmentation_model: directory.join("absent-segmentation.onnx"),
            speakers_dir: directory.to_path_buf(),
        }
    }

    /// An enroller over a speakers directory that lives as long as the test
    /// holds the pair.
    fn enroller() -> (tempfile::TempDir, Enroller<Silent>) {
        let home = tempfile::tempdir().expect("a temporary directory");
        let enroller = Enroller::new(Arc::new(Silent), Gate::new(), None, &config(home.path()));
        (home, enroller)
    }

    fn request(name: &str, samples: u32, replace: bool) -> EnrollmentRequest {
        EnrollmentRequest {
            name: SpeakerName::new(name).expect("the name is usable"),
            role: None,
            samples,
            replace,
        }
    }

    fn enroll(enroller: &Enroller<Silent>, request: EnrollmentRequest) -> EnrollError {
        enroller
            .enroll(
                request,
                &CancelToken::new(),
                &EnrollmentEventEmitter::unsubscribed(),
            )
            .expect_err("the enrollment cannot finish in a test")
    }

    #[test]
    fn a_sample_count_outside_the_range_is_refused() {
        let (_home, enroller) = enroller();

        assert!(matches!(
            enroll(&enroller, request("ada", 1, false)),
            EnrollError::SampleCount { requested: 1, .. }
        ));
        assert!(matches!(
            enroll(&enroller, request("ada", 99, false)),
            EnrollError::SampleCount { requested: 99, .. }
        ));
    }

    #[test]
    fn an_enrolled_name_is_not_overwritten_without_asking() {
        let (home, enroller) = enroller();
        ProfileStore::open(home.path())
            .save(
                &enrolled_voice("ada", Role::Owner, vec![1.0, 0.0]),
                Replace::Never,
            )
            .expect("the profile is written");

        assert!(matches!(
            enroll(&enroller, request("ada", 5, false)),
            EnrollError::AlreadyEnrolled
        ));
    }

    #[test]
    fn a_missing_model_is_refused_before_the_microphone_is_taken() {
        let (_home, enroller) = enroller();

        assert!(matches!(
            enroll(&enroller, request("ada", 5, false)),
            EnrollError::Model(_)
        ));
        assert!(
            enroller.microphone.admit_enrollment().is_some(),
            "a refused enrollment leaves the microphone free"
        );
    }

    #[test]
    fn removing_a_name_nobody_enrolled_says_so() {
        let home = tempfile::tempdir().expect("a temporary directory");
        let speakers = Speakers::open(&config(home.path()));
        let ada = SpeakerName::new("ada").expect("the name is usable");

        assert!(matches!(
            speakers.remove(&ada),
            Err(EnrollError::NotEnrolled)
        ));
    }
}
