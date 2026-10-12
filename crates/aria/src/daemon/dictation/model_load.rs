//! Model readiness for eager and on-demand transcription engines.

use std::panic::{AssertUnwindSafe, catch_unwind};

use aria_kit::cancel::CancelToken;
use aria_kit::primitives::slot::Slot;
use aria_kit::transcription::{
    ChunkStream, Hypothesis, Language, Transcriber, TranscriptionAudio, TranscriptionError,
};

type Build<E> = dyn Fn(&CancelToken) -> Result<E, TranscriptionError> + Send + Sync;

pub(super) enum ModelLoad<E> {
    Loaded(E),
    Deferred(DeferredModel<E>),
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
        build: impl Fn(&CancelToken) -> Result<E, TranscriptionError> + Send + Sync + 'static,
    ) -> Self {
        Self::Deferred(DeferredModel {
            build: Box::new(build),
            model: Slot::empty(),
        })
    }

    fn get(&self, cancel: &CancelToken) -> Result<&E, TranscriptionError> {
        match self {
            Self::Loaded(value) => Ok(value),
            Self::Deferred(model) => model.get(cancel),
        }
    }
}

impl<E> DeferredModel<E> {
    fn get(&self, cancel: &CancelToken) -> Result<&E, TranscriptionError> {
        self.model.get_or_try_init(|| {
            if cancel.is_cancelled() {
                return Err(TranscriptionError::Cancelled);
            }
            catch_unwind(AssertUnwindSafe(|| (self.build)(cancel)))
                .unwrap_or(Err(TranscriptionError::LoadPanicked))
        })
    }
}

impl<E: Transcriber> Transcriber for ModelLoad<E> {
    fn validate_language(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        self.get(cancel)?.validate_language(language, cancel)
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        self.get(cancel)?.transcribe_with(audio, language, cancel)
    }

    fn open_stream(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Option<Box<dyn ChunkStream>>, TranscriptionError> {
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
                Err(TranscriptionError::NoSpeech)
            } else {
                Ok(42_u32)
            }
        });
        let first = model.get(&CancelToken::never());
        let second = model.get(&CancelToken::never());

        assert!(matches!(first, Err(TranscriptionError::NoSpeech)));
        assert_eq!(*second.expect("the retry should succeed"), 42);
    }

    #[test]
    fn request_cancellation_stops_the_load() {
        let (started, loading) = mpsc::sync_channel(1);
        let model = Arc::new(ModelLoad::<u32>::deferred(move |cancel| {
            started.send(()).expect("the test should be listening");
            while !cancel.is_cancelled() {
                thread::sleep(Duration::from_millis(5));
            }
            Err(TranscriptionError::Cancelled)
        }));
        let request = CancelToken::new();
        let waiting_request = request.clone();
        let waiting_model = Arc::clone(&model);
        let waiting = thread::spawn(move || waiting_model.get(&waiting_request).copied());
        loading.recv().expect("the load should start");

        request.cancel();
        let outcome = waiting.join().expect("the caller should finish");

        assert!(matches!(outcome, Err(TranscriptionError::Cancelled)));
    }
}
