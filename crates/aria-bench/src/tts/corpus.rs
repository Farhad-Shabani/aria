//! The utterances every backend speaks.

use std::path::Path;

use aria_kit::synthesis::SpeechText;

/// The corpus shipped with the benchmark, spoken when no file is named.
const DEFAULT_CORPUS: &str = include_str!("corpus.txt");

pub(crate) struct Corpus {
    pub utterances: Vec<Utterance>,
}

/// One line of the corpus, numbered in corpus order. The id is what rows and
/// summaries key on; utterance text is personal-content-shaped and never
/// enters either.
pub(crate) struct Utterance {
    pub id: usize,
    pub text: SpeechText,
}

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("failed to read the corpus")]
    Read(#[source] std::io::Error),

    #[error("corpus line {line} exceeds the speech text limit")]
    TooLong { line: usize },

    #[error("the corpus names no utterance")]
    Empty,
}

/// Loads a corpus file, falling back to the one shipped with the benchmark.
pub(crate) fn load(path: Option<&Path>) -> Result<Corpus, CorpusError> {
    let text = match path {
        Some(path) => std::fs::read_to_string(path).map_err(CorpusError::Read)?,
        None => DEFAULT_CORPUS.to_owned(),
    };
    parse(&text)
}

fn parse(text: &str) -> Result<Corpus, CorpusError> {
    let mut utterances = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let text = SpeechText::new(line).map_err(|error| match error {
            aria_kit::synthesis::SpeechTextError::TooLong { .. } => {
                CorpusError::TooLong { line: number + 1 }
            }
            // A line that survived the blank check cannot be empty.
            aria_kit::synthesis::SpeechTextError::Empty => unreachable!(),
        })?;
        utterances.push(Utterance {
            id: utterances.len(),
            text,
        });
    }

    if utterances.is_empty() {
        return Err(CorpusError::Empty);
    }
    Ok(Corpus { utterances })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_and_comments_are_not_utterances() {
        let corpus = parse("# a note\n\nhello there\n   \nsecond line\n").unwrap();
        let spoken: Vec<&str> = corpus
            .utterances
            .iter()
            .map(|utterance| utterance.text.as_str())
            .collect();
        assert_eq!(spoken, ["hello there", "second line"]);
    }

    #[test]
    fn ids_number_the_utterances_not_the_lines() {
        let corpus = parse("# a note\nfirst\n\nsecond\n").unwrap();
        let ids: Vec<usize> = corpus.utterances.iter().map(|line| line.id).collect();
        assert_eq!(ids, [0, 1]);
    }

    #[test]
    fn a_corpus_of_only_notes_is_empty() {
        assert!(matches!(
            parse("# just a note\n\n"),
            Err(CorpusError::Empty)
        ));
    }

    #[test]
    fn an_overlong_line_names_its_line_number() {
        let text = format!("fine\n{}\n", "x".repeat(16 * 1024));
        assert!(matches!(
            parse(&text),
            Err(CorpusError::TooLong { line: 2 })
        ));
    }

    #[test]
    fn the_shipped_corpus_parses() {
        assert!(!parse(DEFAULT_CORPUS).unwrap().utterances.is_empty());
    }
}
