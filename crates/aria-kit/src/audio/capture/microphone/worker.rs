//! Dedicated thread that owns microphone capture end to end.
//!
//! `cpal::Stream` is not `Send`, so a stream can only live and die on one
//! thread. The worker also owns the voice-activity model, keeping the
//! capture path free of cross-thread locking.
//!
//! The stream stays open between recordings, buffering up to the last second
//! while idle. A press usually starts with that audio in hand, so a word
//! spoken on the press keeps its start. An open stream also keeps Bluetooth
//! headsets in their mic-capable profile.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::capture::RecordingMode;
use crate::audio::recording::RecordingWriter;
use crate::audio::spool::WavSpool;
use crate::audio::{CaptureError, RecordedAudio, ResampleError, StopSignal};
use crate::primitives::thread::raise_scheduling_class;
use crate::voice_activity::VoiceActivityDetector;
use crate::wake_word::WakeWordDetector;

use super::devices::{self, Follow, InputDeviceId};
use super::stream::{DRAIN_BUFFER_SAMPLES, LiveStream};
use super::transfer::{CaptureRecording, capture};
use super::wake_word::{WakeMonitor, WakeWordEvent};
use super::{MicrophoneError, Result};

/// The sample ring holds 5 s; draining every 100ms keeps it far from full.
const IDLE_DRAIN_INTERVAL: Duration = Duration::from_millis(100);
const STREAM_REOPEN_INTERVAL: Duration = Duration::from_secs(1);
/// How often a missing chosen device is looked for while idle.
const CHOSEN_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// One second of canonical 16 kHz audio kept from before the press.
const PRE_PRESS_SAMPLES: usize = 16_000;

pub(super) enum Command {
    Record(RecordCommand),
    /// Records on this device while it's connected, else the system default.
    UseInput(Option<InputDeviceId>),
    SuspendWakeWord,
    ResumeWakeWord,
}

pub(super) struct RecordCommand {
    pub(super) mode: RecordingMode,
    pub(super) writer: RecordingWriter,
    pub(super) stop: StopSignal,
}

pub(super) fn spawn<V: VoiceActivityDetector + Send + 'static>(
    spool_dir: PathBuf,
    maximum_duration: Duration,
    input: Option<InputDeviceId>,
    vad: V,
) -> Result<Sender<Command>> {
    spawn_worker(spool_dir, maximum_duration, input, vad, Disabled)
}

pub(super) fn spawn_with_wake_word<
    V: VoiceActivityDetector + Send + 'static,
    W: WakeWordDetector + Send + 'static,
>(
    spool_dir: PathBuf,
    maximum_duration: Duration,
    input: Option<InputDeviceId>,
    vad: V,
    detector: W,
) -> Result<(Sender<Command>, Receiver<WakeWordEvent>)> {
    let (events, detections) = sync_channel(1);
    let commands = spawn_worker(
        spool_dir,
        maximum_duration,
        input,
        vad,
        WakeMonitor::new(detector, events, V::FRAME_SAMPLES),
    )?;
    Ok((commands, detections))
}

fn spawn_worker<V, I>(
    spool_dir: PathBuf,
    maximum_duration: Duration,
    input: Option<InputDeviceId>,
    vad: V,
    monitor: I,
) -> Result<Sender<Command>>
where
    V: VoiceActivityDetector + Send + 'static,
    I: IdleMonitor + Send + 'static,
{
    let (commands, requests) = channel();
    let now = Instant::now();
    let worker = Worker {
        spool_dir,
        maximum_duration,
        input,
        vad,
        monitor,
        live: None,
        pre_press: VecDeque::with_capacity(PRE_PRESS_SAMPLES + DRAIN_BUFFER_SAMPLES),
        reopen_after: now,
        check_chosen_after: now,
    };
    thread::Builder::new()
        .name("aria-microphone".to_owned())
        .spawn(move || {
            raise_scheduling_class();
            worker.serve(requests);
        })
        .map_err(MicrophoneError::SpawnWorker)
        .map_err(CaptureError::from)?;
    Ok(commands)
}

struct Worker<V, I> {
    spool_dir: PathBuf,
    maximum_duration: Duration,
    /// The chosen device. `None` follows the system default.
    input: Option<InputDeviceId>,
    vad: V,
    monitor: I,
    live: Option<LiveStream>,
    /// The newest pre-press audio; cleared whenever the stream drops.
    pre_press: VecDeque<i16>,
    reopen_after: Instant,
    check_chosen_after: Instant,
}

impl<V: VoiceActivityDetector, I: IdleMonitor> Worker<V, I> {
    // A missing microphone doesn't fail the spawn. Each recording, and the
    // wake word's held stream, opens it again.
    fn serve(mut self, requests: Receiver<Command>) {
        if let Err(error) = ensure_live(&mut self.live, self.input.as_ref()) {
            tracing::warn!(%error, "microphone is unavailable at startup; retrying");
        }
        self.serve_requests(requests);
    }

    fn serve_requests(&mut self, requests: Receiver<Command>) {
        loop {
            match requests.recv_timeout(IDLE_DRAIN_INTERVAL) {
                Ok(Command::Record(request)) => self.handle(request),
                Ok(Command::UseInput(input)) => self.use_input(input),
                Ok(Command::SuspendWakeWord) => {
                    self.vad.reset();
                    self.monitor.suspend();
                }
                Ok(Command::ResumeWakeWord) => {
                    self.pre_press.clear();
                    self.vad.reset();
                    self.monitor.resume();
                }
                Err(RecvTimeoutError::Timeout) => self.drain_idle(),
                Err(RecvTimeoutError::Disconnected) => {
                    tracing::info!("microphone worker stopped; command channel closed");
                    return;
                }
            }
        }
    }

    fn handle(&mut self, request: RecordCommand) {
        let RecordCommand { mode, writer, stop } = request;

        let result = match catch_unwind(AssertUnwindSafe(|| self.record_once(mode, &writer, &stop)))
        {
            Ok(result) => result,
            Err(_panic) => {
                self.reset_stream();
                tracing::error!("microphone worker recovered from a panic");
                Err(MicrophoneError::WorkerPanicked.into())
            }
        };
        // Drop `stop` before closing the writer. The caller's stop closure can
        // own state, such as the daemon's recording guard, that must be
        // released before a reader sees capture end.
        drop(stop);
        if let Err(error) = &result
            && invalidates_stream(error)
        {
            tracing::warn!(%error, "dropping the held microphone stream after a failed recording");
            self.reset_stream();
        }
        writer.close(result);
        self.vad.reset();
        self.monitor.recording_finished();
    }

    fn record_once(
        &mut self,
        mode: RecordingMode,
        writer: &RecordingWriter,
        stop: &StopSignal,
    ) -> Result<RecordedAudio> {
        let spool = WavSpool::create(&self.spool_dir, self.maximum_duration, writer)?;
        self.follow_input();
        let recording = CaptureRecording::new(spool, mode, &mut self.vad);
        let live = ensure_live(&mut self.live, self.input.as_ref())?;
        writer.begin();
        capture(
            live,
            recording,
            &mut self.pre_press,
            self.maximum_duration,
            stop,
        )
    }

    fn drain_idle(&mut self) {
        self.follow_input();
        self.reopen_held();
        let Self {
            live,
            pre_press,
            monitor,
            vad,
            ..
        } = self;
        let Some(live) = live else { return };
        match catch_unwind(AssertUnwindSafe(|| {
            live.drain_into(pre_press, PRE_PRESS_SAMPLES, |samples| {
                monitor.observe(vad, samples)
            })
        })) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    %error,
                    "held microphone stream failed"
                );
                self.reset_stream();
            }
            Err(_panic) => {
                tracing::error!("microphone worker recovered from a panic while idle");
                self.reset_stream();
            }
        }
    }

    fn reopen_held(&mut self) {
        if self.live.is_some()
            || !self.monitor.requires_live_stream()
            || Instant::now() < self.reopen_after
        {
            return;
        }
        self.open_held();
    }

    fn open_held(&mut self) {
        if let Err(error) = ensure_live(&mut self.live, self.input.as_ref()) {
            tracing::warn!(%error, "held microphone stream is unavailable; retrying");
            self.reopen_after = Instant::now() + STREAM_REOPEN_INTERVAL;
        }
    }

    // Applied between recordings only: one command runs at a time, so a
    // recording never changes device midway.
    fn use_input(&mut self, input: Option<InputDeviceId>) {
        if input == self.input {
            return;
        }
        self.input = input;
        self.check_chosen_after = Instant::now();
        self.follow_input();
    }

    // A move is not a failure: the new device opens now, with no backoff,
    // and stays held whatever the wake word's state, as the old stream was.
    fn follow_input(&mut self) {
        let Some(live) = &self.live else { return };
        let reopen = match devices::follow(
            self.input.as_ref(),
            live.device(),
            devices::system_default().as_ref(),
        ) {
            Follow::Stay => false,
            Follow::Reopen => true,
            Follow::CheckChosen => self.chosen_came_back(),
        };
        if reopen {
            tracing::info!("the microphone to record on changed; reopening the held stream");
            self.forget_stream();
            self.open_held();
        }
    }

    fn chosen_came_back(&mut self) -> bool {
        let now = Instant::now();
        if now < self.check_chosen_after {
            return false;
        }
        self.check_chosen_after = now + CHOSEN_CHECK_INTERVAL;
        self.input.as_ref().is_some_and(devices::is_connected)
    }

    /// Drops the held stream and the pre-press audio gathered from it, so
    /// pre-press audio cannot span a stream failure.
    fn reset_stream(&mut self) {
        self.forget_stream();
        self.reopen_after = Instant::now() + STREAM_REOPEN_INTERVAL;
    }

    fn forget_stream(&mut self) {
        self.live = None;
        self.pre_press.clear();
        self.vad.reset();
        self.monitor.reset();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IdleObservation {
    Continue,
    Detected { command_start: usize },
}

pub(super) trait IdleMonitor {
    fn observe<V: VoiceActivityDetector>(
        &mut self,
        vad: &mut V,
        samples: &[i16],
    ) -> IdleObservation;

    fn reset(&mut self);

    fn suspend(&mut self);

    fn resume(&mut self);

    fn recording_finished(&mut self);

    fn requires_live_stream(&self) -> bool;
}

struct Disabled;

impl IdleMonitor for Disabled {
    fn observe<V: VoiceActivityDetector>(
        &mut self,
        _vad: &mut V,
        _samples: &[i16],
    ) -> IdleObservation {
        IdleObservation::Continue
    }

    fn reset(&mut self) {}

    fn suspend(&mut self) {}

    fn resume(&mut self) {}

    fn recording_finished(&mut self) {}

    fn requires_live_stream(&self) -> bool {
        true
    }
}

fn ensure_live<'slot>(
    slot: &'slot mut Option<LiveStream>,
    input: Option<&InputDeviceId>,
) -> Result<&'slot mut LiveStream> {
    match slot {
        Some(live) => Ok(live),
        slot @ None => {
            let live = slot.insert(LiveStream::open(input)?);
            tracing::info!("microphone stream opened and held");
            Ok(live)
        }
    }
}

/// Whether an error indicts the held stream rather than one recording.
fn invalidates_stream(error: &CaptureError) -> bool {
    matches!(
        error,
        CaptureError::Microphone(
            MicrophoneError::Stream(_)
                | MicrophoneError::StreamEnded
                | MicrophoneError::IncompleteFrame
                | MicrophoneError::Overrun
                | MicrophoneError::Resample(ResampleError::Process(_))
        )
    )
}
