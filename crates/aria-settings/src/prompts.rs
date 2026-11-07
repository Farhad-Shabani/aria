use std::path::{Path, PathBuf};

use super::Chord;

// Sized so a full list fits the settings file's byte cap beside a full
// list of app overrides, with every name and path escaped at worst.
const MAX_PROMPTS: usize = 16;
const MAX_ID_BYTES: usize = 64;
pub const MAX_NAME_BYTES: usize = 128;
const MAX_FILE_BYTES: usize = 512;

/// The folder under the home that holds prompt files written in Aria, and the cleanup and
/// extraction guidance files.
pub const PROMPTS_DIR: &str = ".aria/prompts";
/// Dictation cleanup reads this file in [`PROMPTS_DIR`] in place of its own
/// guidance.
pub const CLEANUP_PROMPT_FILE: &str = "cleanup.md";
/// Memory extraction reads this file in [`PROMPTS_DIR`] in place of its own
/// guidance.
pub const EXTRACT_PROMPT_FILE: &str = "extract.md";
/// A prompt saved under one of these names would change what Aria itself
/// does, so no prompt file takes one.
pub const RESERVED_PROMPT_FILES: [&str; 2] = [CLEANUP_PROMPT_FILE, EXTRACT_PROMPT_FILE];

/// What a prompt makes of what was said, which sets how the assistant is told to
/// treat the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    /// The same words and meaning, cleaned up or restyled.
    Rewrite,
    /// New text made from what was said, such as a reply or a summary.
    Compose,
}

impl PromptKind {
    /// The kind as the settings file spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rewrite => "rewrite",
            Self::Compose => "compose",
        }
    }
}

/// What `RunPrompt` names a prompt by; a rename keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptId(String);

impl PromptId {
    /// Letters, digits and '-', which covers a UUID.
    pub fn new(id: impl Into<String>) -> Result<Self, PromptError> {
        let id = id.into();
        let well_formed = !id.is_empty()
            && id.len() <= MAX_ID_BYTES
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
        if !well_formed {
            return Err(PromptError::Id);
        }
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Markdown file of instructions and the chord that runs it. The file is
/// only named here; it is read on each run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    id: PromptId,
    name: String,
    kind: PromptKind,
    file: PathBuf,
    /// Off, it runs neither from the prompt list nor from its chord.
    enabled: bool,
    /// None for a prompt run only from the prompt list.
    chord: Option<Chord>,
}

impl Prompt {
    pub fn new(
        id: PromptId,
        name: String,
        kind: PromptKind,
        file: PathBuf,
        enabled: bool,
        chord: Option<Chord>,
    ) -> Result<Self, PromptError> {
        if name.trim().is_empty() || name.len() > MAX_NAME_BYTES || has_control(&name) {
            return Err(PromptError::Name);
        }
        let file_text = file.to_str().ok_or(PromptError::File)?;
        if !file.is_absolute() || file_text.len() > MAX_FILE_BYTES || has_control(file_text) {
            return Err(PromptError::File);
        }
        Ok(Self {
            id,
            name,
            kind,
            file,
            enabled,
            chord,
        })
    }

    pub fn id(&self) -> &PromptId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> PromptKind {
        self.kind
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn chord(&self) -> Option<Chord> {
        self.chord
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Prompts(Vec<Prompt>);

impl Prompts {
    pub fn new(entries: Vec<Prompt>) -> Result<Self, PromptError> {
        if entries.len() > MAX_PROMPTS {
            return Err(PromptError::TooMany);
        }
        for (index, entry) in entries.iter().enumerate() {
            let earlier = &entries[..index];
            if earlier.iter().any(|seen| seen.id == entry.id) {
                return Err(PromptError::DuplicateId);
            }
        }
        Ok(Self(entries))
    }

    pub fn entries(&self) -> &[Prompt] {
        &self.0
    }

    pub fn find(&self, id: &str) -> Option<&Prompt> {
        self.0.iter().find(|prompt| prompt.id.as_str() == id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("a prompt ID is 1 to {MAX_ID_BYTES} letters, digits, or '-'")]
    Id,

    #[error("a prompt name is 1 to {MAX_NAME_BYTES} bytes and not blank")]
    Name,

    #[error("a prompt file is an absolute path of at most {MAX_FILE_BYTES} bytes")]
    File,

    #[error("at most {MAX_PROMPTS} prompts are stored")]
    TooMany,

    #[error("two prompts share one ID")]
    DuplicateId,
}

fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Modifiers;

    fn prompt(id: &str, chord: Option<Chord>) -> Prompt {
        Prompt::new(
            PromptId::new(id).expect("id"),
            "Polish".to_owned(),
            PromptKind::Rewrite,
            PathBuf::from("/prompts/polish.md"),
            true,
            chord,
        )
        .expect("prompt")
    }

    fn control(key_code: u16) -> Option<Chord> {
        let modifiers = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        Some(Chord::new(key_code, modifiers).expect("chord"))
    }

    #[test]
    fn a_uuid_is_an_id_and_a_path_is_not() {
        assert!(PromptId::new("7f3c2a90-1b2d-4e5f-8a9b-0c1d2e3f4a5b").is_ok());
        for id in ["", "../polish", "a b", &"a".repeat(MAX_ID_BYTES + 1)] {
            assert!(PromptId::new(id).is_err(), "{id}");
        }
    }

    #[test]
    fn a_prompt_needs_a_plain_name_and_an_absolute_file() {
        let new = |name: &str, file: &str| {
            let id = PromptId::new("p").expect("id");
            Prompt::new(
                id,
                name.into(),
                PromptKind::Rewrite,
                file.into(),
                true,
                None,
            )
        };
        assert!(new(" ", "/p.md").is_err());
        assert!(new("P\u{1b}", "/p.md").is_err());
        assert!(new("P", "p.md").is_err());
        assert!(new("P", &format!("/{}", "a".repeat(MAX_FILE_BYTES))).is_err());
        assert!(new("P", "/p.md").is_ok());
    }

    #[test]
    fn a_list_holds_each_id_once_and_is_bounded() {
        assert!(matches!(
            Prompts::new(vec![prompt("a", None), prompt("a", None)]),
            Err(PromptError::DuplicateId)
        ));
        let many = (0..=MAX_PROMPTS)
            .map(|n| prompt(&n.to_string(), None))
            .collect();
        assert!(matches!(Prompts::new(many), Err(PromptError::TooMany)));

        let prompts =
            Prompts::new(vec![prompt("a", None), prompt("b", control(1))]).expect("prompts");
        assert_eq!(prompts.find("b").map(|p| p.id().as_str()), Some("b"));
        assert!(prompts.find("c").is_none());
    }
}
