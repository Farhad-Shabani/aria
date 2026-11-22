//! Bounded transfer from the microphone stream to the recording.

use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::capture::modes::{CaptureProgress, CaptureRecording};
use crate::audio::{CaptureError, StopSignal};
use crate::voice_activity::VoiceActivityDetector;

use super::Result;
use super::stream::{DRAIN_BUFFER_SAMPLES, LiveStream};

const EMPTY_BUFFER_WAIT: Duration = Duration::from_millis(10);

pub(super) fn capture<V: VoiceActivityDetector>(
    live: &mut LiveStream,
    mut recording: CaptureRecording<V>,
    pre_press: &mut VecDeque<i16>,
    maximum_duration: Duration,
    stop: &StopSignal,
) -> Result<()> {
    if prepend_pre_press(&mut recording, pre_press)? == CaptureProgress::Continue {
        pump(live, &mut recording, maximum_duration, stop)?;
    }
    drain_tail(live, &mut recording)?;
    live.check_failures()?;
    recording.finish()
}

/// Writes the samples still buffered when a manual stop lands, so the last
/// words before the stop are kept.
fn drain_tail<V: VoiceActivityDetector>(
    live: &mut LiveStream,
    recording: &mut CaptureRecording<V>,
) -> Result<()> {
    let Some(limited) = recording.tail() else {
        return Ok(());
    };
    let mut scratch = [0_i16; DRAIN_BUFFER_SAMPLES];
    while let input @ [_, ..] = live.pop(&mut scratch)? {
        limited.push(input)?;
    }
    Ok(())
}

fn prepend_pre_press<V: VoiceActivityDetector>(
    recording: &mut CaptureRecording<V>,
    pre_press: &mut VecDeque<i16>,
) -> Result<CaptureProgress> {
    let progress = recording.push(pre_press.make_contiguous())?;
    pre_press.clear();
    Ok(progress)
}

fn pump<V: VoiceActivityDetector>(
    live: &mut LiveStream,
    recording: &mut CaptureRecording<V>,
    maximum_duration: Duration,
    stop: &StopSignal,
) -> Result<()> {
    let deadline = Instant::now() + maximum_duration;
    let mut scratch = [0_i16; DRAIN_BUFFER_SAMPLES];
    loop {
        let input = live.pop(&mut scratch)?;
        if recording.push(input)? == CaptureProgress::Complete {
            return Ok(());
        }
        live.check_state()?;

        if stop.should_stop() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CaptureError::DeadlineExceeded(maximum_duration));
        }
        if input.is_empty() {
            thread::park_timeout(EMPTY_BUFFER_WAIT);
        }
    }
}
