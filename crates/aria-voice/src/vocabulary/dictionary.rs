//! Your own word list: one word per line, `#` comments. Personal rules
//! turn what a recognizer writes for these words into the words, and a
//! pair memory learns must name one of them.

use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

pub const FILE: &str = "dictionary.txt";
/// Under the home directory, beside the project vocabulary's catalog.
pub const DIRECTORY: &str = ".aria/vocabulary";

const MAX_BYTES: u64 = 1024 * 1024;
const MAX_WORDS: usize = 100_000;
const MAX_WORD_BYTES: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum DictionaryError {
    #[error("the dictionary could not be read or written")]
    Io(#[from] std::io::Error),
    #[error("the dictionary exceeds its input limit")]
    TooLarge,
    #[error("a word is one line of at most 256 bytes, and doesn't start with #")]
    InvalidWord,
    #[error("a line of the dictionary is over 256 bytes or holds a control character")]
    InvalidFile,
    /// Aria wrote nothing, so the save stays.
    #[error("the dictionary was saved while Aria edited it")]
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddWordOutcome {
    Added,
    /// The file already listed the word, so nothing changed.
    AlreadyListed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveWordOutcome {
    Removed,
    /// The file didn't list the word, so nothing changed.
    NotListed,
}

/// What any save changes: an edit in place, or a new file renamed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    len: u64,
    modified: SystemTime,
    inode: u64,
}

impl Stamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            inode: metadata.ino(),
        }
    }
}

/// What a write may land on.
enum Target {
    /// The file as read. A save since then refuses the write.
    AsRead(Stamp),
    /// Only a path nothing holds.
    Vacant,
}

struct Text {
    text: String,
    stamp: Stamp,
}

pub struct Dictionary {
    path: PathBuf,
    /// Aria's own writes. An editor saving the file never takes it.
    write: Mutex<()>,
}

impl Dictionary {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write: Mutex::new(()),
        }
    }

    pub fn from_home(home: &Path) -> Self {
        Self::new(home.join(DIRECTORY).join(FILE))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `None` when there is no file.
    pub fn stamp(&self) -> Option<Stamp> {
        std::fs::symlink_metadata(&self.path)
            .ok()
            .map(|metadata| Stamp::of(&metadata))
    }

    /// The words, or `None` when there is no file.
    pub fn words(&self) -> Result<Option<Vec<String>>, DictionaryError> {
        read(&self.path)?
            .map(|read| checked(&read.text))
            .transpose()
    }

    /// Appends `word`, trimmed, unless the file lists it already. Everything
    /// else in the file stays as written, comments included. A file with a
    /// line that isn't a word is refused, since the word would never apply.
    pub fn add(&self, word: &str) -> Result<AddWordOutcome, DictionaryError> {
        let word = word.trim();
        validate_word(word)?;
        let _write = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        let read = read(&self.path)?;
        let (mut text, words) = match &read {
            Some(read) => (read.text.clone(), checked(&read.text)?),
            None => (String::new(), Vec::new()),
        };
        if words.iter().any(|listed| listed == word) {
            return Ok(AddWordOutcome::AlreadyListed);
        }
        if words.len() >= MAX_WORDS {
            return Err(DictionaryError::TooLarge);
        }
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(word);
        text.push('\n');
        if text.len() as u64 > MAX_BYTES {
            return Err(DictionaryError::TooLarge);
        }
        let target = read.map_or(Target::Vacant, |read| Target::AsRead(read.stamp));
        self.replace(&text, target)?;
        Ok(AddWordOutcome::Added)
    }

    /// Drops every line that lists `word`, trimmed. Everything else in the
    /// file stays as written.
    pub fn remove(&self, word: &str) -> Result<RemoveWordOutcome, DictionaryError> {
        let word = word.trim();
        validate_word(word)?;
        let _write = self.write.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(read) = read(&self.path)? else {
            return Ok(RemoveWordOutcome::NotListed);
        };
        let kept: String = read
            .text
            .split_inclusive('\n')
            .filter(|line| line.trim() != word)
            .collect();
        if kept.len() == read.text.len() {
            return Ok(RemoveWordOutcome::NotListed);
        }
        self.replace(&kept, Target::AsRead(read.stamp))?;
        Ok(RemoveWordOutcome::Removed)
    }

    /// The new text lands whole, under the file's name, or not at all.
    fn replace(&self, text: &str, target: Target) -> Result<(), DictionaryError> {
        let directory = self.path.parent().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "no dictionary directory")
        })?;
        aria_primitives::private_fs::prepare_directory(directory).map_err(std::io::Error::other)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        file.write_all(text.as_bytes())?;
        file.as_file().sync_all()?;
        match target {
            Target::AsRead(stamp) => {
                // An editor takes no lock, so its save may have landed since the read.
                if self.stamp() != Some(stamp) {
                    return Err(DictionaryError::Changed);
                }
                file.persist(&self.path).map_err(|error| error.error)?;
            }
            Target::Vacant => match file.persist_noclobber(&self.path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    return Err(DictionaryError::Changed);
                }
                Err(error) => return Err(error.error.into()),
            },
        }
        std::fs::File::open(directory)?.sync_all()?;
        Ok(())
    }
}

/// The file's text, or `None` when there is none. Never follows a link.
fn read(path: &Path) -> Result<Option<Text>, DictionaryError> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the dictionary must be a regular file",
        )
        .into());
    }
    let mut text = String::new();
    file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_BYTES {
        return Err(DictionaryError::TooLarge);
    }
    Ok(Some(Text {
        text,
        stamp: Stamp::of(&metadata),
    }))
}

fn checked(text: &str) -> Result<Vec<String>, DictionaryError> {
    let words = parse(text);
    if words.len() > MAX_WORDS {
        return Err(DictionaryError::TooLarge);
    }
    if words.iter().any(|word| validate_word(word).is_err()) {
        return Err(DictionaryError::InvalidFile);
    }
    Ok(words)
}

fn validate_word(word: &str) -> Result<(), DictionaryError> {
    if word.is_empty()
        || word.starts_with('#')
        || word.len() > MAX_WORD_BYTES
        || word.chars().any(char::is_control)
    {
        return Err(DictionaryError::InvalidWord);
    }
    Ok(())
}

fn parse(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn written(text: &str) -> (tempfile::TempDir, PathBuf, Dictionary) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, text).unwrap();
        let dictionary = Dictionary::new(path.clone());
        (dir, path, dictionary)
    }

    #[test]
    fn words_skip_comments_and_blanks_and_a_missing_file() {
        let (dir, _, dictionary) = written("# mine\n\n  db2  \nvex16\n");
        assert_eq!(dictionary.words().unwrap().unwrap(), ["db2", "vex16"]);
        let missing = Dictionary::new(dir.path().join("none.txt"));
        assert!(missing.words().expect("reads").is_none());
    }

    #[test]
    fn invalid_words_fail_loudly() {
        let (_dir, path, dictionary) = written("");
        std::fs::write(&path, [255]).unwrap();
        assert!(matches!(dictionary.words(), Err(DictionaryError::Io(_))));
        std::fs::write(&path, format!("db2\n{}\n", "a".repeat(257))).unwrap();
        assert!(matches!(
            dictionary.words(),
            Err(DictionaryError::InvalidFile)
        ));
        assert!(matches!(
            dictionary.add("Adzviser"),
            Err(DictionaryError::InvalidFile)
        ));
        assert!(!std::fs::read_to_string(&path).unwrap().contains("Adzviser"));
    }

    #[test]
    fn an_add_appends_the_word_and_keeps_what_was_written() {
        let (_dir, path, dictionary) = written("# mine\ndb2");
        assert_eq!(
            dictionary.add("  Adzviser ").unwrap(),
            AddWordOutcome::Added
        );
        assert_eq!(
            dictionary.add("db2").unwrap(),
            AddWordOutcome::AlreadyListed
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\ndb2\nAdzviser\n"
        );
    }

    #[test]
    fn the_first_add_creates_a_private_file() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".aria")).unwrap();
        let dictionary = Dictionary::from_home(home.path());
        assert_eq!(dictionary.add("Adzviser").unwrap(), AddWordOutcome::Added);
        let mode = std::fs::metadata(dictionary.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(dictionary.words().unwrap().unwrap(), ["Adzviser"]);
    }

    #[test]
    fn a_bad_word_leaves_the_file_unchanged() {
        let (_dir, path, dictionary) = written("db2\n");
        for word in ["", "  ", "# note", "two\nlines", &"a".repeat(257)] {
            assert!(
                matches!(dictionary.add(word), Err(DictionaryError::InvalidWord)),
                "{word:?}"
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "db2\n");
    }

    #[test]
    fn a_remove_drops_every_copy_and_keeps_the_rest() {
        let (dir, path, dictionary) = written("# mine\ndb2\n\n  Adzviser \nvex16\nAdzviser");
        assert_eq!(
            dictionary.remove(" Adzviser").unwrap(),
            RemoveWordOutcome::Removed
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\ndb2\n\nvex16\n"
        );
        assert_eq!(
            dictionary.remove("adzviser").unwrap(),
            RemoveWordOutcome::NotListed
        );
        assert!(matches!(
            dictionary.remove("# mine"),
            Err(DictionaryError::InvalidWord)
        ));

        let missing = Dictionary::new(dir.path().join("none.txt"));
        assert_eq!(missing.remove("db2").unwrap(), RemoveWordOutcome::NotListed);
        assert!(!missing.path().exists());
    }
}
