//! The chat folder record: the folder each named chat runs in. It lives
//! apart from the resume record, so a new assistant session, a failed resume,
//! or the resume record's cap never loses the folder.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use aria_settings::config::AssistantId;
use aria_settings::{ChatKey, ChatName};
use serde::{Deserialize, Serialize};

use super::SessionKey;
use super::sessions::{RecordError, lock, read_records, write_records};

/// Twice history's cap on asks. Every ask marks its chat used, so every
/// chat that history shows keeps its folder, with room left for chats
/// asked while history was off.
const MAX_RECORDS: usize = 1_000;

const RECORD: &str = "chat folder record";

const MAX_NAME_TRIES: usize = 1_000;

const DEFAULT_NAME_CHARS: usize = 40;

/// Leaves room under a chat name's byte limit for a numeric suffix.
const DEFAULT_NAME_BYTES: usize = 120;

pub struct ChatFolders {
    path: PathBuf,
    lock: Mutex<()>,
}

#[derive(Serialize, Deserialize)]
struct FolderRecord {
    #[serde(flatten)]
    key: SessionKey,
    workdir: PathBuf,
}

impl ChatFolders {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }

    /// The folder the chat runs in, or `None` when it has no record. It
    /// also marks the record used, so the cap drops it last.
    pub fn recall(&self, key: &SessionKey) -> Result<Option<PathBuf>, RecordError> {
        let _lock = lock(&self.lock);
        let mut records = read_records::<FolderRecord>(&self.path, RECORD)?;
        let Some(index) = records.iter().position(|record| record.key == *key) else {
            return Ok(None);
        };
        let workdir = records[index].workdir.clone();
        if index + 1 < records.len() {
            let used = records.remove(index);
            records.push(used);
            write_records(&self.path, RECORD, &records)?;
        }
        Ok(Some(workdir))
    }

    /// Records `workdir` for a chat with no folder and answers the chat's
    /// folder. A chat that already has one keeps it, so two first asks
    /// racing under one name share a folder.
    pub fn start(&self, key: SessionKey, workdir: &Path) -> Result<PathBuf, RecordError> {
        let _lock = lock(&self.lock);
        let mut records = read_records::<FolderRecord>(&self.path, RECORD)?;
        let started = match records.iter().position(|record| record.key == key) {
            Some(index) => records.remove(index),
            None => FolderRecord {
                key,
                workdir: workdir.to_path_buf(),
            },
        };
        let workdir = started.workdir.clone();
        records.push(started);
        let excess = records.len().saturating_sub(MAX_RECORDS);
        records.drain(..excess);
        write_records(&self.path, RECORD, &records)?;
        Ok(workdir)
    }

    /// Starts a chat named after `question` and answers the name. A number
    /// follows the name when another chat has it.
    pub fn start_new(
        &self,
        assistant_id: AssistantId,
        question: &str,
        workdir: &Path,
    ) -> Result<ChatName, StartError> {
        let base = default_name(question);
        let _lock = lock(&self.lock);
        let mut records = read_records::<FolderRecord>(&self.path, RECORD)?;
        let free = (1..=MAX_NAME_TRIES)
            .map(|try_number| match try_number {
                1 => base.clone(),
                n => format!("{base} {n}"),
            })
            .filter_map(|name| ChatName::new(&name).ok())
            .map(|name| ChatKey { assistant_id, name })
            .find(|chat| {
                let key = SessionKey::named(chat);
                records.iter().all(|record| record.key != key)
            })
            .ok_or(StartError::NoFreeName)?;
        records.push(FolderRecord {
            key: SessionKey::named(&free),
            workdir: workdir.to_path_buf(),
        });
        let excess = records.len().saturating_sub(MAX_RECORDS);
        records.drain(..excess);
        write_records(&self.path, RECORD, &records)?;
        Ok(free.name)
    }

    pub fn rename(&self, old: &SessionKey, new: SessionKey) -> Result<(), RenameError> {
        let _lock = lock(&self.lock);
        let mut records = read_records::<FolderRecord>(&self.path, RECORD)?;
        if records.iter().any(|record| record.key == new) {
            return Err(RenameError::Taken);
        }
        let record = records
            .iter_mut()
            .find(|record| record.key == *old)
            .ok_or(RenameError::Unknown)?;
        record.key = new;
        write_records(&self.path, RECORD, &records)?;
        Ok(())
    }

    /// Answers whether the chat had a record.
    pub fn forget(&self, key: &SessionKey) -> Result<bool, RecordError> {
        let _lock = lock(&self.lock);
        let mut records = read_records::<FolderRecord>(&self.path, RECORD)?;
        let before = records.len();
        records.retain(|record| record.key != *key);
        write_records(&self.path, RECORD, &records)?;
        Ok(records.len() < before)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("every name for the new chat is taken")]
    NoFreeName,
    #[error(transparent)]
    Record(#[from] RecordError),
}

#[derive(Debug, thiserror::Error)]
pub enum RenameError {
    #[error("another chat of that assistant already has the name")]
    Taken,
    #[error("no chat has that name yet")]
    Unknown,
    #[error(transparent)]
    Record(#[from] RecordError),
}

/// The first line of `question` with its spaces collapsed, cut at a word, and
/// never starting with `/`, which names a folder's own chat.
fn default_name(question: &str) -> String {
    let line = question
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let words = line
        .split_whitespace()
        .map(|word| word.chars().filter(|c| !c.is_control()).collect::<String>())
        .filter(|word| !word.is_empty());
    let mut name = String::new();
    for word in words {
        let joined = name.chars().count() + usize::from(!name.is_empty()) + word.chars().count();
        if joined > DEFAULT_NAME_CHARS {
            if name.is_empty() {
                name = word.chars().take(DEFAULT_NAME_CHARS).collect();
            }
            break;
        }
        if !name.is_empty() {
            name.push(' ');
        }
        name.push_str(&word);
    }
    while name.len() > DEFAULT_NAME_BYTES {
        name.pop();
    }
    let name = name.trim_start_matches('/').trim();
    if name.is_empty() { "Chat" } else { name }.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: usize) -> SessionKey {
        SessionKey {
            assistant_id: "claude-code".to_owned(),
            name: name.to_string(),
        }
    }

    #[test]
    fn the_record_drops_the_chat_used_longest_ago() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chat-folders.json");
        let full = (0..MAX_RECORDS)
            .map(|name| FolderRecord {
                key: key(name),
                workdir: PathBuf::from(format!("/work/{name}")),
            })
            .collect::<Vec<_>>();
        write_records(&path, RECORD, &full).unwrap();
        let store = ChatFolders::open(&path);

        store.recall(&key(0)).unwrap();
        store
            .start(key(MAX_RECORDS), Path::new("/work/newest"))
            .unwrap();

        assert_eq!(
            store.recall(&key(0)).unwrap(),
            Some(PathBuf::from("/work/0"))
        );
        assert_eq!(store.recall(&key(1)).unwrap(), None);
        assert_eq!(
            read_records::<FolderRecord>(&path, RECORD).unwrap().len(),
            MAX_RECORDS
        );
    }

    fn opened() -> (tempfile::TempDir, ChatFolders) {
        let directory = tempfile::tempdir().unwrap();
        let store = ChatFolders::open(directory.path().join("chat-folders.json"));
        (directory, store)
    }

    fn named(name: &str) -> SessionKey {
        SessionKey::named(&ChatKey {
            assistant_id: AssistantId::ClaudeCode,
            name: ChatName::new(name).unwrap(),
        })
    }

    #[test]
    fn a_renamed_chat_keeps_its_folder_under_the_new_name_only() {
        let (_directory, store) = opened();
        store.start(named("trip"), Path::new("/work/trip")).unwrap();
        store
            .start(named("other"), Path::new("/work/other"))
            .unwrap();

        store.rename(&named("trip"), named("Weekend")).unwrap();

        assert_eq!(
            store.recall(&named("Weekend")).unwrap().as_deref(),
            Some(Path::new("/work/trip"))
        );
        assert_eq!(store.recall(&named("trip")).unwrap(), None);
        assert!(matches!(
            store.rename(&named("Weekend"), named("other")),
            Err(RenameError::Taken)
        ));
        assert!(matches!(
            store.rename(&named("trip"), named("again")),
            Err(RenameError::Unknown)
        ));
    }

    #[test]
    fn a_new_chat_takes_its_questions_words_and_a_number_when_they_are_taken() {
        let (_directory, store) = opened();
        let start = |question| {
            store
                .start_new(AssistantId::ClaudeCode, question, Path::new("/work"))
                .unwrap()
        };

        assert_eq!(start("Plan the trip").as_str(), "Plan the trip");
        assert_eq!(start("Plan the trip").as_str(), "Plan the trip 2");
        assert_eq!(start("  plan   the trip ").as_str(), "plan the trip");
        assert_eq!(
            store.recall(&named("Plan the trip 2")).unwrap().as_deref(),
            Some(Path::new("/work"))
        );
    }

    #[test]
    fn a_default_name_is_the_first_line_cut_at_a_word() {
        assert_eq!(
            default_name("\n  When is\tlunch?\nAnd dinner?"),
            "When is lunch?"
        );
        assert_eq!(
            default_name("Could you explain how the history store migrates its schema"),
            "Could you explain how the history store"
        );
        assert_eq!(default_name(&"x".repeat(50)), "x".repeat(40));
        assert_eq!(default_name("é".repeat(70).as_str()).len(), 80);
        assert_eq!(default_name("/tmp please"), "tmp please");
        for empty in ["", "   ", "\n\n", "/"] {
            assert_eq!(default_name(empty), "Chat", "{empty:?}");
        }
    }

    #[test]
    fn a_started_chat_keeps_the_folder_it_started_in() {
        let (_directory, store) = opened();

        let first = store.start(key(1), Path::new("/work/first")).unwrap();
        let second = store.start(key(1), Path::new("/work/second")).unwrap();

        assert_eq!(first, Path::new("/work/first"));
        assert_eq!(second, Path::new("/work/first"));
        assert_eq!(
            store.recall(&key(1)).unwrap().as_deref(),
            Some(Path::new("/work/first"))
        );
    }
}
