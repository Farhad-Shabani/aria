//! Who a stretch of speech is attributed to.

use std::fmt;

use super::SpeakerName;

/// An enrolled person, or a voice nobody enrolled, numbered in the order it
/// was first heard.
///
/// The two never compare equal, so an enrolled person named "Speaker 2" is
/// not mistaken for the second anonymous speaker, though both render alike.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Label {
    Enrolled(SpeakerName),
    Anonymous(u32),
}

impl fmt::Display for Label {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enrolled(name) => write!(formatter, "{name}"),
            Self::Anonymous(number) => write!(formatter, "Speaker {number}"),
        }
    }
}

/// Hands out anonymous speaker numbers from 1, never reusing one: a number
/// retired by a merge leaves a gap rather than renaming a label somebody
/// already read.
#[derive(Debug, Default)]
pub(crate) struct AnonymousNumbering {
    issued: u32,
}

impl AnonymousNumbering {
    pub(crate) fn issue(&mut self) -> Label {
        self.issued += 1;
        Label::Anonymous(self.issued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_renders_as_a_name_or_a_numbered_speaker() {
        let alex = Label::Enrolled(SpeakerName::new("alex").expect("a usable name"));
        assert_eq!(alex.to_string(), "alex");
        assert_eq!(Label::Anonymous(3).to_string(), "Speaker 3");
    }
}
