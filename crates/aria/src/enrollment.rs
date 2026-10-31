//! Recording a voice into a stored profile.
//!
//! Records several utterances, averages their voiceprints, and saves a profile.
//! Enrollment holds the microphone and suspends wake-word detection.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use aria_kit::cancel::{CancelToken, Deadline};
use aria_speaker::wespeaker::WeSpeaker;

use crate::audio::{
    AudioCapture, CaptureError, RecordingMode, SilenceDuration, StopSignal, WakeWordControl,
};
use crate::config::SpeakerConfig;
use crate::event::EventEmitter;
use crate::microphone::Gate;
use crate::primitives::error_chain;
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
    /// Absent keeps a replaced voice's role, and makes a new voice the owner
    /// only while nobody owns the machine.
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
    speakers: Arc<Speakers>,
    model: std::path::PathBuf,
}

/// The enrolled voices, which need no microphone. Every write to the
/// speakers directory goes through here.
pub(crate) struct Speakers {
    store: ProfileStore,
    /// Held from the read that decides roles to the last write, so two
    /// requests never write from each other's stale read.
    writing: Mutex<()>,
}

impl Speakers {
    pub(crate) fn open(speaker: &SpeakerConfig) -> Self {
        Self {
            store: ProfileStore::open(&speaker.profiles_dir),
            writing: Mutex::new(()),
        }
    }

    pub(crate) fn list(&self) -> Vec<EnrolledSpeaker> {
        crate::speaker::usable_profiles(self.store.read_all())
            .iter()
            .map(EnrolledSpeaker::from)
            .collect()
    }

    /// `on_last_owner` runs under the write lock, so an owner enrolled or
    /// chosen meanwhile is never mistaken for none.
    pub(crate) fn remove(
        &self,
        name: &SpeakerName,
        on_last_owner: impl FnOnce(),
    ) -> Result<(), EnrollError> {
        let _writing = self.lock();
        if !self.store.remove(name)? {
            return Err(EnrollError::NotEnrolled);
        }
        tracing::info!("a speaker profile was removed");
        match self.store.read_all() {
            Ok(stored) if !stored.profiles.iter().any(|p| p.role == Role::Owner) => on_last_owner(),
            Ok(_) => {}
            Err(error) => tracing::warn!(
                error = %error_chain(&error),
                "the speakers directory is unreadable; the owner is unknown"
            ),
        }
        Ok(())
    }

    /// Whether `name` addresses a stored voice, matched the way saving
    /// matches it.
    pub(crate) fn is_enrolled(&self, name: &SpeakerName) -> bool {
        let profiles = crate::speaker::usable_profiles(self.store.read_all());
        self.find(&profiles, name).is_some()
    }

    /// Makes `name` the only owner.
    pub(crate) fn make_owner(&self, name: &SpeakerName) -> Result<(), EnrollError> {
        let _writing = self.lock();
        let profiles = self.store.read_all()?.profiles;
        let chosen = self.find(&profiles, name).ok_or(EnrollError::NotEnrolled)?;
        if chosen.role != Role::Owner {
            self.save(
                &Profile {
                    role: Role::Owner,
                    ..chosen.clone()
                },
                Replace::Allowed,
            )?;
        }
        self.demote_owners_besides(&profiles, &chosen.name)?;
        tracing::info!("the machine's owner changed");
        Ok(())
    }

    /// Settles the role here rather than when recording began, since the
    /// owner can change meanwhile.
    fn save_recorded(
        &self,
        mut profile: Profile,
        requested: Option<Role>,
        replace: Replace,
    ) -> Result<Role, EnrollError> {
        let _writing = self.lock();
        let profiles = self.store.read_all()?.profiles;
        let replaced = self.find(&profiles, &profile.name);
        profile.role = requested.unwrap_or_else(|| match replaced {
            Some(replaced) => replaced.role,
            None if profiles.iter().any(|other| other.role == Role::Owner) => Role::Guest,
            None => Role::Owner,
        });
        self.save(&profile, replace)?;
        if profile.role == Role::Owner {
            // The replaced profile is the file just written, whatever case
            // its name is stored in, so it is never demoted.
            let saved = replaced.map_or(&profile.name, |replaced| &replaced.name);
            self.demote_owners_besides(&profiles, saved)?;
        }
        Ok(profile.role)
    }

    /// Runs after the new owner is saved, so a failure partway leaves two
    /// owners rather than none.
    fn demote_owners_besides(
        &self,
        profiles: &[Profile],
        owner: &SpeakerName,
    ) -> Result<(), EnrollError> {
        for other in profiles
            .iter()
            .filter(|profile| &profile.name != owner && profile.role == Role::Owner)
        {
            self.save(
                &Profile {
                    role: Role::Guest,
                    ..other.clone()
                },
                Replace::Allowed,
            )?;
        }
        Ok(())
    }

    fn save(&self, profile: &Profile, replace: Replace) -> Result<(), EnrollError> {
        match self.store.save(profile, replace) {
            Ok(()) => Ok(()),
            // The profile is stored and live; only whether it survives a
            // power cut is unknown. Reporting failure would send the person
            // back to redo what is already there.
            Err(error @ ProfileError::DurabilityUnconfirmed(_)) => {
                tracing::warn!(%error, "the speaker profile may not survive a power cut");
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    /// The stored profile `name` addresses. The filesystem decides whether
    /// "Ada" and `ada.json` are one voice, so a match in another case counts
    /// only where it finds a file for `name`.
    fn find<'a>(&self, profiles: &'a [Profile], name: &SpeakerName) -> Option<&'a Profile> {
        if let Some(exact) = profiles.iter().find(|profile| &profile.name == name) {
            return Some(exact);
        }
        if !self.store.contains(name) {
            return None;
        }
        // ponytail: folds case only. A name that differs from its file only
        // in Unicode normalization goes unmatched; compare file identities if
        // that ever matters.
        let wanted = name.as_str().to_lowercase();
        let mut caseless = profiles
            .iter()
            .filter(|profile| profile.name.as_str().to_lowercase() == wanted);
        match (caseless.next(), caseless.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    }

    fn lock(&self) -> MutexGuard<'_, ()> {
        self.writing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<C: AudioCapture> Enroller<C> {
    pub(crate) fn new(
        capture: Arc<C>,
        microphone: Arc<Gate>,
        wake: Option<WakeWordControl>,
        speakers: Arc<Speakers>,
        speaker: &SpeakerConfig,
    ) -> Self {
        Self {
            capture,
            microphone,
            wake,
            speakers,
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
            // A client that hangs up has already dropped its RPC call, so the
            // RPC layer never logs this ending.
            Err(EnrollError::Cancelled) => {
                tracing::info!(status = "cancelled", "a speaker enrollment ended");
                events.emit(EnrollmentStage::Cancelled);
            }
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

        if !request.replace && self.speakers.is_enrolled(&request.name) {
            return Err(EnrollError::AlreadyEnrolled);
        }

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

        // A placeholder: the role is settled as the voice is saved.
        let enrolled = enrollment.finish(request.name.clone(), Role::Guest)?;
        let replace = if request.replace {
            Replace::Allowed
        } else {
            Replace::Never
        };
        let role = self
            .speakers
            .save_recorded(enrolled.profile, request.role, replace)?;
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
            profiles_dir: directory.to_path_buf(),
        }
    }

    /// An enroller over a speakers directory that lives as long as the test
    /// holds the pair.
    fn enroller() -> (tempfile::TempDir, Enroller<Silent>) {
        let home = tempfile::tempdir().expect("a temporary directory");
        let config = config(home.path());
        let speakers = Arc::new(Speakers::open(&config));
        let enroller = Enroller::new(Arc::new(Silent), Gate::new(), None, speakers, &config);
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
            speakers.remove(&ada, || unreachable!("nothing was removed")),
            Err(EnrollError::NotEnrolled)
        ));
    }

    fn name(value: &str) -> SpeakerName {
        SpeakerName::new(value).expect("the name is usable")
    }

    /// Speakers over a directory holding `voices`, kept alive by the guard.
    fn speakers_with(voices: &[(&str, Role)]) -> (tempfile::TempDir, Speakers) {
        let home = tempfile::tempdir().expect("a temporary directory");
        let store = ProfileStore::open(home.path());
        for (name, role) in voices {
            store
                .save(&enrolled_voice(name, *role, vec![1.0, 0.0]), Replace::Never)
                .expect("the profile is written");
        }
        let speakers = Speakers::open(&config(home.path()));
        (home, speakers)
    }

    fn roles(speakers: &Speakers) -> Vec<(String, Role)> {
        speakers
            .list()
            .into_iter()
            .map(|speaker| (speaker.name.to_string(), speaker.role))
            .collect()
    }

    #[test]
    fn a_new_owner_takes_over_from_the_old_one() {
        let (_home, speakers) = speakers_with(&[("ada", Role::Owner), ("bo", Role::Guest)]);

        speakers.make_owner(&name("bo")).expect("the owner changes");

        assert_eq!(
            roles(&speakers),
            [
                ("ada".to_owned(), Role::Guest),
                ("bo".to_owned(), Role::Owner)
            ]
        );
    }

    #[test]
    fn choosing_the_owner_again_changes_nothing() {
        let (_home, speakers) = speakers_with(&[("ada", Role::Owner), ("bo", Role::Guest)]);

        speakers.make_owner(&name("ada")).expect("the owner stays");

        assert_eq!(
            roles(&speakers),
            [
                ("ada".to_owned(), Role::Owner),
                ("bo".to_owned(), Role::Guest)
            ]
        );
    }

    /// Whether the directory's filesystem finds `ada.json` for "Ada", as
    /// macOS does and Linux does not.
    fn folds_case(home: &tempfile::TempDir) -> bool {
        ProfileStore::open(home.path()).contains(&name("ADA"))
    }

    #[test]
    fn a_name_in_another_case_is_the_same_voice_only_where_the_filesystem_says_so() {
        let (home, speakers) = speakers_with(&[("ada", Role::Guest), ("bo", Role::Owner)]);

        assert_eq!(speakers.is_enrolled(&name("Ada")), folds_case(&home));
        assert_eq!(speakers.make_owner(&name("Ada")).is_ok(), folds_case(&home));
    }

    #[test]
    fn re_recording_the_owner_in_another_case_keeps_one_owner() {
        let (home, speakers) = speakers_with(&[("ada", Role::Owner)]);

        let role = record(&speakers, "Ada", None, Replace::Allowed);

        let expected = if folds_case(&home) {
            Role::Owner
        } else {
            Role::Guest
        };
        assert_eq!(role, expected);
        let owners = roles(&speakers)
            .into_iter()
            .filter(|(_, role)| *role == Role::Owner)
            .count();
        assert_eq!(owners, 1, "never none, never two");
    }

    /// Saves a fresh recording of `name` as enrollment would.
    fn record(speakers: &Speakers, name: &str, requested: Option<Role>, replace: Replace) -> Role {
        speakers
            .save_recorded(
                enrolled_voice(name, Role::Guest, vec![0.0, 1.0]),
                requested,
                replace,
            )
            .expect("the voice is saved")
    }

    #[test]
    fn a_re_recorded_voice_keeps_the_role_it_has_when_saved() {
        let (_home, speakers) = speakers_with(&[("ada", Role::Owner), ("bo", Role::Guest)]);
        // Bo became the owner while the recording was under way.
        speakers.make_owner(&name("bo")).expect("the owner changes");

        assert_eq!(record(&speakers, "bo", None, Replace::Allowed), Role::Owner);
        assert_eq!(
            roles(&speakers),
            [
                ("ada".to_owned(), Role::Guest),
                ("bo".to_owned(), Role::Owner)
            ]
        );
    }

    #[test]
    fn a_new_voice_owns_the_machine_only_while_nobody_does() {
        let (_home, speakers) = speakers_with(&[("ada", Role::Guest)]);

        assert_eq!(record(&speakers, "bo", None, Replace::Never), Role::Owner);
        assert_eq!(record(&speakers, "cy", None, Replace::Never), Role::Guest);
    }

    #[test]
    fn recording_a_new_owner_makes_the_old_one_a_guest() {
        let (_home, speakers) = speakers_with(&[("ada", Role::Owner)]);

        assert_eq!(
            record(&speakers, "bo", Some(Role::Owner), Replace::Never),
            Role::Owner
        );
        assert_eq!(
            roles(&speakers),
            [
                ("ada".to_owned(), Role::Guest),
                ("bo".to_owned(), Role::Owner)
            ]
        );
    }

    #[test]
    fn owning_with_a_name_nobody_enrolled_says_so() {
        let (_home, speakers) = speakers_with(&[("ada", Role::Owner)]);

        assert!(matches!(
            speakers.make_owner(&name("bo")),
            Err(EnrollError::NotEnrolled)
        ));
        assert_eq!(roles(&speakers), [("ada".to_owned(), Role::Owner)]);
    }
}
