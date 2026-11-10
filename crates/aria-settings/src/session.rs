use crate::config::AssistantId;

const MAX_NAME_BYTES: usize = 128;

/// Names an assistant chat a client continues, apart from a
/// folder's own: trimmed, 1 to 128 bytes, with no control characters. It
/// never starts with `/`, because a folder's own chat goes by its
/// absolute path, and the two share one set of names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatName(String);

impl ChatName {
    pub fn new(name: &str) -> Result<Self, ChatNameError> {
        let name = name.trim();
        if name.is_empty() || name.len() > MAX_NAME_BYTES || name.chars().any(char::is_control) {
            return Err(ChatNameError::Malformed);
        }
        if name.starts_with('/') {
            return Err(ChatNameError::FolderPath);
        }
        Ok(Self(name.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Names a chat: the asks a client sends under one name. A new assistant
/// session keeps the chat and its name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChatKey {
    pub assistant_id: AssistantId,
    pub name: ChatName,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ChatNameError {
    #[error("a chat name must be one line of 1 to {MAX_NAME_BYTES} bytes")]
    Malformed,
    #[error("a chat name can't start with `/`, which names a folder's own chat")]
    FolderPath,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_name_is_trimmed_and_a_blank_overlong_or_multiline_one_is_refused() {
        assert_eq!(
            ChatName::new("  planning ").map(|name| name.as_str().to_owned()),
            Ok("planning".to_owned())
        );
        assert!(ChatName::new(&"é".repeat(64)).is_ok());
        for name in ["", "   ", "two\nlines", "tab\there", &"é".repeat(65)] {
            assert_eq!(
                ChatName::new(name),
                Err(ChatNameError::Malformed),
                "{name:?}"
            );
        }
    }

    #[test]
    fn a_chat_name_that_could_name_a_folder_is_refused() {
        assert!(ChatName::new("proj/notes").is_ok());
        for name in ["/Users/me/proj", " /tmp", "/"] {
            assert_eq!(
                ChatName::new(name),
                Err(ChatNameError::FolderPath),
                "{name:?}"
            );
        }
    }
}
