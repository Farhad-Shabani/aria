#![allow(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use cidre::{cat, cf, core_audio as ca, ns, os};
use rtrb::{Producer, RingBuffer};

use super::failure;
use crate::audio::{RecordingWriter, ResampleError, Resampler, Stopper, f32_from_pcm16};
use crate::listen::ListenError;
use crate::primitives::error_chain;

const INVALID_BUFFER: u8 = 1;
const QUEUE_FULL: u8 = 2;
const CLOSED: u8 = 3;
const POLL_INTERVAL: Duration = Duration::from_millis(10);

struct Callback {
    samples: Producer<f32>,
    failed: Arc<AtomicU8>,
    stop: Stopper,
    format: SampleFormat,
}

struct Registration {
    device: std::mem::ManuallyDrop<ca::AggregateDevice>,
    context: std::mem::ManuallyDrop<Box<Callback>>,
    tap: std::mem::ManuallyDrop<ca::TapGuard>,
    callback: Option<ca::DeviceIoProcId>,
    failed: Arc<AtomicU8>,
}

impl AsRef<ca::Device> for Registration {
    fn as_ref(&self) -> &ca::Device {
        self.device.as_ref()
    }
}

impl Registration {
    fn unregister(&mut self) -> Result<(), ListenError> {
        let _ = self
            .failed
            .compare_exchange(0, CLOSED, Ordering::AcqRel, Ordering::Acquire);
        if let Some(callback) = self.callback {
            // SAFETY: Both IDs come from this owned device's registration.
            unsafe { AudioDeviceDestroyIOProcID(self.as_ref().0.0, callback) }
                .result()
                .map_err(|error| native_error("unregister audio callback", error))?;
            self.callback = None;
        }
        Ok(())
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Err(error) = self.unregister() {
            // Native code may still reference the context. Retaining its memory
            // is safer than freeing it after an unsuccessful unregistration.
            tracing::error!(%error, "retaining native capture resources after callback teardown failure");
            return;
        }
        // SAFETY: Unregistration prevents further callbacks. Each owner is
        // dropped exactly once; aggregate destruction precedes tap destruction.
        unsafe {
            std::mem::ManuallyDrop::drop(&mut self.device);
            std::mem::ManuallyDrop::drop(&mut self.context);
            std::mem::ManuallyDrop::drop(&mut self.tap);
        }
    }
}

unsafe extern "C" {
    fn AudioDeviceDestroyIOProcID(device: u32, callback: ca::DeviceIoProcId) -> os::Status;
}

#[derive(Clone, Copy)]
enum SampleFormat {
    Float32,
    Float64,
    Int16,
    Int32,
}

impl SampleFormat {
    fn from_description(format: &cat::AudioStreamBasicDesc) -> Result<Self, ListenError> {
        if format.channels_per_frame != 1
            || !format.is_native_endian()
            || format.frames_per_packet != 1
            || format.bytes_per_packet != format.bytes_per_frame
            || !format
                .format_flags
                .contains(cat::AudioFormatFlags::IS_PACKED)
        {
            return Err(failure("unsupported system-audio format"));
        }
        let float = format
            .format_flags
            .contains(cat::AudioFormatFlags::IS_FLOAT);
        let signed = format
            .format_flags
            .contains(cat::AudioFormatFlags::IS_SIGNED_INTEGER);
        match (
            float,
            signed,
            format.bits_per_channel,
            format.bytes_per_frame,
        ) {
            (true, false, 32, 4) => Ok(Self::Float32),
            (true, false, 64, 8) => Ok(Self::Float64),
            (false, true, 16, 2) => Ok(Self::Int16),
            (false, true, 32, 4) => Ok(Self::Int32),
            _ => Err(failure("unsupported system-audio sample representation")),
        }
    }

    fn width(self) -> usize {
        match self {
            Self::Int16 => 2,
            Self::Float32 | Self::Int32 => 4,
            Self::Float64 => 8,
        }
    }

    fn sample(self, bytes: &[u8]) -> f32 {
        match self {
            Self::Float32 => f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            Self::Float64 => f64::from_ne_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]) as f32,
            Self::Int16 => f32_from_pcm16(i16::from_ne_bytes([bytes[0], bytes[1]])),
            Self::Int32 => {
                i32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32
                    / 2_147_483_648.0
            }
        }
    }
}

extern "C" fn receive(
    _: ca::Device,
    _: &cat::AudioTimeStamp,
    input: &cat::AudioBufList<1>,
    _: &cat::AudioTimeStamp,
    _: &mut cat::AudioBufList<0>,
    _: &cat::AudioTimeStamp,
    context: Option<&mut Callback>,
) -> os::Status {
    if let Some(context) = context
        && context.failed.load(Ordering::Relaxed) == 0
    {
        // SAFETY: Core Audio supplies valid buffers for this callback only.
        unsafe { receive_buffer(input, context) };
    }
    os::Status::NO_ERR
}

// The caller must keep every non-null buffer readable for its declared byte
// length until this function returns.
unsafe fn receive_buffer(input: &cat::AudioBufList<1>, context: &mut Callback) {
    if context.stop.is_stopped() {
        return;
    }
    if input.number_buffers != 1 {
        context.failed.store(INVALID_BUFFER, Ordering::Release);
        return;
    }
    let buffer = &input.buffers[0];
    let bytes = buffer.data_bytes_size as usize;
    if bytes == 0 {
        return;
    }
    if buffer.number_channels != 1
        || buffer.data.is_null()
        || !bytes.is_multiple_of(context.format.width())
    {
        context.failed.store(INVALID_BUFFER, Ordering::Release);
        return;
    }
    // SAFETY: The caller guarantees Core Audio owns this byte range for the callback's duration. Reading bytes
    // avoids alignment assumptions; no reference escapes this callback.
    let data = unsafe { std::slice::from_raw_parts(buffer.data, bytes) };
    for bytes in data.chunks_exact(context.format.width()) {
        let sample = context.format.sample(bytes);
        if !sample.is_finite() {
            context.failed.store(INVALID_BUFFER, Ordering::Release);
            return;
        }
        if context.samples.push(sample).is_err() {
            context.failed.store(QUEUE_FULL, Ordering::Release);
            return;
        }
    }
}

fn native_error(operation: &str, error: os::Error) -> ListenError {
    if error == ca::hardware_err::PERMISSIONS {
        ListenError::PermissionDenied
    } else {
        failure(format!("{operation}: {error}"))
    }
}

pub(super) fn run(mut capture: RecordingWriter, stop: Stopper) -> Result<(), ListenError> {
    if stop.is_stopped() {
        capture.finish();
        return Ok(());
    }
    let version = ns::ProcessInfo::current().os_version();
    if (version.major, version.minor) < (14, 2) {
        return Err(ListenError::UnsupportedOs);
    }
    let mut description = ca::TapDesc::with_mono_global_tap_excluding_processes(&ns::Array::new());
    description.set_private(true);
    description.set_mute_behavior(ca::TapMuteBehavior::Unmuted);
    let tap = description
        .create_process_tap()
        .map_err(|e| native_error("create tap", e))?;
    let format = tap.asbd().map_err(|e| native_error("read tap format", e))?;
    if !(8_000.0..=192_000.0).contains(&format.sample_rate) || format.sample_rate.fract() != 0.0 {
        return Err(failure("unsupported system-audio sample rate"));
    }
    let sample_format = SampleFormat::from_description(&format)?;
    let mut resampler = Resampler::new(format.sample_rate as u32).map_err(resample_failure)?;
    let (producer, mut consumer) = RingBuffer::new(format.sample_rate as usize / 2);
    let failed = Arc::new(AtomicU8::new(0));
    // The callback context outlives the aggregate device, including failed start.
    let context = Box::new(Callback {
        samples: producer,
        failed: Arc::clone(&failed),
        stop: stop.clone(),
        format: sample_format,
    });
    let tap_uid = tap
        .uid()
        .map_err(|e| native_error("read tap identity", e))?;
    let sub_tap =
        cf::DictionaryOf::with_keys_values(&[ca::sub_device_keys::uid()], &[tap_uid.as_type_ref()]);
    use ca::aggregate_device_keys as keys;
    let properties = cf::DictionaryOf::with_keys_values(
        &[
            keys::is_private(),
            keys::tap_auto_start(),
            keys::name(),
            keys::uid(),
            keys::tap_list(),
        ],
        &[
            cf::Boolean::value_true().as_type_ref(),
            cf::Boolean::value_false(),
            cf::str!(c"Aria system audio"),
            &cf::Uuid::new().to_cf_string(),
            &cf::ArrayOf::from_slice(&[sub_tap.as_ref()]),
        ],
    );
    let device = ca::AggregateDevice::with_desc(&properties)
        .map_err(|e| native_error("create aggregate device", e))?;
    let mut registration = Registration {
        device: std::mem::ManuallyDrop::new(device),
        context: std::mem::ManuallyDrop::new(context),
        tap: std::mem::ManuallyDrop::new(tap),
        callback: None,
        failed: Arc::clone(&failed),
    };
    let callback = registration
        .device
        .create_io_proc_id(receive, Some(registration.context.as_mut()))
        .map_err(|e| native_error("install audio callback", e))?;
    registration.callback = Some(callback);
    if stop.is_stopped() {
        registration.unregister()?;
        drop(registration);
        capture.finish();
        return Ok(());
    }
    let device = ca::device_start(registration, Some(callback))
        .map_err(|e| native_error("start capture", e))?;
    let mut buffer = [0.0; 1_024];
    let mut checked = Instant::now();
    while !stop.is_stopped() {
        check_callback(&failed)?;
        let count = consumer.pop_partial_slice(&mut buffer).0.len();
        resampler
            .push(&buffer[..count], |block| capture.push(block))
            .map_err(resample_failure)?;
        if checked.elapsed() >= Duration::from_millis(100) {
            if device
                .tap
                .asbd()
                .map_err(|e| native_error("check tap format", e))?
                != format
            {
                return Err(failure("system-audio format changed; restart listen"));
            }
            if !device
                .device
                .is_alive()
                .map_err(|e| native_error("check capture device", e))?
            {
                return Err(failure("system-audio device disconnected"));
            }
            if !device
                .device
                .is_running()
                .map_err(|e| native_error("check capture state", e))?
            {
                return Err(failure("system-audio device stopped unexpectedly"));
            }
            checked = Instant::now();
        }
        if count == 0 {
            std::thread::sleep(POLL_INTERVAL);
        }
    }
    let mut device = device.stop().map_err(|e| native_error("stop capture", e))?;
    device.unregister()?;
    drop(device);
    check_callback(&failed)?;
    loop {
        let count = consumer.pop_partial_slice(&mut buffer).0.len();
        if count == 0 {
            break;
        }
        resampler
            .push(&buffer[..count], |block| capture.push(block))
            .map_err(resample_failure)?;
    }
    resampler
        .finish(|block| capture.push(block))
        .map_err(resample_failure)?;
    capture.finish();
    Ok(())
}

fn resample_failure(error: ResampleError) -> ListenError {
    failure(error_chain(&error))
}

fn check_callback(failed: &AtomicU8) -> Result<(), ListenError> {
    match failed.load(Ordering::Acquire) {
        0 | CLOSED => Ok(()),
        QUEUE_FULL => Err(failure("system-audio queue overflow")),
        _ => Err(failure("invalid system-audio callback buffer")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Recording;

    fn callback(capacity: usize) -> (Callback, rtrb::Consumer<f32>) {
        let (samples, consumer) = RingBuffer::new(capacity);
        (
            Callback {
                samples,
                failed: Arc::new(AtomicU8::new(0)),
                stop: Stopper::new(),
                format: SampleFormat::Float32,
            },
            consumer,
        )
    }

    #[test]
    fn callback_reports_overflow_without_losing_order() {
        let (mut context, mut consumer) = callback(2);
        let mut samples = [0.25_f32, -0.5, 0.75];
        let input = cat::AudioBufList {
            number_buffers: 1,
            buffers: [cat::AudioBuf {
                number_channels: 1,
                data_bytes_size: 12,
                data: samples.as_mut_ptr().cast(),
            }],
        };
        // SAFETY: The buffer points to the live samples array above.
        unsafe { receive_buffer(&input, &mut context) };
        assert_eq!(consumer.pop(), Ok(0.25));
        assert_eq!(consumer.pop(), Ok(-0.5));
        assert_eq!(context.failed.load(Ordering::Acquire), QUEUE_FULL);
        assert!(check_callback(&context.failed).is_err());
        context.failed.store(0, Ordering::Release);
        context.stop.stop();
        // SAFETY: The buffer still points to the live samples array above.
        unsafe { receive_buffer(&input, &mut context) };
        assert!(consumer.pop().is_err());
        assert_eq!(context.failed.load(Ordering::Acquire), 0);
    }

    #[test]
    fn callback_rejects_invalid_buffers_and_samples() {
        let (mut context, _) = callback(4);
        let mut sample = f32::NAN;
        let valid = cat::AudioBuf {
            number_channels: 1,
            data_bytes_size: 4,
            data: std::ptr::from_mut(&mut sample).cast(),
        };
        for buffer in [
            cat::AudioBuf {
                data: std::ptr::null_mut(),
                ..valid
            },
            cat::AudioBuf {
                data_bytes_size: 3,
                ..valid
            },
            cat::AudioBuf {
                number_channels: 2,
                ..valid
            },
            valid,
        ] {
            context.failed.store(0, Ordering::Release);
            let input = cat::AudioBufList {
                number_buffers: 1,
                buffers: [buffer],
            };
            // SAFETY: Non-null buffers reference sample; malformed sizes are
            // rejected before dereferencing and never exceed its allocation.
            unsafe { receive_buffer(&input, &mut context) };
            assert_eq!(context.failed.load(Ordering::Acquire), INVALID_BUFFER);
        }
    }

    #[test]
    fn format_validation_rejects_stereo_and_non_pcm() {
        let mut format = cat::AudioStreamBasicDesc::common_f32(48_000.0, 1, true);
        assert!(SampleFormat::from_description(&format).is_ok());
        format.channels_per_frame = 2;
        assert!(SampleFormat::from_description(&format).is_err());
        format.channels_per_frame = 1;
        format.bytes_per_frame = 0;
        assert!(SampleFormat::from_description(&format).is_err());
        format = cat::AudioStreamBasicDesc::common_f32(48_000.0, 1, true);
        format.format = cat::AudioFormat::default();
        assert!(SampleFormat::from_description(&format).is_err());
    }

    #[test]
    fn native_samples_convert_without_alignment_assumptions() {
        assert_eq!(SampleFormat::Float32.sample(&0.25_f32.to_ne_bytes()), 0.25);
        assert_eq!(
            SampleFormat::Float64.sample(&(-0.5_f64).to_ne_bytes()),
            -0.5
        );
        assert_eq!(SampleFormat::Int16.sample(&i16::MIN.to_ne_bytes()), -1.0);
        assert_eq!(SampleFormat::Int32.sample(&i32::MIN.to_ne_bytes()), -1.0);
    }

    #[test]
    fn cancelled_startup_finishes_without_audio() {
        let (mut recording, capture) = Recording::open();
        let stop = Stopper::new();
        stop.stop();
        run(capture, stop).unwrap();
        recording.fill().unwrap();
        assert!(recording.samples().is_empty());
    }

    #[test]
    #[ignore = "captures system audio and plays a quiet test sound"]
    fn native_capture_stops_and_releases_resources() {
        let baseline = (
            ca::System::taps().unwrap().len(),
            ca::System::devices().unwrap().len(),
        );
        for _ in 0..3 {
            let (tap, mut recording) = super::super::SystemAudioCapture::spawn().unwrap();
            wait_for_audio(&recording);
            let mut command = std::process::Command::new("/usr/bin/afplay");
            command.args(["-v", "0.05", "/System/Library/Sounds/Glass.aiff"]);
            let mut playback =
                crate::primitives::process::ChildProcess::spawn(&mut command).unwrap();
            assert!(matches!(
                playback.wait_until(Instant::now() + Duration::from_secs(5), || false),
                Ok(crate::primitives::process::WaitOutcome::Exited(status)) if status.success()
            ));
            tap.stopper().stop();
            tap.finish().unwrap();
            recording.fill().unwrap();
            assert!(recording.samples().len() > 1_600);
            assert!(recording.samples().iter().any(|x| x.abs() > 0.001));
            assert_released(baseline);
        }
        let (tap, mut recording) = super::super::SystemAudioCapture::spawn().unwrap();
        wait_for_audio(&recording);
        drop(tap);
        recording.fill().unwrap();
        assert_released(baseline);
    }

    fn wait_for_audio(recording: &crate::audio::RecordingReader) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while recording.duration().is_zero() {
            assert!(
                recording.ended_at().is_none(),
                "capture ended during startup"
            );
            assert!(Instant::now() < deadline, "capture did not produce samples");
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn assert_released(baseline: (usize, usize)) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let current = (
                ca::System::taps().unwrap().len(),
                ca::System::devices().unwrap().len(),
            );
            if current == baseline {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "native resources remain: {current:?}, baseline {baseline:?}"
            );
            std::thread::sleep(POLL_INTERVAL);
        }
    }
}
