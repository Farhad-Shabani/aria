//! The supervised owner of speaker-enrollment work.
//!
//! Callers hold a [`SpeakerHandle`]; the actor runs each operation on the
//! blocking pool. Enrolling holds the microphone for as long as it takes to
//! record its samples, so it carries a cancellation token: a client that
//! disconnects gives the microphone back rather than leaving it recording.

use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::capture::CaptureBackend;
use crate::enrollment::{
    EnrollError, EnrolledSpeaker, Enroller, EnrollmentEventEmitter, EnrollmentRequest,
    EnrollmentResult, Speakers,
};
use crate::preferences::{PreferencePatch, PreferenceStore, UpdateResult};
use crate::speaker::{Role, SpeakerName};

use super::operation::{self, CallError, Pending, call, cancellable_call, operation_cancellation};
use super::tasks::Supervised;

/// A handoff slot, not a work queue.
const COMMAND_CAPACITY: usize = 1;

/// An enrollment holds the microphone, so a second one is refused as busy.
/// Two slots let a list, a remove or an owner change run beside it.
const MAX_IN_FLIGHT_JOBS: usize = 2;

enum SpeakerCommand {
    Enroll {
        request: EnrollmentRequest,
        cancellation: CancelToken,
        reply: Reply<EnrollmentResult>,
    },
    List {
        reply: Reply<Vec<EnrolledSpeaker>>,
    },
    Remove {
        name: SpeakerName,
        reply: Reply<()>,
    },
    SetOwner {
        name: SpeakerName,
        reply: Reply<()>,
    },
}

type Reply<T> = operation::Reply<T, EnrollError>;

pub(super) type SpeakerCallError = CallError<EnrollError>;

/// Enrollment and the profile directory, as the services see them.
pub(super) struct SpeakerRuntime {
    /// Absent when the speaker configuration would not load.
    speakers: Option<Arc<Speakers>>,
    /// Absent on a daemon that builds its microphone for the one press it was
    /// started to serve; there is nothing for an enrollment to record with.
    /// Listing and forgetting still work.
    enroller: Option<Enroller<CaptureBackend>>,
    events: EnrollmentEventEmitter,
    preferences: Arc<PreferenceStore>,
}

impl SpeakerRuntime {
    pub(super) fn new(
        speakers: Option<Arc<Speakers>>,
        enroller: Option<Enroller<CaptureBackend>>,
        events: EnrollmentEventEmitter,
        preferences: Arc<PreferenceStore>,
    ) -> Self {
        Self {
            speakers,
            enroller,
            events,
            preferences,
        }
    }

    fn remove(&self, name: &SpeakerName) -> Result<(), EnrollError> {
        let speakers = self.speakers()?;
        speakers.remove(name)?;
        if !speakers
            .list()
            .iter()
            .any(|speaker| speaker.role == Role::Owner)
        {
            self.release_own_voice();
        }
        Ok(())
    }

    /// With no owner left, "only type my voice" would refuse every press.
    /// The voice is already gone, so a failure here is only logged.
    fn release_own_voice(&self) {
        const ATTEMPTS: usize = 3;
        let off = PreferencePatch {
            own_voice_only: Some(false),
            ..PreferencePatch::default()
        };
        for _ in 0..ATTEMPTS {
            let snapshot = match self.preferences.snapshot() {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(error = %crate::primitives::error_chain(&error), "own-voice filtering stayed on");
                    return;
                }
            };
            if !snapshot.preferences.own_voice_only {
                return;
            }
            match self.preferences.update(snapshot.revision, off.clone()) {
                Ok(UpdateResult::Updated(_)) => {
                    tracing::info!("own-voice filtering turned off with the last owner");
                    return;
                }
                Ok(UpdateResult::Conflict(_)) => {}
                Err(error) => {
                    tracing::warn!(error = %crate::primitives::error_chain(&error), "own-voice filtering stayed on");
                    return;
                }
            }
        }
        tracing::warn!("own-voice filtering stayed on: preferences kept changing");
    }

    fn enroller(&self) -> Result<&Enroller<CaptureBackend>, EnrollError> {
        self.enroller.as_ref().ok_or(EnrollError::Unsupported)
    }

    fn speakers(&self) -> Result<&Speakers, EnrollError> {
        self.speakers.as_deref().ok_or(EnrollError::Unsupported)
    }
}

/// The application-facing entry point for enrollment. Cloneable; the channel
/// behind it is private.
#[derive(Clone)]
pub(super) struct SpeakerHandle {
    commands: mpsc::Sender<SpeakerCommand>,
}

impl SpeakerHandle {
    pub(super) async fn enroll(
        &self,
        request: EnrollmentRequest,
    ) -> Result<EnrollmentResult, SpeakerCallError> {
        let (command, pending) = cancellable_call(|cancellation, reply| SpeakerCommand::Enroll {
            request,
            cancellation,
            reply,
        });
        // The guard in `pending` cancels the enrollment if this future is
        // dropped after the command went out.
        self.submit(command, pending).await
    }

    pub(super) async fn list(&self) -> Result<Vec<EnrolledSpeaker>, SpeakerCallError> {
        let (command, pending) = call(|reply| SpeakerCommand::List { reply });
        self.submit(command, pending).await
    }

    pub(super) async fn remove(&self, name: SpeakerName) -> Result<(), SpeakerCallError> {
        let (command, pending) = call(|reply| SpeakerCommand::Remove { name, reply });
        self.submit(command, pending).await
    }

    pub(super) async fn set_owner(&self, name: SpeakerName) -> Result<(), SpeakerCallError> {
        let (command, pending) = call(|reply| SpeakerCommand::SetOwner { name, reply });
        self.submit(command, pending).await
    }

    async fn submit<T>(
        &self,
        command: SpeakerCommand,
        pending: Pending<T, EnrollError>,
    ) -> Result<T, SpeakerCallError> {
        self.commands
            .send(command)
            .await
            .map_err(|_closed| CallError::Unavailable)?;
        pending.wait().await
    }
}

pub(super) struct SpeakerActor {
    commands: mpsc::Receiver<SpeakerCommand>,
    runtime: Arc<SpeakerRuntime>,
    jobs: JoinSet<()>,
}

pub(super) fn channel(runtime: Arc<SpeakerRuntime>) -> (SpeakerHandle, SpeakerActor) {
    let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
    (
        SpeakerHandle { commands },
        SpeakerActor {
            commands: receiver,
            runtime,
            jobs: JoinSet::new(),
        },
    )
}

impl Supervised for SpeakerActor {
    /// Runs until daemon shutdown or until every handle is gone, then drains:
    /// no started job outlives this future.
    async fn run(mut self, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => break,
                Some(finished) = self.jobs.join_next() => observe(finished),
                command = self.commands.recv(),
                    if self.jobs.len() < MAX_IN_FLIGHT_JOBS => match command {
                    Some(command) => self.start(command, &shutdown),
                    None => break,
                },
            }
        }
        self.drain().await;
    }
}

impl SpeakerActor {
    fn start(&mut self, command: SpeakerCommand, shutdown: &CancellationToken) {
        let runtime = Arc::clone(&self.runtime);
        match command {
            SpeakerCommand::Enroll {
                request,
                cancellation,
                reply,
            } => {
                let cancellation = operation_cancellation(cancellation, shutdown.clone());
                reply.answer_with(&mut self.jobs, "enrollment", move || {
                    runtime
                        .enroller()?
                        .enroll(request, &cancellation, &runtime.events)
                });
            }
            SpeakerCommand::List { reply } => {
                reply.answer_with(&mut self.jobs, "speaker list", move || {
                    Ok(runtime.speakers()?.list())
                });
            }
            SpeakerCommand::Remove { name, reply } => {
                reply.answer_with(&mut self.jobs, "speaker remove", move || {
                    runtime.remove(&name)
                });
            }
            SpeakerCommand::SetOwner { name, reply } => {
                reply.answer_with(&mut self.jobs, "speaker owner change", move || {
                    runtime.speakers()?.make_owner(&name)
                });
            }
        }
    }

    async fn drain(mut self) {
        self.commands.close();
        while let Some(command) = self.commands.recv().await {
            match command {
                SpeakerCommand::Enroll { reply, .. } => reply.unavailable(),
                SpeakerCommand::List { reply } => reply.unavailable(),
                SpeakerCommand::Remove { reply, .. } | SpeakerCommand::SetOwner { reply, .. } => {
                    reply.unavailable();
                }
            }
        }
        while let Some(finished) = self.jobs.join_next().await {
            observe(finished);
        }
    }
}

fn observe(finished: Result<(), tokio::task::JoinError>) {
    if let Err(error) = finished {
        tracing::error!(%error, "a speaker job ended unexpectedly");
    }
}

#[cfg(test)]
mod tests {
    use aria_kit::speaker::enrolled_voice;

    use super::*;
    use crate::config::SpeakerConfig;
    use crate::speaker::{ProfileStore, Replace};

    /// A runtime over a speakers directory holding `voices`, with
    /// "only type my voice" on.
    fn runtime_with(voices: &[(&str, Role)]) -> (tempfile::TempDir, SpeakerRuntime) {
        let home = tempfile::tempdir().expect("a temporary directory");
        let profiles = home.path().join("speakers");
        let store = ProfileStore::open(&profiles);
        for (name, role) in voices {
            store
                .save(&enrolled_voice(name, *role, vec![1.0, 0.0]), Replace::Never)
                .expect("the profile is written");
        }
        let config = SpeakerConfig {
            model: home.path().join("absent.onnx"),
            segmentation_model: home.path().join("absent-segmentation.onnx"),
            speakers_dir: profiles,
        };
        let preferences = PreferenceStore::open(home.path().join("preferences.json"))
            .expect("the preferences open");
        let on = PreferencePatch {
            own_voice_only: Some(true),
            ..PreferencePatch::default()
        };
        preferences.update(0, on).expect("own-voice turns on");
        let runtime = SpeakerRuntime::new(
            Some(Arc::new(Speakers::open(&config))),
            None,
            EnrollmentEventEmitter::unsubscribed(),
            Arc::new(preferences),
        );
        (home, runtime)
    }

    fn own_voice_only(runtime: &SpeakerRuntime) -> bool {
        let snapshot = runtime.preferences.snapshot().expect("a snapshot");
        snapshot.preferences.own_voice_only
    }

    fn name(value: &str) -> SpeakerName {
        SpeakerName::new(value).expect("the name is usable")
    }

    #[test]
    fn removing_the_last_owner_turns_own_voice_off() {
        let (_home, runtime) = runtime_with(&[("ada", Role::Owner), ("bo", Role::Guest)]);

        runtime.remove(&name("ada")).expect("the owner is removed");

        assert!(!own_voice_only(&runtime));
    }

    #[test]
    fn removing_a_guest_keeps_own_voice_on() {
        let (_home, runtime) = runtime_with(&[("ada", Role::Owner), ("bo", Role::Guest)]);

        runtime.remove(&name("bo")).expect("the guest is removed");

        assert!(own_voice_only(&runtime));
    }
}
