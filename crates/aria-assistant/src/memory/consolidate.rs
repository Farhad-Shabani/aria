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
use super::{DB_FILE, LEXICON_FILE};
use crate::chat::AssistantError;
use crate::memory::MemoryConfig;
use aria_primitives::error_chain;

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
    for (session, through) in sessions {
        if cancel.is_cancelled() {
            break;
        }
        let outcome = consolidate_one(
            store, extractor, cancel, &session, through, max_facts, &terms,
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
        if let Err(error) = store.prune_turns() {
            tracing::warn!(error = %error_chain(&error), "old memory asks were not pruned");
        }
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
    through: i64,
    max_facts: u32,
    terms: &[String],
) -> Result<Option<ConsolidationResult>, ConsolidateError> {
    if through <= store.consolidated_through(session)? {
        return Ok(None);
    }
    let Some(after) = store.claim_consolidation(session, through)? else {
        return Ok(None);
    };
    let outcome = (|| {
        let turns = store.turns(session, after, through)?;
        let window = Window {
            session,
            turns: &turns,
            through,
        };
        consolidate_session(store, extractor, cancel, window, max_facts, terms)
    })();
    match outcome {
        Ok(result) => Ok(Some(result)),
        Err(error) => {
            store.release_consolidation(session, through, after)?;
            Err(error)
        }
    }
}

/// The backlog as the memory section configures it: nothing when memory is
/// off. A session that fails is logged and left for the next run; a store
/// that cannot be opened is the caller's to report.
pub fn run_backlog(
    config: MemoryConfig,
    home: &Path,
    cancel: &CancelToken,
) -> Result<ConsolidationReport, StoreError> {
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

    /// Makes the run's claim stale and clears memory while the run waits on
    /// its extraction.
    struct ClearsMidway {
        path: std::path::PathBuf,
    }

    impl Extractor for ClearsMidway {
        fn extract(&self, _message: &str, _cancel: &CancelToken) -> Result<String, AssistantError> {
            Connection::open(&self.path)
                .expect("opens")
                .execute(
                    "UPDATE consolidations SET claimed_at = \
                     strftime('%Y-%m-%dT%H:%M:%S', 'now', 'localtime', '-601 seconds')",
                    [],
                )
                .expect("ages the claim");
            Store::open(&self.path)
                .expect("opens")
                .clear()
                .expect("clears");
            Ok(ONE_FACT.to_owned())
        }
    }

    #[test]
    fn a_run_whose_memory_was_cleared_writes_nothing() {
        let (dir, store) = fixture();
        speak(&store, "a", "hi");
        let extractor = ClearsMidway {
            path: dir.path().join(DB_FILE),
        };

        let report = consolidate_backlog(&store, &extractor, &CancelToken::never(), dir.path(), 10)
            .expect("runs");

        assert_eq!(report.failed, 1);
        assert!(store.active_facts().expect("reads").is_empty());
        assert_eq!(store.consolidated_through("a").expect("reads"), 0);
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
        assert_eq!(store.consolidated_through("a").expect("watermark"), 0);
        assert_eq!(store.consolidated_through("b").expect("watermark"), 2);
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
        assert_eq!(store.consolidated_through("a").expect("watermark"), 1);
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
                MemoryConfig {
                    enabled: false,
                    extract_model: "sonnet".to_owned(),
                    max_facts_per_session: 10,
                    dir: home.path().join(".aria/memory"),
                },
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
