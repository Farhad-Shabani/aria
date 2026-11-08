//! Model readiness for eager and on-demand recognition engines.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::slot::Slot;
use aria_kit::recognition::{
    ChunkStream, Hypothesis, Language, RecognitionAudio, RecognitionError, Recognizer,
};

type Build<E> = dyn Fn(&CancelToken) -> Result<E, RecognitionError> + Send + Sync;

/// Loads a deferred model ahead of the first press. A press that comes first
/// waits on the same load instead of starting its own. Once the model is in,
/// running it again returns at once, so a stopped preload can simply rerun.
pub(super) type Preload = Arc<dyn Fn(&CancelToken) -> Result<(), RecognitionError> + Send + Sync>;

pub(super) enum ModelLoad<E> {
    Loaded(E),
    /// Shared with a preload, which may still be loading it.
    Deferred(Arc<DeferredModel<E>>),
}

pub(super) struct DeferredModel<E> {
    build: Box<Build<E>>,
    model: Slot<E>,
}

impl<E> ModelLoad<E> {
    pub(super) fn loaded(value: E) -> Self {
        Self::Loaded(value)
    }

    pub(super) fn deferred(
        build: impl Fn(&CancelToken) -> Result<E, RecognitionError> + Send + Sync + 'static,
    ) -> Self {
        Self::Deferred(Arc::new(DeferredModel {
            build: Box::new(build),
            model: Slot::empty(),
        }))
    }

    pub(super) fn preload(&self) -> Option<Preload>
    where
        E: Send + Sync + 'static,
    {
        match self {
            Self::Loaded(_) => None,
            Self::Deferred(model) => {
                let model = Arc::clone(model);
                Some(Arc::new(move |cancel: &CancelToken| {
                    model.get(cancel).map(drop)
                }))
            }
        }
    }

    fn get(&self, cancel: &CancelToken) -> Result<&E, RecognitionError> {
        match self {
            Self::Loaded(value) => Ok(value),
            Self::Deferred(model) => model.get(cancel),
        }
    }
}

impl<E> DeferredModel<E> {
    fn get(&self, cancel: &CancelToken) -> Result<&E, RecognitionError> {
        // A press's own cancel and deadline still end its wait on a preload.
        let stopped = || cancel.is_cancelled().then_some(RecognitionError::Cancelled);
        self.model.get_or_try_init_unless(stopped, || {
            if cancel.is_cancelled() {
                return Err(RecognitionError::Cancelled);
            }
            catch_unwind(AssertUnwindSafe(|| (self.build)(cancel)))
                .unwrap_or(Err(RecognitionError::LoadPanicked))
        })
    }
}

impl<E: Recognizer> Recognizer for ModelLoad<E> {
    fn validate_language(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), RecognitionError> {
        self.get(cancel)?.validate_language(language, cancel)
    }

    fn recognize_with(
        &self,
        audio: RecognitionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, RecognitionError> {
        self.get(cancel)?.recognize_with(audio, language, cancel)
    }

    fn open_stream(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, RecognitionError> {
        self.get(cancel)?.open_stream(language, cancel)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_failed_load_is_retried_by_the_next_caller() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let build_attempts = Arc::clone(&attempts);
        let model = ModelLoad::deferred(move |_cancel| {
            if build_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(RecognitionError::NoSpeech)
            } else {
                Ok(42_u32)
            }
        });
        let first = model.get(&CancelToken::never());
        let second = model.get(&CancelToken::never());

        assert!(matches!(first, Err(RecognitionError::NoSpeech)));
        assert_eq!(*second.expect("the retry should succeed"), 42);
    }

    #[test]
    fn a_preload_loads_the_model_the_next_caller_uses() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let build_attempts = Arc::clone(&attempts);
        let model = ModelLoad::deferred(move |_cancel| {
            build_attempts.fetch_add(1, Ordering::SeqCst);
            Ok(7_u32)
        });
        let preload = model.preload().expect("a deferred model preloads");

        thread::spawn(move || preload(&CancelToken::never()))
            .join()
            .expect("the preload should finish")
            .expect("the preload should load");

        assert_eq!(*model.get(&CancelToken::never()).unwrap(), 7);
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn request_cancellation_stops_the_load() {
        let (started, loading) = mpsc::sync_channel(1);
        let model = Arc::new(ModelLoad::<u32>::deferred(move |cancel| {
            started.send(()).expect("the test should be listening");
            while !cancel.is_cancelled() {
                thread::sleep(Duration::from_millis(5));
            }
            Err(RecognitionError::Cancelled)
        }));
        let request = CancelToken::new();
        let waiting_request = request.clone();
        let waiting_model = Arc::clone(&model);
        let waiting = thread::spawn(move || waiting_model.get(&waiting_request).copied());
        loading.recv().expect("the load should start");

        request.cancel();
        let outcome = waiting.join().expect("the caller should finish");

        assert!(matches!(outcome, Err(RecognitionError::Cancelled)));
    }
}
