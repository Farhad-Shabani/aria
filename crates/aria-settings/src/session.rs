use crate::config::AssistantId;

const MAX_NAME_BYTES: usize = 128;

/// The name of a chat other than a folder's own: trimmed, 1 to
/// `MAX_NAME_BYTES` bytes, with no control characters. It never starts with
/// `/`, because a folder's own chat goes by its absolute path and the two
/// share one set of names.
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

/// Text an ask is about, such as a dictation or a prompt result: not blank,
/// at most `MAX_BYTES`.
#[derive(Clone, PartialEq, Eq)]
pub struct Quote(String);

impl Quote {
    pub const MAX_BYTES: usize = 128 * 1024;

    pub fn new(text: String) -> Result<Self, QuoteError> {
        if text.trim().is_empty() {
            return Err(QuoteError::Blank);
        }
        if text.len() > Self::MAX_BYTES {
            return Err(QuoteError::TooLong);
        }
        Ok(Self(text))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Only the length, so a quote never reaches a log.
impl std::fmt::Debug for Quote {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Quote({} bytes)", self.0.len())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QuoteError {
    #[error("a quote can't be blank")]
    Blank,
    #[error("a quote can be at most {} bytes", Quote::MAX_BYTES)]
    TooLong,
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
    fn a_quote_keeps_its_text_and_refuses_blank_or_overlong_text() {
        let text = "  first line\nsecond line\n".to_owned();
        assert_eq!(
            Quote::new(text.clone()).map(|quote| quote.as_str().to_owned()),
            Ok(text)
        );
        assert!(Quote::new("a".repeat(Quote::MAX_BYTES)).is_ok());
        assert_eq!(Quote::new(" \n\t".to_owned()), Err(QuoteError::Blank));
        assert_eq!(
            Quote::new("a".repeat(Quote::MAX_BYTES + 1)),
            Err(QuoteError::TooLong)
        );
        assert_eq!(
            format!("{:?}", Quote::new("secret".to_owned())),
            "Ok(Quote(6 bytes))"
        );
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
