//! Extracts unprocessed chat turns, oldest session first.
//! A fresh claim excludes other runs; failed or expired claims can be retried.

use std::path::Path;

use aria_kit::cancel::CancelToken;

use super::extract::{
    ClaudeExtractor, ConsolidateError, ConsolidationResult, Extractor, Window, build_system_prompt,
    consolidate_session,
};
use super::lexicon::load_terms;
use super::profile::write_profile;
use super::store::{Store, StoreError};
use super::{DB_FILE, LEXICON_FILE, MemoryError};
use crate::chat::AssistantError;
use crate::config::MemoryConfig;
use crate::primitives::error_chain;
use crate::settings::SettingsFile;

#[derive(Debug, Default)]
pub struct ConsolidationReport {
    pub results: Vec<ConsolidationResult>,
    pub failed: usize,
}

pub(crate) fn consolidate_backlog(
    store: &Store,
    extractor: &dyn Extractor,
    cancel: &CancelToken,
    dir: &Path,
    max_facts: u32,
) -> Result<ConsolidationReport, StoreError> {
    let terms = load_terms(&dir.join(LEXICON_FILE));
    let sessions = store.sessions()?;
    let mut report = ConsolidationReport::default();
    for (session, history_len) in sessions {
        if cancel.is_cancelled() {
            break;
        }
        let outcome = consolidate_one(
            store,
            extractor,
            cancel,
            &session,
            history_len,
            max_facts,
            &terms,
        );
        match outcome {
            Ok(Some(result)) => report.results.push(result),
            Ok(None) | Err(ConsolidateError::Extract(AssistantError::Cancelled)) => {}
            Err(error) => {
                report.failed += 1;
                tracing::warn!(session, error = %error_chain(&error), "memory session not consolidated");
            }
        }
    }
    if !cancel.is_cancelled() {
        write_profile(store, dir)?;
    }
    Ok(report)
}

/// One session: nothing when it is up to date or another run holds it.
fn consolidate_one(
    store: &Store,
    extractor: &dyn Extractor,
    cancel: &CancelToken,
    session: &str,
    history_len: u32,
    max_facts: u32,
    terms: &[String],
) -> Result<Option<ConsolidationResult>, ConsolidateError> {
    if history_len <= store.consolidated_history_len(session)? {
        return Ok(None);
    }
    let Some(start) = store.claim_consolidation(session, history_len)? else {
        return Ok(None);
    };
    let outcome = (|| {
        let turns = store.turns(session, start, history_len)?;
        let window = Window {
            session,
            turns: &turns,
            end: history_len,
        };
        consolidate_session(store, extractor, cancel, window, max_facts, terms)
    })();
    match outcome {
        Ok(result) => Ok(Some(result)),
        Err(error) => {
            store.release_consolidation(session, history_len, start)?;
            Err(error)
        }
    }
}

/// The backlog as the memory section configures it: nothing when memory is
/// off. A session that fails is logged and left for the next run; a store
/// that cannot be opened is the caller's to report.
pub(crate) fn run_backlog(
    settings: &SettingsFile,
    home: &Path,
    cancel: &CancelToken,
) -> Result<ConsolidationReport, MemoryError> {
    let config = MemoryConfig::load(&settings.memory, home);
    if !config.enabled {
        return Ok(ConsolidationReport::default());
    }
    let store = Store::open(&config.dir.join(DB_FILE))?;
    let extractor = ClaudeExtractor::new(
        config.extract_model,
        build_system_prompt(home),
        config.dir.clone(),
    );
    let report = consolidate_backlog(
        &store,
        &extractor,
        cancel,
        &config.dir,
        config.max_facts_per_session,
    )?;
    if !report.results.is_empty() {
        tracing::info!(
            sessions = report.results.len(),
            added = report
                .results
                .iter()
                .map(|result| result.added)
                .sum::<u32>(),
            pending = report
                .results
                .iter()
                .map(|result| result.pending)
                .sum::<u32>(),
            "memory consolidated"
        );
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use rusqlite::Connection;

    use super::*;
    use crate::memory::PROFILE_FILE;

    struct Scripted {
        replies: Mutex<Vec<Result<String, AssistantError>>>,
        calls: Mutex<u32>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<String, AssistantError>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().expect("lock")
        }
    }

    impl Extractor for Scripted {
        fn extract(&self, _message: &str, _cancel: &CancelToken) -> Result<String, AssistantError> {
            *self.calls.lock().expect("lock") += 1;
            let mut replies = self.replies.lock().expect("lock");
            match replies.is_empty() {
                true => Ok("[]".to_owned()),
                false => replies.remove(0),
            }
        }
    }

    fn fixture() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join(DB_FILE)).expect("opens");
        (dir, store)
    }

    fn speak(store: &Store, session: &str, text: &str) {
        store.add_turn(session, text, "sam").expect("adds");
    }

    fn backlog(store: &Store, extractor: &Scripted, dir: &Path) -> Vec<ConsolidationResult> {
        consolidate_backlog(store, extractor, &CancelToken::never(), dir, 10)
            .expect("runs")
            .results
    }

    const ONE_FACT: &str =
        r#"[{"op":"ADD","kind":"identity","text":"Is Sam","confidence":0.9,"quote":"q"}]"#;

    #[test]
    fn every_session_with_new_turns_is_consolidated_once() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        speak(&store, "b", "there");
        let extractor = Scripted::new(vec![Ok(ONE_FACT.to_owned())]);

        let results = backlog(&store, &extractor, dir.path());
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].added, 1);
        assert_eq!(extractor.calls(), 2);
        assert!(dir.path().join(PROFILE_FILE).is_file());

        assert!(backlog(&store, &extractor, dir.path()).is_empty());
        assert_eq!(extractor.calls(), 2);

        speak(&store, "a", "more");
        let results = backlog(&store, &extractor, dir.path());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].turns, 1);
    }

    #[test]
    fn an_unreadable_reply_is_retried_next_run() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        let extractor = Scripted::new(vec![Ok("nope".to_owned()), Ok(ONE_FACT.to_owned())]);

        assert!(backlog(&store, &extractor, dir.path()).is_empty());
        let results = backlog(&store, &extractor, dir.path());
        assert_eq!(results[0].added, 1);
    }

    #[test]
    fn a_failed_extraction_gives_the_window_back() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        let extractor = Scripted::new(vec![Err(AssistantError::Failed("down".to_owned()))]);

        let report = consolidate_backlog(&store, &extractor, &CancelToken::never(), dir.path(), 10)
            .expect("runs");
        assert!(report.results.is_empty());
        assert_eq!(report.failed, 1);
        assert_eq!(store.claim_consolidation("a", 1).expect("claims"), Some(0));
    }

    #[test]
    fn a_failed_session_does_not_hide_successful_sessions() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        speak(&store, "b", "hello");
        let extractor = Scripted::new(vec![
            Err(AssistantError::Failed("down".to_owned())),
            Ok(ONE_FACT.to_owned()),
        ]);

        let report = consolidate_backlog(&store, &extractor, &CancelToken::never(), dir.path(), 10)
            .expect("runs");

        assert_eq!(report.failed, 1);
        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].added, 1);
        assert_eq!(store.consolidated_history_len("a").expect("watermark"), 0);
        assert_eq!(store.consolidated_history_len("b").expect("watermark"), 1);
    }

    #[test]
    fn an_unreadable_turn_releases_its_claim() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        Connection::open(dir.path().join(DB_FILE))
            .expect("opens")
            .execute("UPDATE turns SET text = X'00'", [])
            .expect("makes the turn unreadable");
        let extractor = Scripted::new(Vec::new());

        let report = consolidate_backlog(&store, &extractor, &CancelToken::never(), dir.path(), 10)
            .expect("runs");

        assert_eq!(report.failed, 1);
        assert_eq!(extractor.calls(), 0);
        assert_eq!(store.claim_consolidation("a", 1).expect("claims"), Some(0));
    }

    #[test]
    fn a_profile_write_is_retried_without_extracting_again() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        let extractor = Scripted::new(vec![Ok(ONE_FACT.to_owned())]);
        let profile = dir.path().join(PROFILE_FILE);
        std::fs::create_dir(&profile).expect("blocks profile publication");

        assert!(matches!(
            consolidate_backlog(&store, &extractor, &CancelToken::never(), dir.path(), 10),
            Err(StoreError::Profile(_))
        ));
        assert_eq!(store.consolidated_history_len("a").expect("watermark"), 1);
        std::fs::rename(&profile, dir.path().join("blocked-profile"))
            .expect("restores profile publication");
        assert!(backlog(&store, &extractor, dir.path()).is_empty());

        assert_eq!(extractor.calls(), 1);
        assert!(
            std::fs::read_to_string(profile)
                .expect("profile")
                .contains("Is Sam")
        );
    }

    #[test]
    fn a_fresh_claim_elsewhere_is_left_alone_and_a_stale_one_taken_over() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        store.claim_consolidation("a", 1).expect("claims");
        let extractor = Scripted::new(vec![Ok(ONE_FACT.to_owned())]);

        assert!(backlog(&store, &extractor, dir.path()).is_empty());
        assert_eq!(extractor.calls(), 0);

        Connection::open(dir.path().join(DB_FILE))
            .expect("opens")
            .execute(
                "UPDATE consolidations SET claimed_at = \
                 strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime', '-601 seconds')",
                [],
            )
            .expect("ages the claim");
        let results = backlog(&store, &extractor, dir.path());
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].added, 1);
        assert_eq!(extractor.calls(), 1);
    }

    #[test]
    fn a_cancelled_run_stops_before_the_next_session() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        let extractor = Scripted::new(Vec::new());
        let cancel = CancelToken::from_fn(|| true);
        assert!(
            consolidate_backlog(&store, &extractor, &cancel, dir.path(), 10)
                .expect("runs")
                .results
                .is_empty()
        );
        assert_eq!(extractor.calls(), 0);
        assert_eq!(store.claim_consolidation("a", 1).expect("claims"), Some(0));
    }

    #[test]
    fn memory_off_runs_nothing() {
        let home = tempfile::tempdir().expect("temp home");
        assert!(
            run_backlog(
                &crate::settings::SettingsFile::default(),
                home.path(),
                &CancelToken::never()
            )
            .expect("runs")
            .results
            .is_empty()
        );
        assert!(!home.path().join(".aria/memory").exists());
    }
}
