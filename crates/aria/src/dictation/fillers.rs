//! Drops what the decoder heard but nobody meant: disfluencies and a word
//! said twice in a row.

use super::Corrector;

/// Sounds that are never a word, after long runs of a letter shrink to
/// two ("mmm", "hmmmm"). Real words that also serve as fillers ("like",
/// "so") stay, since dropping them changes meaning.
const FILLERS: &[&str] = &[
    "um", "umm", "uh", "uhh", "uhm", "er", "erm", "ah", "ahh", "hmm", "hm", "mm", "mhm",
];

pub(crate) struct Fillers;

impl Corrector for Fillers {
    fn correct(&self, text: &str) -> Option<String> {
        let mut kept: Vec<Word> = Vec::new();
        let mut capitalize_next = false;
        let mut dropped = false;
        for mut word in words(text) {
            let core = word.core();
            let filler = FILLERS.contains(&squeezed(&core).as_str());
            // Every doubled word goes, contractions included, even valid
            // English like "had had".
            let repeat = is_spoken(&core)
                && kept
                    .last()
                    .is_some_and(|last| !last.ends_with_punctuation() && last.core() == core);
            if !(filler || repeat) {
                if capitalize_next {
                    word.capitalize();
                    capitalize_next = false;
                }
                kept.push(word);
                continue;
            }
            dropped = true;
            capitalize_next |= filler && word.text.starts_with(char::is_uppercase);
            if let Some(last) = kept.last_mut() {
                last.absorb(&word);
            }
        }
        if !dropped {
            return None;
        }
        Some(kept.iter().map(Word::render).collect())
    }
}

struct Word {
    text: String,
    trailing_space: String,
}

impl Word {
    /// The word without surrounding punctuation, lowercased, for matching.
    fn core(&self) -> String {
        self.text
            .trim_matches(|c: char| c.is_ascii_punctuation())
            .to_lowercase()
    }

    fn ends_with_punctuation(&self) -> bool {
        self.text.ends_with(|c: char| c.is_ascii_punctuation())
    }

    fn capitalize(&mut self) {
        let mut chars = self.text.chars();
        if let Some(first) = chars.next() {
            self.text = first.to_uppercase().chain(chars).collect();
        }
    }

    /// A dropped neighbour leaves its punctuation and spacing behind, so
    /// "report um, by" reads "report, by".
    fn absorb(&mut self, dropped: &Self) {
        let words_end = dropped
            .text
            .trim_end_matches(|c: char| c.is_ascii_punctuation())
            .len();
        let punctuation = &dropped.text[words_end..];
        if !self.ends_with_punctuation() {
            self.text.push_str(punctuation);
        }
        if !self.trailing_space.contains('\n') {
            self.trailing_space = dropped.trailing_space.clone();
        }
    }

    fn render(&self) -> String {
        format!("{}{}", self.text, self.trailing_space)
    }
}

fn squeezed(core: &str) -> String {
    let mut out = String::with_capacity(core.len());
    let mut run = 0;
    let mut last = None;
    for c in core.chars() {
        run = if last == Some(c) { run + 1 } else { 1 };
        last = Some(c);
        if run <= 2 {
            out.push(c);
        }
    }
    out
}

fn is_spoken(core: &str) -> bool {
    !core.is_empty() && core.chars().all(|c| c.is_alphabetic() || c == '\'')
}

fn words(text: &str) -> Vec<Word> {
    let mut words = Vec::new();
    let mut rest = text.trim_start();
    while !rest.is_empty() {
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, after) = rest.split_at(end);
        let space_end = after
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(after.len());
        let (space, next) = after.split_at(space_end);
        words.push(Word {
            text: word.to_owned(),
            trailing_space: space.to_owned(),
        });
        rest = next;
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corrected(text: &str) -> Option<String> {
        Fillers.correct(text)
    }

    #[test]
    fn the_corpus_cleans_as_expected() {
        let corpus = include_str!("../../tests/fixtures/fillers.txt");
        for case in corpus.split("\n\n") {
            let mut lines = case.lines().filter(|line| !line.starts_with('#'));
            let Some(heard) = lines.next() else {
                continue;
            };
            assert_eq!(corrected(heard).as_deref(), lines.next(), "heard {heard:?}");
        }
    }

    #[test]
    fn a_newline_before_a_dropped_word_survives() {
        assert_eq!(
            corrected("report.\num, by friday").as_deref(),
            Some("report.\nby friday")
        );
    }

    #[test]
    fn nothing_but_fillers_leaves_nothing() {
        assert_eq!(corrected("Um.").as_deref(), Some(""));
        assert_eq!(corrected("uh uh").as_deref(), Some(""));
    }

    #[test]
    fn empty_text_is_unchanged() {
        assert_eq!(corrected(""), None);
    }
}
