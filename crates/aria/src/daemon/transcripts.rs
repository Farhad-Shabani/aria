use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::audio::RecordingReader;
use crate::delivery::Caret;
use crate::dictation::{
    DictationError, DictationSettings, ExecutionControl, TranscriptSink, Utterance,
};
use crate::history::{
    AudioAvailability, ClearHistoryResult, HistoryEntry, HistoryError, HistoryPage, HistoryQuery,
    HistorySource, HistoryStatus,
};
use crate::rpc::{RpcErrorCode, RpcFailure};

pub(in crate::daemon) mod storage;

pub(super) struct TranscriptStore {
    storage: Option<Arc<storage::Store>>,
    warning: Mutex<Option<String>>,
}

impl TranscriptStore {
    pub(super) fn open(directory: &Path) -> Self {
        let opened = directory
            .parent()
            .ok_or(HistoryError::Storage)
            .and_then(|parent| {
                crate::primitives::private_fs::prepare_directory(parent)
                    .map_err(|_| HistoryError::Storage)?;
                storage::Store::open(directory)
            });
        match opened {
            Ok(storage) => Self {
                storage: Some(storage),
                warning: Mutex::new(None),
            },
            Err(error) => Self {
                storage: None,
                warning: Mutex::new(Some(error.to_string())),
            },
        }
    }

    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self {
            storage: None,
            warning: Mutex::new(None),
        }
    }

    pub(super) fn storage(&self) -> Result<Arc<storage::Store>, HistoryError> {
        self.storage.clone().ok_or(HistoryError::Unavailable)
    }

    pub(super) fn list(&self, query: HistoryQuery) -> Result<HistoryPage, HistoryError> {
        let mut page = self.storage()?.list(&query)?;
        // Reporting a saved warning consumes it, so one failure does not pin a
        // banner over whatever the store later finds on its own.
        if let Some(message) = self
            .warning
            .lock()
            .map_err(|_| HistoryError::Storage)?
            .take()
        {
            page.warning = Some(message);
        }
        Ok(page)
    }

    pub(super) fn warn(&self, message: String) {
        if let Ok(mut warning) = self.warning.lock() {
            *warning = Some(message);
        }
    }

    pub(super) fn get(&self, id: u64) -> Result<HistoryEntry, HistoryError> {
        self.storage()?.get(id)
    }

    pub(super) fn delete(&self, id: u64) -> Result<(), HistoryError> {
        self.storage()?.delete(id).map(|_| ())
    }

    pub(super) fn clear(&self) -> Result<ClearHistoryResult, HistoryError> {
        self.storage()?.clear()
    }

    pub(super) fn maintenance(&self) -> Result<(), HistoryError> {
        self.storage()?.maintenance()
    }

    pub(super) fn save(&self, entry: HistoryEntry, audio: Option<&Path>) {
        let warning = match self.storage().and_then(|store| store.insert(entry, audio)) {
            Ok(entry) => entry.warning,
            Err(error) => {
                tracing::warn!(%error, "dictation history could not be saved");
                Some(format!(
                    "A dictation could not be saved to history: {error}"
                ))
            }
        };
        if let Some(message) = warning {
            self.warn(message);
        }
    }
}

pub(super) struct History {
    store: Arc<TranscriptStore>,
    engine: &'static str,
    live: Option<HistoryEntry>,
    oversized: bool,
    retain_audio: bool,
}

impl History {
    pub(super) fn new(store: Arc<TranscriptStore>, engine: &'static str) -> Self {
        Self {
            store,
            engine,
            live: None,
            oversized: false,
            retain_audio: false,
        }
    }

    pub(super) fn start(&mut self, settings: &DictationSettings, source: HistorySource) {
        self.oversized = false;
        self.retain_audio = settings.history_keep_audio;
        self.live = (settings.history_enabled && !settings.transcript_only).then(|| HistoryEntry {
            id: 0,
            created_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            source,
            application: settings.application.clone(),
            duration_ms: None,
            text: String::new(),
            complete: false,
            status: HistoryStatus::TranscriptionFailed,
            error: None,
            warning: None,
            audio: AudioAvailability::Unavailable,
            retrying: false,
            deletion_pending: false,
            language: settings.stt_language.as_str().to_owned(),
            engine: self.engine.to_owned(),
            own_voice_only: settings.own_voice_only,
            project: settings
                .project
                .as_ref()
                .map(|path| Path::new(path.as_str()).to_path_buf()),
        });
    }
    pub(super) fn transcription_failed(&mut self, recording: &mut RecordingReader) {
        if let Some(mut entry) = self.live.take() {
            entry.error = Some(failure_from_code(RpcErrorCode::TranscriptionFailed));
            self.save_recording(recording, entry);
        }
    }

    fn save_recording(&self, recording: &mut RecordingReader, mut entry: HistoryEntry) {
        let retain_audio = self.retain_audio || entry.status == HistoryStatus::TranscriptionFailed;
        // A failed decoder may stop before capture publishes its finalized file.
        let ended = retain_audio && recording.wait_for_end(Duration::from_secs(2));
        entry.duration_ms = Some(recording.duration().as_millis() as u64);
        let audio = if ended {
            recording
                .finished_audio()
                .ok()
                .and_then(|audio| audio.path())
        } else {
            None
        };
        self.store.save(entry, audio);
    }
}

impl TranscriptSink for History {
    type Error = DictationError;

    fn dictation_started(
        &mut self,
        settings: &DictationSettings,
        _control: &Arc<ExecutionControl>,
    ) -> Result<(), DictationError> {
        self.start(settings, HistorySource::Local);
        Ok(())
    }

    fn served_by(&mut self, engine: &'static str) {
        if let Some(entry) = &mut self.live {
            entry.engine = engine.to_owned();
        }
    }

    fn segment(&mut self, utterance: Utterance<'_>) -> Result<(), DictationError> {
        if let Some(entry) = &mut self.live
            && !entry.complete
            && !self.oversized
        {
            let spaced = Caret::between(&entry.text, "").insert(utterance.text);
            if entry.text.len() + spaced.len() > storage::MAX_ENTRY_BYTES {
                self.oversized = true;
            } else {
                entry.text.push_str(&spaced);
            }
        }
        Ok(())
    }

    fn completed(&mut self, utterance: Utterance<'_>) -> Result<(), DictationError> {
        if let Some(entry) = &mut self.live {
            self.oversized = utterance.text.len() > storage::MAX_ENTRY_BYTES;
            if !self.oversized {
                entry.text = utterance.text.to_owned();
                entry.complete = true;
            }
        }
        Ok(())
    }

    fn recording_finished(
        &mut self,
        recording: &mut RecordingReader,
        result: Result<(), &DictationError>,
    ) {
        let Some(mut entry) = self.live.take() else {
            return;
        };
        if matches!(
            result,
            Err(DictationError::Cancelled | DictationError::NoSpeech)
        ) {
            return;
        }
        if self.oversized {
            self.store
                .warn("A dictation exceeded the history text limit and was not saved.".to_owned());
            return;
        }
        entry.status = match result {
            Ok(()) => HistoryStatus::Success,
            Err(_) if entry.complete => HistoryStatus::DeliveryFailed,
            Err(_) => HistoryStatus::TranscriptionFailed,
        };
        entry.error = result
            .err()
            .map(|error| failure_from_code(RpcErrorCode::from(error)));
        self.save_recording(recording, entry);
    }

    fn dictation_finished(&mut self) {
        self.live = None;
    }
}

/// A history failure with no specific text reads as its code's label.
fn failure_from_code(code: RpcErrorCode) -> RpcFailure {
    RpcFailure::new(code, code.words())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::RecordingReader;
    use crate::dictation::{DictationRequest, StopMode};
    use crate::settings::Settings;
    use crate::transcription::TranscriptionError;

    fn store(directory: &Path) -> Arc<TranscriptStore> {
        Arc::new(TranscriptStore::open(&directory.join(".aria/history")))
    }

    fn settings() -> DictationSettings {
        Settings::default().resolve_dictation(DictationRequest::new(StopMode::OnSilence), None)
    }

    fn entries(store: &TranscriptStore) -> Vec<HistoryEntry> {
        store
            .list(HistoryQuery {
                query: String::new(),
                before_id: None,
                limit: 100,
            })
            .unwrap()
            .entries
    }

    #[test]
    fn complete_text_survives_success_or_delivery_failure_and_restart() {
        for (status, retain_audio) in [
            (HistoryStatus::Success, false),
            (HistoryStatus::Success, true),
            (HistoryStatus::DeliveryFailed, false),
            (HistoryStatus::DeliveryFailed, true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let saved = store(directory.path());
            let mut sink = History::new(Arc::clone(&saved), "test");
            let mut recording =
                RecordingReader::from_samples_in(directory.path(), &[0.25; 16_000]).unwrap();
            let original = recording
                .finished_audio()
                .unwrap()
                .path()
                .unwrap()
                .to_owned();
            let mut settings = settings();
            settings.history_keep_audio = retain_audio;
            sink.dictation_started(&settings, &ExecutionControl::for_test(StopMode::Manual))
                .unwrap();
            sink.segment(Utterance::anonymous("hello")).unwrap();
            sink.segment(Utterance::anonymous("world")).unwrap();
            sink.completed(Utterance::anonymous("hello world")).unwrap();
            let error = DictationError::DeliveryTargetChanged;
            let outcome = if status == HistoryStatus::Success {
                Ok(())
            } else {
                Err(&error)
            };
            sink.recording_finished(&mut recording, outcome);
            sink.dictation_finished();
            let found = entries(&saved);
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].text, "hello world");
            assert_eq!(found[0].status, status);
            assert_eq!(
                found[0].error.as_ref().map(|error| error.code),
                (status == HistoryStatus::DeliveryFailed)
                    .then_some(RpcErrorCode::DeliveryTargetChanged)
            );
            assert!(found[0].complete);
            assert_eq!(
                matches!(found[0].audio, AudioAvailability::Available { .. }),
                retain_audio
            );
            assert_eq!(found[0].duration_ms, Some(1_000));
            let id = found[0].id;
            drop(recording);
            assert!(!original.exists());
            drop(sink);
            drop(saved);
            let reopened = store(directory.path());
            assert_eq!(reopened.get(id).unwrap().text, "hello world");
            assert_eq!(entries(&reopened).len(), 1);
            assert_eq!(
                reopened.storage().unwrap().begin_retry(id).is_ok(),
                retain_audio
            );
        }
    }

    #[test]
    fn transcription_failure_keeps_partial_text_and_owns_a_copy_of_finalized_audio() {
        let directory = tempfile::tempdir().unwrap();
        let saved = store(directory.path());
        let mut sink = History::new(Arc::clone(&saved), "test");
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.25; 16_000]).unwrap();
        let original = recording
            .finished_audio()
            .unwrap()
            .path()
            .unwrap()
            .to_owned();
        let mut settings = settings();
        settings.own_voice_only = true;
        sink.dictation_started(&settings, &ExecutionControl::for_test(StopMode::Manual))
            .unwrap();
        sink.segment(Utterance::anonymous("accepted")).unwrap();
        sink.segment(Utterance::anonymous("partial")).unwrap();
        sink.recording_finished(
            &mut recording,
            Err(&DictationError::Transcription(
                TranscriptionError::NoNativeStream,
            )),
        );
        sink.dictation_finished();
        drop(recording);
        assert!(!original.exists());
        let found = entries(&saved);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "accepted partial");
        assert!(!found[0].complete);
        assert_eq!(found[0].status, HistoryStatus::TranscriptionFailed);
        assert!(found[0].own_voice_only);
        assert!(matches!(
            found[0].audio,
            AudioAvailability::Available { .. }
        ));
        let mut lease = saved.storage().unwrap().begin_retry(found[0].id).unwrap();
        let audio = lease.recording.finished_audio().unwrap();
        assert_ne!(audio.path(), Some(original.as_path()));
    }

    #[test]
    fn partial_text_is_spaced_as_it_was_typed() {
        let directory = tempfile::tempdir().unwrap();
        let saved = store(directory.path());
        let mut sink = History::new(Arc::clone(&saved), "test");
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.25; 1_600]).unwrap();
        sink.dictation_started(&settings(), &ExecutionControl::for_test(StopMode::Manual))
            .unwrap();
        for text in ["we should", ". Ship it", "."] {
            sink.segment(Utterance::anonymous(text)).unwrap();
        }
        sink.recording_finished(&mut recording, Err(&DictationError::DeliveryTargetChanged));

        assert_eq!(entries(&saved)[0].text, "we should. Ship it.");
    }

    #[test]
    fn interrupted_stream_keeps_audio_when_delivery_fails_before_completion() {
        let directory = tempfile::tempdir().unwrap();
        let saved = store(directory.path());
        let mut sink = History::new(Arc::clone(&saved), "test");
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.25; 1_600]).unwrap();
        sink.dictation_started(&settings(), &ExecutionControl::for_test(StopMode::Manual))
            .unwrap();
        sink.segment(Utterance::anonymous("partial")).unwrap();
        sink.recording_finished(&mut recording, Err(&DictationError::DeliveryTargetChanged));
        let found = entries(&saved);
        assert_eq!(found[0].status, HistoryStatus::TranscriptionFailed);
        assert!(!found[0].complete);
        assert_eq!(found[0].text, "partial");
        assert!(matches!(
            found[0].audio,
            AudioAvailability::Available { .. }
        ));
    }

    #[test]
    fn ask_disabled_history_cancellation_and_rejected_speech_leave_no_entries() {
        let directory = tempfile::tempdir().unwrap();
        let saved = store(directory.path());
        let mut sink = History::new(Arc::clone(&saved), "test");
        for (disabled, transcript_only, failure) in [
            (true, false, None),
            (false, true, None),
            (false, false, Some(DictationError::Cancelled)),
            (false, false, Some(DictationError::NoSpeech)),
        ] {
            let mut settings = settings();
            settings.history_enabled = !disabled;
            settings.transcript_only = transcript_only;
            let mut recording =
                RecordingReader::from_samples_in(directory.path(), &[0.25; 1_600]).unwrap();
            sink.dictation_started(&settings, &ExecutionControl::for_test(StopMode::Manual))
                .unwrap();
            sink.segment(Utterance::anonymous("partial")).unwrap();
            sink.recording_finished(&mut recording, failure.as_ref().map_or(Ok(()), Err));
            sink.dictation_finished();
        }
        assert!(entries(&saved).is_empty());
        assert_eq!(
            std::fs::read_dir(directory.path().join(".aria/history/audio"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn relay_entries_share_the_store_and_name_the_served_by() {
        let directory = tempfile::tempdir().unwrap();
        let saved = store(directory.path());
        let mut sink = History::new(Arc::clone(&saved), "server");
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.25; 1_600]).unwrap();
        sink.start(&settings(), HistorySource::Relay);
        sink.served_by("parakeet");
        sink.completed(Utterance::anonymous("from relay")).unwrap();
        sink.recording_finished(&mut recording, Ok(()));
        let entry = &entries(&saved)[0];
        assert_eq!(entry.source, HistorySource::Relay);
        assert_eq!(entry.engine, "parakeet");
    }

    #[test]
    fn unavailable_storage_warns_without_failing_successful_dictation() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(".aria"), "blocked").unwrap();
        let saved = store(directory.path());
        let mut sink = History::new(Arc::clone(&saved), "test");
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.25; 1_600]).unwrap();
        sink.dictation_started(&settings(), &ExecutionControl::for_test(StopMode::Manual))
            .unwrap();
        sink.completed(Utterance::anonymous("delivered words"))
            .unwrap();
        sink.recording_finished(&mut recording, Ok(()));
        sink.dictation_finished();
        assert!(saved.warning.lock().unwrap().is_some());
        assert!(matches!(
            saved.list(HistoryQuery {
                query: String::new(),
                before_id: None,
                limit: 100
            }),
            Err(HistoryError::Unavailable)
        ));
    }

    #[test]
    fn oversized_transcript_warns_instead_of_saving_truncated_text() {
        let directory = tempfile::tempdir().unwrap();
        let saved = store(directory.path());
        let mut sink = History::new(Arc::clone(&saved), "test");
        let mut recording =
            RecordingReader::from_samples_in(directory.path(), &[0.25; 1_600]).unwrap();
        sink.dictation_started(&settings(), &ExecutionControl::for_test(StopMode::Manual))
            .unwrap();
        sink.segment(Utterance::anonymous("partial")).unwrap();
        sink.completed(Utterance::anonymous(
            &"x".repeat(storage::MAX_ENTRY_BYTES + 1),
        ))
        .unwrap();
        sink.recording_finished(&mut recording, Ok(()));
        let page = saved
            .list(HistoryQuery {
                query: String::new(),
                before_id: None,
                limit: 100,
            })
            .unwrap();
        assert!(page.entries.is_empty());
        assert!(page.warning.is_some());
    }
}
