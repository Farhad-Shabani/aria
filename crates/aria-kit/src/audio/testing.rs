use std::path::{Path, PathBuf};

use super::wav::wav_spec;

#[cfg(test)]
pub(in crate::audio) use recordings::spooled;
#[cfg(test)]
pub(crate) use recordings::{Pace, paced, recorded};

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

#[cfg(test)]
mod recordings {
    use std::sync::Mutex;
    use std::time::Duration;

    use super::super::recording::ends;
    use super::super::spool::WavSpool;
    use super::super::{CaptureError, RecordingReader, RecordingWriter};

    /// One recording written the way capture writes it, closed before it returns.
    pub(crate) fn recorded(samples: &[i16]) -> (tempfile::TempDir, RecordingReader) {
        let directory = tempfile::TempDir::new().expect("test directory should be created");
        let recording = RecordingReader::from_pcm16_in(directory.path(), samples)
            .expect("samples are writable");
        (directory, recording)
    }

    /// One recording whose writing thread waits for [`Pace::advance`] before
    /// each batch, so a reader observes exactly one batch per permit.
    pub(crate) fn paced(batches: Vec<Vec<i16>>) -> (tempfile::TempDir, RecordingReader, Pace) {
        let (directory, writer, mut spool, recording) = spooled();
        let (advance, wait) = std::sync::mpsc::channel();
        let (acknowledge, written) = std::sync::mpsc::channel();
        advance.send(()).expect("the first permit is delivered");
        std::thread::spawn(move || {
            let count = batches.len();
            for (index, batch) in batches.into_iter().enumerate() {
                if wait.recv().is_err() {
                    break;
                }
                spool.write_samples(&batch).expect("samples are writable");
                if index + 1 < count {
                    let _ = acknowledge.send(());
                }
            }
            writer.close(spool.finish().map_err(CaptureError::from));
            // The final permit includes EOF, so decoding cannot race closure.
            let _ = acknowledge.send(());
        });
        written.recv().expect("the first batch is written");
        (
            directory,
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

    pub(in crate::audio) fn spooled() -> (
        tempfile::TempDir,
        RecordingWriter,
        WavSpool,
        RecordingReader,
    ) {
        let directory = tempfile::TempDir::new().expect("test directory should be created");
        let (recording, writer) = ends();
        let spool = WavSpool::create(directory.path(), Duration::from_secs(15 * 60), &writer)
            .expect("the recording should be creatable");
        (directory, writer, spool, recording)
    }
}
