//! A paired phone's microphone, offered for as long as its app is in front.
//!
//! The phone offers and the Mac asks. While attached, the phone records
//! nothing until a dictation sends Start, and stops on Stop. Audio at any
//! other time is a protocol violation, apart from what was already in
//! flight when Stop went out. One phone is attached at a time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use aria_settings::Microphone;
use aria_settings::rpc::RpcErrorCode;

/// How long a phone may take to confirm Stop. Past it, the phone may still
/// be capturing, so its stream ends.
const STOP_DEADLINE: Duration = Duration::from_secs(2);
/// About six seconds of tenth-second chunks, far more than the capture
/// thread ever lags.
const FEED_CHUNKS: usize = 64;
/// Start and Stop alternate, so two would do.
const COMMANDS: usize = 4;

/// Whether a press would record on an attached phone under this setting.
/// The capture owns the rule, and a capture that never records on a phone
/// answers no.
pub(in crate::daemon) type AdmitsPhone = fn(&Microphone) -> bool;

/// One recording on the phone. Stop and Stopped name it, so a Stopped that
/// crossed the daemon's Stop can't end the next recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PhoneRecordingId(u64);

impl From<u64> for PhoneRecordingId {
    fn from(id: u64) -> Self {
        Self(id)
    }
}

impl From<PhoneRecordingId> for u64 {
    fn from(id: PhoneRecordingId) -> Self {
        id.0
    }
}

/// What the daemon tells the attached phone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PhoneCommand {
    Start(PhoneRecordingId),
    Stop(PhoneRecordingId),
}

/// What a phone says on its stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PhoneRequest {
    Offer,
    /// 16 kHz mono samples.
    Audio(Vec<i16>),
    /// The phone stopped capturing this recording.
    Stopped(PhoneRecordingId),
}

/// The one attached phone, shared by the stream that serves it and the
/// capture that records on it.
#[derive(Default)]
pub(in crate::daemon) struct PhoneMicrophone {
    attached: Mutex<Option<Attached>>,
    /// Numbers attachments and recordings alike, so no id repeats.
    ids: AtomicU64,
}

struct Attached {
    generation: u64,
    commands: mpsc::Sender<PhoneCommand>,
    state: State,
    /// Says why this phone stopped being the attached one.
    ended: oneshot::Sender<Detached>,
}

impl Attached {
    /// Ends this phone's stream with `why`.
    fn end(self, why: Detached) {
        // A stream that already ended has nobody left to tell.
        let _ = self.ended.send(why);
    }
}

/// Why a phone stopped being the attached one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::daemon) enum Detached {
    /// A newer phone offered.
    Replaced,
    /// It sent nothing after Start.
    Silent,
    /// It fell too far behind to take Stop.
    Behind,
    /// A press recorded on the Mac instead.
    Dismissed,
}

enum State {
    Idle,
    Recording {
        id: PhoneRecordingId,
        feed: SyncSender<Vec<i16>>,
    },
    /// Stop went out, and audio still in flight is dropped until Stopped.
    Stopping {
        id: PhoneRecordingId,
        deadline: Instant,
    },
}

/// One recording on the attached phone, as the capture reads it.
pub(super) struct PhoneRecording {
    pub(super) generation: u64,
    pub(super) id: PhoneRecordingId,
    pub(super) feed: Receiver<Vec<i16>>,
}

/// The stream's hold on being the attached phone.
pub(super) struct Attachment {
    pub(super) generation: u64,
    /// What the capture asks of the phone.
    pub(super) commands: mpsc::Receiver<PhoneCommand>,
    pub(super) ended: oneshot::Receiver<Detached>,
}

impl PhoneMicrophone {
    pub(in crate::daemon) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(super) fn is_attached(&self) -> bool {
        self.lock().is_some()
    }

    /// Asks the attached phone to record. `None` when no phone is attached
    /// or idle, or it can't be reached.
    pub(super) fn start(&self) -> Option<PhoneRecording> {
        let mut attached = self.lock();
        let phone = attached.as_mut()?;
        if !matches!(phone.state, State::Idle) {
            return None;
        }
        let id = PhoneRecordingId(self.next_id());
        phone.commands.try_send(PhoneCommand::Start(id)).ok()?;
        let (feed, samples) = sync_channel(FEED_CHUNKS);
        phone.state = State::Recording { id, feed };
        Some(PhoneRecording {
            generation: phone.generation,
            id,
            feed: samples,
        })
    }

    /// The capture of `recording` ended. Asks the phone to stop, unless it
    /// already did.
    pub(super) fn stop(&self, recording: PhoneRecordingId) {
        let attached = self.lock();
        let records = attached.as_ref().is_some_and(
            |phone| matches!(phone.state, State::Recording { id, .. } if id == recording),
        );
        if records {
            send_stop(attached, recording);
        }
    }

    /// The phone sent nothing after Start, so a press must not wait on it
    /// again. Its stream ends, and it can offer again.
    pub(super) fn abandon(&self, generation: u64) {
        let mut attached = self.lock();
        if attached
            .as_ref()
            .is_some_and(|phone| phone.generation == generation)
        {
            end_attachment(&mut attached, Detached::Silent);
        }
    }

    /// A press records on the Mac while this phone stays attached, so the
    /// phone would stay awake for nothing. Its stream ends.
    pub(super) fn dismiss(&self) {
        end_attachment(&mut self.lock(), Detached::Dismissed);
    }

    /// Makes the offering phone the attached one, ending any other.
    pub(super) fn attach(&self) -> Attachment {
        let generation = self.next_id();
        let (commands, queued) = mpsc::channel(COMMANDS);
        let (detached, ended) = oneshot::channel();
        let replaced = self.lock().replace(Attached {
            generation,
            commands,
            state: State::Idle,
            ended: detached,
        });
        if let Some(replaced) = replaced {
            tracing::info!("a newer phone offered its microphone; the earlier one detaches");
            replaced.end(Detached::Replaced);
        }
        Attachment {
            generation,
            commands: queued,
            ended,
        }
    }

    /// Dropping a recording's feed finishes it with the audio that arrived.
    pub(super) fn detach(&self, generation: u64) {
        let mut attached = self.lock();
        if attached
            .as_ref()
            .is_some_and(|phone| phone.generation == generation)
        {
            attached.take();
        }
    }

    /// When the attached phone's Stop goes unconfirmed for too long.
    pub(super) fn stop_deadline(&self, generation: u64) -> Option<Instant> {
        match self.lock().as_ref() {
            Some(Attached {
                generation: attached,
                state: State::Stopping { deadline, .. },
                ..
            }) if *attached == generation => Some(*deadline),
            _ => None,
        }
    }

    /// Takes what the phone of `generation` said, by the state it's in.
    pub(super) fn hear(
        &self,
        generation: u64,
        request: PhoneRequest,
    ) -> Result<(), PhoneMicrophoneError> {
        let mut attached = self.lock();
        let Some(phone) = attached
            .as_mut()
            .filter(|phone| phone.generation == generation)
        else {
            return Ok(());
        };
        match (request, &phone.state) {
            (PhoneRequest::Offer, _) => return Err(PhoneMicrophoneError::SecondOffer),
            (PhoneRequest::Audio(samples), State::Recording { id, feed }) => {
                // A capture that ended or fell this far behind keeps what it
                // has, and the phone is told to stop.
                if feed.try_send(samples).is_err() {
                    let id = *id;
                    send_stop(attached, id);
                }
            }
            (PhoneRequest::Audio(_), State::Idle) => {
                return Err(PhoneMicrophoneError::OutsideRecording);
            }
            // Dropping the feed finishes the recording with what arrived.
            (
                PhoneRequest::Stopped(stopped),
                State::Recording { id, .. } | State::Stopping { id, .. },
            ) if stopped == *id => {
                phone.state = State::Idle;
            }
            // Audio still in flight after Stop is dropped. A phone that
            // stopped on its own as the daemon's Stop went out confirms
            // twice, and the second Stopped names a finished recording.
            (PhoneRequest::Audio(_), State::Stopping { .. }) | (PhoneRequest::Stopped(_), _) => {}
        }
        Ok(())
    }

    fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed)
    }

    fn lock(&self) -> MutexGuard<'_, Option<Attached>> {
        self.attached.lock().unwrap_or_else(|poisoned| {
            tracing::error!("the phone microphone lock was poisoned; recovering");
            poisoned.into_inner()
        })
    }
}

/// Asks the attached phone to stop `recording`. A phone too far behind to
/// take Stop can't be trusted to stop, so its stream ends instead.
fn send_stop(mut attached: MutexGuard<'_, Option<Attached>>, recording: PhoneRecordingId) {
    let Some(phone) = attached.as_mut() else {
        return;
    };
    if phone
        .commands
        .try_send(PhoneCommand::Stop(recording))
        .is_ok()
    {
        phone.state = State::Stopping {
            id: recording,
            deadline: Instant::now() + STOP_DEADLINE,
        };
    } else {
        end_attachment(&mut attached, Detached::Behind);
    }
}

/// An ended stream stops the phone's capture, so it needs no Stop.
fn end_attachment(attached: &mut MutexGuard<'_, Option<Attached>>, why: Detached) {
    if let Some(phone) = attached.take() {
        phone.end(why);
    }
}

#[derive(Debug, thiserror::Error)]
pub(in crate::daemon) enum PhoneMicrophoneError {
    #[error("a press wouldn't record on a phone now")]
    Unsupported,

    #[error("the phone didn't open with an offer")]
    NoOffer,

    #[error("the phone offered twice on one stream")]
    SecondOffer,

    #[error("the phone sent audio outside a recording")]
    OutsideRecording,

    #[error("the phone didn't confirm Stop in time")]
    StopUnconfirmed,

    #[error("the phone's stream failed before it offered")]
    ClientGone,

    #[error("the daemon is stopping")]
    ShuttingDown,
}

impl From<&PhoneMicrophoneError> for RpcErrorCode {
    fn from(error: &PhoneMicrophoneError) -> Self {
        match error {
            PhoneMicrophoneError::Unsupported => Self::Unsupported,
            PhoneMicrophoneError::NoOffer
            | PhoneMicrophoneError::SecondOffer
            | PhoneMicrophoneError::OutsideRecording
            | PhoneMicrophoneError::StopUnconfirmed => Self::InvalidRequest,
            PhoneMicrophoneError::ClientGone => Self::Cancelled,
            PhoneMicrophoneError::ShuttingDown => Self::ShuttingDown,
        }
    }
}

#[cfg(test)]
pub(in crate::daemon) mod testing {
    use super::*;

    /// One attached phone, driven as its stream would drive it.
    pub(in crate::daemon) struct FakePhone {
        phones: Arc<PhoneMicrophone>,
        generation: u64,
        pub(in crate::daemon) commands: mpsc::Receiver<PhoneCommand>,
        ended: oneshot::Receiver<Detached>,
    }

    impl FakePhone {
        pub(in crate::daemon) fn attach(phones: &Arc<PhoneMicrophone>) -> Self {
            let Attachment {
                generation,
                commands,
                ended,
            } = phones.attach();
            Self {
                phones: Arc::clone(phones),
                generation,
                commands,
                ended,
            }
        }

        pub(in crate::daemon) fn send(
            &self,
            request: PhoneRequest,
        ) -> Result<(), PhoneMicrophoneError> {
            self.phones.hear(self.generation, request)
        }

        /// Why this phone's stream ended, `None` while it stays attached.
        pub(in crate::daemon) fn detached(&mut self) -> Option<Detached> {
            self.ended.try_recv().ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::FakePhone;
    use super::*;

    fn started(phone: &mut FakePhone) -> PhoneRecordingId {
        match phone.commands.try_recv().expect("a command") {
            PhoneCommand::Start(id) => id,
            PhoneCommand::Stop(_) => panic!("expected Start"),
        }
    }

    #[test]
    fn a_recording_starts_takes_audio_and_stops() {
        let phones = PhoneMicrophone::new();
        let mut phone = FakePhone::attach(&phones);

        let recording = phones.start().expect("an idle phone records");
        assert_eq!(started(&mut phone), recording.id);
        phone.send(PhoneRequest::Audio(vec![1, 2])).unwrap();
        assert_eq!(recording.feed.try_recv().expect("audio arrives"), [1, 2]);

        phones.stop(recording.id);
        assert_eq!(
            phone.commands.try_recv().unwrap(),
            PhoneCommand::Stop(recording.id)
        );
        // Audio already in flight when Stop went out is dropped, not a fault.
        phone.send(PhoneRequest::Audio(vec![3])).unwrap();
        phone.send(PhoneRequest::Stopped(recording.id)).unwrap();
        assert!(phones.start().is_some(), "the phone is idle again");
    }

    #[test]
    fn a_stopped_that_crossed_stop_ends_nothing() {
        let phones = PhoneMicrophone::new();
        let mut phone = FakePhone::attach(&phones);
        let first = phones.start().expect("an idle phone records");
        started(&mut phone);

        // The phone stops on its own as the capture ends, so it confirms
        // twice.
        phones.stop(first.id);
        phone.send(PhoneRequest::Stopped(first.id)).unwrap();
        let next = phones.start().expect("the first Stopped makes it idle");
        phone.send(PhoneRequest::Stopped(first.id)).unwrap();
        phone.send(PhoneRequest::Audio(vec![7])).unwrap();

        assert_eq!(next.feed.try_recv().expect("the next recording hears"), [7]);
        assert!(phones.is_attached());
    }

    #[test]
    fn a_late_stop_leaves_the_next_recording_alone() {
        let phones = PhoneMicrophone::new();
        let mut phone = FakePhone::attach(&phones);
        let first = phones.start().expect("an idle phone records");
        started(&mut phone);
        phone.send(PhoneRequest::Stopped(first.id)).unwrap();
        let next = phones.start().expect("idle again");
        started(&mut phone);

        phones.stop(first.id);

        assert!(phone.commands.try_recv().is_err(), "no Stop went out");
        phone.send(PhoneRequest::Audio(vec![5])).unwrap();
        assert_eq!(next.feed.try_recv().expect("still recording"), [5]);
    }

    #[test]
    fn audio_outside_a_recording_or_a_second_offer_is_refused() {
        let phones = PhoneMicrophone::new();
        let phone = FakePhone::attach(&phones);

        assert!(matches!(
            phone.send(PhoneRequest::Audio(vec![1])),
            Err(PhoneMicrophoneError::OutsideRecording)
        ));
        assert!(matches!(
            phone.send(PhoneRequest::Offer),
            Err(PhoneMicrophoneError::SecondOffer)
        ));
    }

    #[test]
    fn a_detached_phone_hears_why() {
        let phones = PhoneMicrophone::new();
        let mut earlier = FakePhone::attach(&phones);
        let mut newer = FakePhone::attach(&phones);
        assert_eq!(earlier.detached(), Some(Detached::Replaced));
        assert!(phones.is_attached(), "the newer phone stays attached");

        phones.dismiss();
        assert_eq!(newer.detached(), Some(Detached::Dismissed));

        let mut silent = FakePhone::attach(&phones);
        let recording = phones.start().expect("an idle phone records");
        phones.abandon(recording.generation);
        assert_eq!(silent.detached(), Some(Detached::Silent));
        assert!(!phones.is_attached());
    }
}
