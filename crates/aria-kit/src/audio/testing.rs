use std::path::{Path, PathBuf};

use super::wav::wav_spec;

#[cfg(test)]
pub(in crate::audio) use recordings::limited;
#[cfg(test)]
pub(crate) use recordings::{Pace, paced, recorded};
#[cfg(all(test, feature = "microphone"))]
pub(in crate::audio) use voiced::{Loudness, silence, speech};

pub(crate) const FIXTURE_SAMPLES: usize = 16_000;

/// Writes a one-second canonical WAV into `directory` and returns its path.
pub fn canonical_fixture(directory: &Path) -> PathBuf {
    write_fixture(directory, wav_spec(), FIXTURE_SAMPLES)
}

/// `canonical_fixture` with `samples` samples instead of one second.
pub fn fixture_of(directory: &Path, samples: usize) -> PathBuf {
    write_fixture(directory, wav_spec(), samples)
}

pub(crate) fn write_fixture(directory: &Path, spec: hound::WavSpec, samples: usize) -> PathBuf {
    let path = directory.join("fixture.wav");
    let mut writer = hound::WavWriter::create(&path, spec).expect("fixture must be writable");
    for index in 0..samples {
        writer
            .write_sample((index % 100) as i16)
            .expect("fixture sample must be writable");
    }
    writer.finalize().expect("fixture must finalize");
    path
}

#[cfg(all(test, feature = "microphone"))]
mod voiced {
    use std::time::Duration;

    use super::super::canonical_samples;
    use crate::voice_activity::{VoiceActivityDetector, VoiceActivityError};

    pub(in crate::audio) struct Loudness;

    impl VoiceActivityDetector for Loudness {
        const FRAME_SAMPLES: usize = 160;

        fn probability(&mut self, frame: &[i16]) -> Result<f32, VoiceActivityError> {
            Ok(if frame.iter().any(|&sample| sample != 0) {
                1.0
            } else {
                0.0
            })
        }
    }

    pub(in crate::audio) fn speech(duration: Duration) -> Vec<i16> {
        vec![1_000; canonical_samples(duration) as usize]
    }

    pub(in crate::audio) fn silence(duration: Duration) -> Vec<i16> {
        vec![0; canonical_samples(duration) as usize]
    }
}

#[cfg(test)]
mod recordings {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::super::limit::LimitedWriter;
    use super::super::recording::ends;
    use super::super::{CaptureError, RecordingReader, RecordingWriter};

    /// One recording written the way capture writes it, closed before it returns.
    pub(crate) fn recorded(samples: &[i16]) -> RecordingReader {
        let (writer, mut limited, recording) = limited();
        writer.begin(super::super::RecordingSource::Prerecorded);
        limited
            .write_samples(samples)
            .expect("samples are writable");
        writer.close(limited.finish().map_err(CaptureError::from));
        recording
    }

    /// One recording whose writing thread waits for [`Pace::advance`] before
    /// each batch, so a reader observes exactly one batch per permit.
    pub(crate) fn paced(batches: Vec<Vec<i16>>) -> (RecordingReader, Pace) {
        let (writer, mut limited, recording) = limited();
        let (advance, wait) = std::sync::mpsc::channel();
        let (acknowledge, written) = std::sync::mpsc::channel();
        advance.send(()).expect("the first permit is delivered");
        std::thread::spawn(move || {
            let count = batches.len();
            for (index, batch) in batches.into_iter().enumerate() {
                if wait.recv().is_err() {
                    break;
                }
                limited.write_samples(&batch).expect("samples are writable");
                if index + 1 < count {
                    let _ = acknowledge.send(());
                }
            }
            writer.close(limited.finish().map_err(CaptureError::from));
            // The final permit includes EOF, so decoding cannot race closure.
            let _ = acknowledge.send(());
        });
        written.recv().expect("the first batch is written");
        (
            recording,
            Pace {
                advance,
                written: Mutex::new(written),
            },
        )
    }

    /// A permit that releases the next batch of a [`paced`] recording.
    pub(crate) struct Pace {
        advance: std::sync::mpsc::Sender<()>,
        written: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Pace {
        pub(crate) fn advance(&self) {
            if self.advance.send(()).is_ok() {
                let _ = self.written.lock().expect("no poison").recv();
            }
        }
    }

    pub(in crate::audio) fn limited() -> (RecordingWriter, LimitedWriter, RecordingReader) {
        let (recording, writer) = ends();
        let limited =
            LimitedWriter::new(Duration::from_secs(15 * 60), &writer).expect("the limit is valid");
        (writer, limited, recording)
    }
}
