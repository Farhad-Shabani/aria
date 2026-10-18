//! Extracts candidates from chat windows and validates them before storage.
//! Model-reported confidence selects automatic acceptance, review, or rejection.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use aria_kit::cancel::CancelToken;
use serde_json::{Map, Value};

use super::store::{KINDS, STATUS_ACTIVE, STATUS_PENDING, Store, StoreError, Turn};
use crate::chat::{AgentError, claude_command, parse_claude, run_cli};

/// At or above this a fact goes straight into the profile.
const AUTO_CONFIDENCE: f64 = 0.85;
/// At or above this a fact waits for `aria memory review`; below, it is
/// dropped.
const PENDING_CONFIDENCE: f64 = 0.6;
const MAX_PAIRS_PER_SESSION: u32 = 5;
/// Minimum length of a misheard token eligible for correction.
const MIN_PAIR_LEN: usize = 3;
/// Caps text retained in a fact or quote before it reaches later prompts.
const MAX_FACT_CHARS: usize = 300;
const EXTRACTION_TIMEOUT: Duration = Duration::from_secs(120);

const PROMPT_OVERRIDE: &str = ".aria/prompts/extract.md";
const DEFAULT_GUIDANCE: &str = include_str!("../../../../prompts/extract.md");

/// The half of the prompt that stays with the code: `parse_candidates` and
/// the gates read exactly this shape.
const OUTPUT_CONTRACT: &str = "\
Output: a JSON array only — no prose, no code fences. Each item:
{\"op\": \"ADD\" | \"NOOP\", \"kind\": \"identity\" | \"preference\" | \"correction\",
 \"text\": \"<fact>\", \"confidence\": <0.0-1.0>, \"quote\": \"<verbatim user words>\",
 \"duplicate_of\": <existing id or null>}
or
{\"op\": \"PAIR\", \"misheard\": \"<verbatim transcript text>\", \"correct\": \"<lexicon term>\",
 \"basis\": \"correction\" | \"observed\"}
Return [] if nothing qualifies.";

pub(crate) trait Extractor {
    /// The model's raw reply to one extraction message.
    fn extract(&self, message: &str, cancel: &CancelToken) -> Result<String, AgentError>;
}

/// `claude -p` with no tools at all: a transcript is untrusted text, and a
/// tool is what an injected instruction would reach for.
pub(crate) struct ClaudeExtractor {
    model: String,
    system_prompt: String,
    workdir: PathBuf,
}

impl ClaudeExtractor {
    pub(crate) fn new(model: String, system_prompt: String, workdir: PathBuf) -> Self {
        Self {
            model,
            system_prompt,
            workdir,
        }
    }

    fn command(&self) -> Command {
        let mut command = claude_command();
        command
            .args(["-p", "--output-format", "stream-json", "--verbose"])
            .args(["--restricted", "--tools", "", "--strict-mcp-config"])
            .args(["--system-prompt", &self.system_prompt])
            .args(["--model", &self.model])
            .current_dir(&self.workdir);
        command
    }
}

impl Extractor for ClaudeExtractor {
    fn extract(&self, message: &str, cancel: &CancelToken) -> Result<String, AgentError> {
        let output = run_cli(
            &mut self.command(),
            message,
            |_line| {},
            cancel,
            EXTRACTION_TIMEOUT,
        )?;
        let reply = output.reply(parse_claude(&output.stdout))?;
        Ok(reply.text)
    }
}

/// Uses home guidance when nonempty, otherwise bundled guidance.
/// Appends the fixed output contract in either case.
pub(crate) fn build_system_prompt(home: &Path) -> String {
    let override_text = std::fs::read_to_string(home.join(PROMPT_OVERRIDE)).unwrap_or_default();
    let guidance = match override_text.trim() {
        "" => DEFAULT_GUIDANCE,
        text => text,
    };
    format!("{}\n\n{OUTPUT_CONTRACT}", without_html_comments(guidance))
}

/// The repo's prompt carries a note to editors in an HTML comment; the
/// model has no use for it.
fn without_html_comments(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        kept.push_str(&rest[..start]);
        rest = match rest[start..].find("-->") {
            Some(end) => &rest[start + end + "-->".len()..],
            None => "",
        };
    }
    kept.push_str(rest);
    kept.trim().to_owned()
}

/// The model's array, fences stripped; `None` when it is not one, so the
/// window is retried rather than lost. Items that are not objects are
/// dropped.
pub(crate) fn parse_candidates(raw: &str) -> Option<Vec<Map<String, Value>>> {
    let text = raw.trim();
    let text = text
        .strip_prefix("```")
        .map(|rest| rest.strip_prefix("json").unwrap_or(rest))
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(text, str::trim);
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(%error, "memory extraction returned no JSON; the window is retried");
            return None;
        }
    };
    let Value::Array(items) = value else {
        tracing::warn!("memory extraction returned no array; the window is retried");
        return None;
    };
    Some(
        items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(object) => Some(object),
                _ => None,
            })
            .collect(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsolidationResult {
    pub turns: usize,
    pub candidates: usize,
    pub added: u32,
    pub noops: u32,
    pub pending: u32,
    pub pairs_added: u32,
}

/// The turns of one session past the watermark a run claimed.
pub(crate) struct Window<'a> {
    pub(crate) session: &'a str,
    pub(crate) turns: &'a [Turn],
    pub(crate) end: u32,
}

fn message(turns: &[&Turn], store: &Store, terms: &[String]) -> Result<String, StoreError> {
    let existing = store.active_facts()?;
    let facts = match existing.is_empty() {
        true => "  (none yet)".to_owned(),
        false => existing
            .iter()
            .map(|fact| format!("  {}: [{}] {}", fact.id, fact.kind, fact.text))
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let transcript = turns
        .iter()
        .map(|turn| format!("  - {}", turn.text))
        .collect::<Vec<_>>()
        .join("\n");
    let mut message =
        format!("EXISTING FACTS:\n{facts}\n\nTRANSCRIPT (user statements):\n{transcript}");
    if !terms.is_empty() {
        message.push_str("\n\nLEXICON TERMS:\n");
        message.push_str(
            &terms
                .iter()
                .map(|term| format!("  - {term}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    Ok(message)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConsolidateError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("memory extraction failed")]
    Extract(#[from] AgentError),
    /// The model's reply was not the array the contract asks for; the
    /// window is left for the next run.
    #[error("memory extraction returned output that could not be read")]
    Unreadable,
}

/// Extracts from the window and moves the session's watermark to its end.
/// Any error leaves the watermark where it was.
pub(crate) fn consolidate_session(
    store: &Store,
    extractor: &dyn Extractor,
    cancel: &CancelToken,
    window: Window<'_>,
    max_facts: u32,
    terms: &[String],
) -> Result<ConsolidationResult, ConsolidateError> {
    let Window {
        session,
        turns,
        end,
    } = window;
    let eligible: Vec<&Turn> = turns.iter().filter(|turn| !turn.text.is_empty()).collect();
    let transcript = eligible
        .iter()
        .map(|turn| turn.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut result = ConsolidationResult {
        turns: eligible.len(),
        candidates: 0,
        added: 0,
        noops: 0,
        pending: 0,
        pairs_added: 0,
    };
    let candidates = match eligible.is_empty() {
        true => Vec::new(),
        false => {
            let raw = extractor.extract(&message(&eligible, store, terms)?, cancel)?;
            parse_candidates(&raw).ok_or(ConsolidateError::Unreadable)?
        }
    };
    result.candidates = candidates.len();

    let listed: Vec<String> = terms.iter().map(|term| term.to_lowercase()).collect();
    let mut discarded = 0;
    for candidate in &candidates {
        match field(candidate, "op") {
            "NOOP" => result.noops += 1,
            "PAIR" => {
                let misheard = field(candidate, "misheard").trim();
                let correct = field(candidate, "correct").trim();
                let misheard_folded = misheard.to_lowercase();
                let grounded = !misheard.is_empty()
                    && !correct.is_empty()
                    && listed.contains(&correct.to_lowercase())
                    && misheard_folded != correct.to_lowercase()
                    && misheard.chars().count() >= MIN_PAIR_LEN
                    && transcript.contains(&misheard_folded)
                    && !listed.contains(&misheard_folded)
                    && result.pairs_added < MAX_PAIRS_PER_SESSION;
                if grounded {
                    let source = match field(candidate, "basis") {
                        "correction" => "correction",
                        _ => "llm",
                    };
                    store.add_pair(misheard, correct, source, session)?;
                    result.pairs_added += 1;
                }
            }
            "ADD" => {
                let kind = field(candidate, "kind");
                let text = one_line(field(candidate, "text"));
                let confidence = confidence(candidate);
                if !KINDS.contains(&kind) || text.is_empty() || confidence < PENDING_CONFIDENCE {
                    continue;
                }
                let goes_active = confidence >= AUTO_CONFIDENCE && result.added < max_facts;
                if !goes_active && result.pending >= max_facts {
                    discarded += 1;
                    continue;
                }
                let status = match goes_active {
                    true => STATUS_ACTIVE,
                    false => STATUS_PENDING,
                };
                let quote = one_line(field(candidate, "quote"));
                store.add_fact(&text, kind, status, confidence, &quote, session)?;
                match goes_active {
                    true => result.added += 1,
                    false => result.pending += 1,
                }
            }
            // Anything else is a guess, and a guess is not a fact.
            _ => {}
        }
    }
    if discarded > 0 {
        tracing::warn!(
            session,
            discarded,
            cap = max_facts,
            "memory extraction dropped candidates: the pending queue is full"
        );
    }
    store.record_consolidation(
        session,
        end,
        result.candidates as u32,
        result.added,
        result.noops,
        result.pending,
    )?;
    Ok(result)
}

fn field<'a>(candidate: &'a Map<String, Value>, key: &str) -> &'a str {
    candidate
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Collapses whitespace and limits text length for storage and later prompts.
fn one_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_FACT_CHARS)
        .collect()
}

fn confidence(candidate: &Map<String, Value>) -> f64 {
    let value = match candidate.get("confidence") {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    };
    match value.is_finite() {
        true => value,
        false => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Answers every call with the same text and keeps what it was asked.
    struct Scripted {
        reply: String,
        asked: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(reply: &str) -> Self {
            Self {
                reply: reply.to_owned(),
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().expect("lock").clone()
        }
    }

    impl Extractor for Scripted {
        fn extract(&self, message: &str, _cancel: &CancelToken) -> Result<String, AgentError> {
            self.asked.lock().expect("lock").push(message.to_owned());
            Ok(self.reply.clone())
        }
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(&dir.path().join("memory.db")).expect("opens");
        (dir, store)
    }

    fn spoken(text: &str) -> Turn {
        Turn {
            text: text.to_owned(),
            speaker: "sam".to_owned(),
        }
    }

    fn add(kind: &str, text: &str, confidence: &str) -> String {
        format!(
            r#"{{"op":"ADD","kind":"{kind}","text":"{text}","confidence":{confidence},"quote":"q"}}"#
        )
    }

    fn run(
        store: &Store,
        reply: &str,
        turns: &[Turn],
        end: u32,
        max_facts: u32,
        terms: &[&str],
    ) -> (Result<ConsolidationResult, ConsolidateError>, Scripted) {
        let extractor = Scripted::new(reply);
        let terms: Vec<String> = terms.iter().map(|term| (*term).to_owned()).collect();
        let window = Window {
            session: "s1",
            turns,
            end,
        };
        let result = consolidate_session(
            store,
            &extractor,
            &CancelToken::never(),
            window,
            max_facts,
            &terms,
        );
        (result, extractor)
    }

    fn ok(
        store: &Store,
        reply: &str,
        turns: &[Turn],
        max_facts: u32,
        terms: &[&str],
    ) -> (ConsolidationResult, Scripted) {
        let (result, extractor) = run(store, reply, turns, turns.len() as u32, max_facts, terms);
        (result.expect("runs"), extractor)
    }

    #[test]
    fn candidates_parse_through_fences_and_reject_non_arrays() {
        assert_eq!(
            parse_candidates("```json\n[]\n```").expect("parses").len(),
            0
        );
        assert_eq!(parse_candidates("```\n[{}]\n```").expect("parses").len(), 1);
        assert_eq!(parse_candidates("[{}, 1, \"x\"]").expect("parses").len(), 1);
        assert!(parse_candidates("not json").is_none());
        assert!(parse_candidates("{\"op\": \"ADD\"}").is_none());
    }

    #[test]
    fn confidence_routes_active_pending_or_dropped() {
        let (_dir, store) = store();
        let reply = format!(
            "[{}, {}, {}, {}, {}]",
            add("identity", "high", "0.9"),
            add("preference", "mid", "0.7"),
            add("preference", "low", "0.5"),
            add("preference", "nan", "\"NaN\""),
            add("preference", "inf", "\"inf\"")
        );
        let (result, _) = ok(&store, &reply, &[spoken("hi")], 10, &[]);
        assert_eq!((result.added, result.pending, result.candidates), (1, 1, 5));
        let active: Vec<String> = store
            .active_facts()
            .expect("reads")
            .into_iter()
            .map(|f| f.text)
            .collect();
        let pending: Vec<String> = store
            .pending_facts()
            .expect("reads")
            .into_iter()
            .map(|f| f.text)
            .collect();
        assert_eq!(active, ["high"]);
        assert_eq!(pending, ["mid"]);
        assert_eq!(store.consolidated_history_len("s1").expect("reads"), 1);
    }

    #[test]
    fn a_fact_is_one_bounded_line() {
        let (_dir, store) = store();
        let long = "x".repeat(400);
        let reply = format!(
            r#"[{{"op":"ADD","kind":"identity","text":"Is\n## Sam\t here","confidence":0.9,"quote":"{long}"}}]"#
        );
        ok(&store, &reply, &[spoken("hi")], 10, &[]);
        let fact = store.get(1).expect("reads").expect("exists");
        assert_eq!(fact.text, "Is ## Sam here");
        assert_eq!(fact.source_quote.chars().count(), MAX_FACT_CHARS);
    }

    #[test]
    fn noop_unknown_kind_and_unknown_op_store_nothing() {
        let (_dir, store) = store();
        let reply = format!(
            r#"[{{"op":"NOOP","duplicate_of":1}}, {}, {{"op":"DELETE","text":"x"}}, {}]"#,
            add("mood", "x", "0.9"),
            add("identity", "", "0.9")
        );
        let (result, _) = ok(&store, &reply, &[spoken("hi")], 10, &[]);
        assert_eq!((result.noops, result.added, result.pending), (1, 0, 0));
        assert!(store.active_facts().expect("reads").is_empty());
    }

    #[test]
    fn the_cap_overflows_to_pending_then_drops() {
        let (_dir, store) = store();
        let items: Vec<String> = (0..5)
            .map(|i| add("identity", &format!("f{i}"), "0.95"))
            .collect();
        let reply = format!("[{}]", items.join(","));
        let (result, _) = ok(&store, &reply, &[spoken("hi")], 2, &[]);
        assert_eq!((result.added, result.pending), (2, 2));
        assert_eq!(store.pending_facts().expect("reads").len(), 2);
    }

    #[test]
    fn unreadable_output_is_an_error_and_a_valid_empty_list_moves_the_watermark() {
        let (_dir, store) = store();
        let (result, _) = run(&store, "sorry", &[spoken("hi")], 1, 10, &[]);
        assert!(matches!(result, Err(ConsolidateError::Unreadable)));
        assert_eq!(store.consolidated_history_len("s1").expect("reads"), 0);

        ok(&store, "[]", &[spoken("hi")], 10, &[]);
        assert_eq!(store.consolidated_history_len("s1").expect("reads"), 1);
    }

    #[test]
    fn no_eligible_turns_skips_the_model_but_moves_the_watermark() {
        let (_dir, store) = store();
        let (result, extractor) = ok(&store, "[]", &[spoken("")], 10, &[]);
        assert!(extractor.asked().is_empty());
        assert_eq!(result.turns, 0);
        assert_eq!(store.consolidated_history_len("s1").expect("reads"), 1);
    }

    #[test]
    fn the_claimed_window_records_its_absolute_end() {
        let (_dir, store) = store();
        store
            .add_fact("Is Sam", "identity", STATUS_ACTIVE, 0.9, "", "")
            .expect("adds");
        let turns = [spoken("new")];
        let (result, extractor) = run(&store, "[]", &turns, 2, 10, &["db2"]);
        assert_eq!(result.expect("runs").turns, 1);
        let asked = extractor.asked();
        assert_eq!(
            asked[0],
            "EXISTING FACTS:\n  1: [identity] Is Sam\n\n\
             TRANSCRIPT (user statements):\n  - new\n\nLEXICON TERMS:\n  - db2"
        );
        assert_eq!(store.consolidated_history_len("s1").expect("reads"), 2);
    }

    #[test]
    fn a_pair_needs_a_listed_term_and_a_grounded_misheard_form() {
        let (_dir, store) = store();
        let pair = |misheard: &str, correct: &str, basis: &str| {
            format!(
                r#"{{"op":"PAIR","misheard":"{misheard}","correct":"{correct}","basis":"{basis}"}}"#
            )
        };
        let reply = format!(
            "[{}, {}, {}, {}, {}, {}]",
            pair("d b two", "db2", "correction"),
            pair("dee bee", "db2", "observed"),
            pair("d b two", "zx9", "observed"),
            pair("db", "db2", "observed"),
            pair("db2", "db2", "observed"),
            pair("vex", "db2", "observed"),
        );
        let turns = [spoken("deploy d b two and dee bee and db and vex")];
        let (result, _) = ok(&store, &reply, &turns, 10, &["db2", "vex"]);
        assert_eq!(result.pairs_added, 2);
        let pairs = store.list_pairs().expect("reads");
        assert_eq!(
            pairs
                .iter()
                .map(|p| (p.misheard.as_str(), p.source.as_str()))
                .collect::<Vec<_>>(),
            [("d b two", "correction"), ("dee bee", "llm")]
        );

        let (result, _) = ok(&store, &reply, &turns, 10, &[]);
        assert_eq!(result.pairs_added, 0);
    }

    #[test]
    fn pairs_are_capped_per_session() {
        let (_dir, store) = store();
        let items: Vec<String> = (0..7)
            .map(|i| {
                format!(
                    r#"{{"op":"PAIR","misheard":"form{i}","correct":"db2","basis":"observed"}}"#
                )
            })
            .collect();
        let reply = format!("[{}]", items.join(","));
        let turns = [spoken("form0 form1 form2 form3 form4 form5 form6")];
        let (result, _) = ok(&store, &reply, &turns, 10, &["db2"]);
        assert_eq!(result.pairs_added, MAX_PAIRS_PER_SESSION);
    }

    #[test]
    fn the_system_prompt_keeps_the_contract_under_an_override() {
        let home = tempfile::tempdir().expect("temp home");
        let prompt = build_system_prompt(home.path());
        assert!(prompt.starts_with("You extract durable personal facts"));
        assert!(!prompt.contains("<!--"));
        assert!(prompt.ends_with(OUTPUT_CONTRACT));

        let prompts = home.path().join(".aria/prompts");
        std::fs::create_dir_all(&prompts).expect("creates");
        std::fs::write(prompts.join("extract.md"), "  \n").expect("writes");
        assert_eq!(build_system_prompt(home.path()), prompt);

        std::fs::write(
            prompts.join("extract.md"),
            "<!-- mine -->\nRemember only names.\n<!-- and\nnothing else -->\nKeep quotes.\n",
        )
        .expect("writes");
        assert_eq!(
            build_system_prompt(home.path()),
            format!("Remember only names.\n\nKeep quotes.\n\n{OUTPUT_CONTRACT}")
        );
    }

    #[test]
    fn the_claude_command_runs_without_tools() {
        let extractor = ClaudeExtractor::new(
            "sonnet".to_owned(),
            "rules".to_owned(),
            PathBuf::from("/tmp"),
        );
        let command = extractor.command();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-p",
                "--output-format",
                "stream-json",
                "--verbose",
                "--restricted",
                "--tools",
                "",
                "--strict-mcp-config",
                "--system-prompt",
                "rules",
                "--model",
                "sonnet",
            ]
        );
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp")));
    }
}
