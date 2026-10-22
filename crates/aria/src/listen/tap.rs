use std::thread::{self, JoinHandle};

use super::ListenError;
use crate::audio::{Recording, RecordingReader, RecordingWriter, Stopper};
use crate::primitives::thread::raise_scheduling_class;

#[cfg(target_os = "macos")]
mod macos;

pub(crate) struct SystemAudioCapture {
    stop: Stopper,
    worker: Option<JoinHandle<Result<(), ListenError>>>,
}

impl SystemAudioCapture {
    pub(crate) fn spawn() -> Result<(Self, RecordingReader), ListenError> {
        Self::spawn_with(run)
    }

    fn spawn_with(
        run: impl FnOnce(RecordingWriter, Stopper) -> Result<(), ListenError> + Send + 'static,
    ) -> Result<(Self, RecordingReader), ListenError> {
        let (recording, capture) = Recording::open();
        let stop = Stopper::new();
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("aria-system-audio".into())
            .spawn(move || {
                raise_scheduling_class();
                run(capture, worker_stop)
            })
            .map_err(ListenError::Spawn)?;
        let tap = Self {
            stop,
            worker: Some(worker),
        };
        Ok((tap, recording))
    }

    pub(crate) fn stopper(&self) -> Stopper {
        self.stop.clone()
    }

    pub(crate) fn finish(mut self) -> Result<(), ListenError> {
        self.join()
    }

    fn join(&mut self) -> Result<(), ListenError> {
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .map_err(|_| failure("capture worker panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stopper().stop();
        if let Err(error) = self.join() {
            tracing::warn!(%error, "system-audio capture failed during cleanup");
        }
    }
}

fn failure(detail: impl Into<String>) -> ListenError {
    ListenError::CaptureFailed {
        detail: detail.into(),
    }
}

#[cfg(target_os = "macos")]
use macos::run;

#[cfg(not(target_os = "macos"))]
fn run(_: RecordingWriter, _: Stopper) -> Result<(), ListenError> {
    Err(ListenError::UnsupportedOs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn spawn_returns_while_native_startup_is_pending() {
        let (release, pending) = mpsc::channel();
        let (returned, result) = mpsc::channel();
        let caller = thread::spawn(move || {
            let tap = SystemAudioCapture::spawn_with(move |capture, stop| {
                pending.recv().unwrap();
                assert!(stop.is_stopped());
                capture.finish();
                Ok(())
            });
            returned.send(tap).unwrap();
        });
        let early = result.recv_timeout(Duration::from_secs(1));
        if let Ok(Ok((tap, _))) = &early {
            tap.stopper().stop();
        }
        release.send(()).unwrap();
        caller.join().unwrap();
        let returned_before_ready = early.is_ok();
        let (tap, _) = early.unwrap_or_else(|_| result.recv().unwrap()).unwrap();
        tap.finish().unwrap();
        assert!(returned_before_ready, "spawn waited for native startup");
    }

    #[test]
    fn finish_preserves_a_native_startup_failure() {
        let (tap, mut recording) =
            SystemAudioCapture::spawn_with(|_, _| Err(ListenError::PermissionDenied)).unwrap();
        assert!(matches!(tap.finish(), Err(ListenError::PermissionDenied)));
        assert!(recording.fill().is_err());
    }
}
