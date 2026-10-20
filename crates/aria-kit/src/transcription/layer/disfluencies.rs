use crate::cancel::CancelToken;

use super::{Layer, TranscriptionError};

const FILLERS: &[&str] = &[
    "um", "umm", "uh", "uhh", "uhm", "er", "erm", "ah", "ahh", "hmm", "hm", "mm", "mhm",
];

#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveHesitations;

impl Layer for RemoveHesitations {
    fn apply(&self, text: &mut String, _cancel: &CancelToken) -> Result<(), TranscriptionError> {
        remove_words(text, DroppedCapital::Carry, |_, word| {
            FILLERS.contains(&squeezed(&word.core()).as_str())
        });
        Ok(())
    }
}

/// Drops the second of two identical spoken words in a row, even valid
/// English such as "had had". Punctuation between them keeps both, as in
/// "very, very good".
///
/// Apply it after [`RemoveHesitations`]. A filler between two copies, as in
/// "the um the", hides the repeat.
#[derive(Debug, Clone, Copy, Default)]
pub struct RemoveRepeats;

impl Layer for RemoveRepeats {
    fn apply(&self, text: &mut String, _cancel: &CancelToken) -> Result<(), TranscriptionError> {
        remove_words(text, DroppedCapital::Discard, |previous, word| {
            let core = word.core();
            is_spoken(&core)
                && previous.is_some_and(|previous| {
                    !previous.ends_with_punctuation() && previous.core() == core
                })
        });
        Ok(())
    }
}

/// Whether a dropped capitalized word passes its capital to the word after
/// it. A filler does; a repeat leaves its first copy capitalized.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DroppedCapital {
    Carry,
    Discard,
}

fn remove_words(
    text: &mut String,
    capital: DroppedCapital,
    should_drop: impl Fn(Option<&Word>, &Word) -> bool,
) {
    let mut kept: Vec<Word> = Vec::new();
    let mut opening = String::new();
    let mut capitalize_next = false;
    let mut dropped = false;
    for mut word in words(text) {
        if !should_drop(kept.last(), &word) {
            if capitalize_next {
                word.capitalize();
                capitalize_next = false;
            }
            word.text.insert_str(0, &opening);
            opening.clear();
            kept.push(word);
            continue;
        }
        dropped = true;
        let spoken = word.text.trim_matches(|c: char| c.is_ascii_punctuation());
        capitalize_next |=
            capital == DroppedCapital::Carry && spoken.starts_with(char::is_uppercase);
        let start = word.text.len()
            - word
                .text
                .trim_start_matches(|c: char| c.is_ascii_punctuation())
                .len();
        let end = word
            .text
            .trim_end_matches(|c: char| c.is_ascii_punctuation())
            .len();
        opening.extend(
            word.text[..start]
                .chars()
                .filter(|c| matches!(c, '(' | '[' | '{' | '\"' | '\'')),
        );
        let mut punctuation = String::new();
        for character in word.text[end..].chars() {
            if matches!(
                (opening.chars().next_back(), character),
                (Some('('), ')')
                    | (Some('['), ']')
                    | (Some('{'), '}')
                    | (Some('\"'), '\"')
                    | (Some('\''), '\'')
            ) {
                opening.pop();
            } else {
                punctuation.push(character);
            }
        }
        if let Some(last) = kept.last_mut() {
            last.absorb(&punctuation, &word.trailing_space);
        }
    }
    if !dropped {
        return;
    }
    *text = kept.iter().map(Word::render).collect();
}

struct Word {
    text: String,
    trailing_space: String,
}

impl Word {
    fn core(&self) -> String {
        self.text
            .trim_matches(|c: char| c.is_ascii_punctuation())
            .to_lowercase()
    }

    fn ends_with_punctuation(&self) -> bool {
        self.text.ends_with(|c: char| c.is_ascii_punctuation())
    }

    fn capitalize(&mut self) {
        let start = self.text.len()
            - self
                .text
                .trim_start_matches(|c: char| c.is_ascii_punctuation())
                .len();
        let mut chars = self.text[start..].chars();
        if let Some(first) = chars.next() {
            self.text.replace_range(
                start..start + first.len_utf8(),
                &first.to_uppercase().to_string(),
            );
        }
    }

    fn absorb(&mut self, punctuation: &str, trailing_space: &str) {
        if punctuation.contains(['.', '!', '?', ')', ']', '}', '"', '\'']) {
            self.text
                .truncate(self.text.trim_end_matches([',', ';', ':']).len());
        }
        if !self.ends_with_punctuation() {
            self.text.push_str(punctuation);
        } else {
            self.text.extend(
                punctuation
                    .chars()
                    .filter(|c| matches!(c, ')' | ']' | '}' | '"' | '\'')),
            );
        }
        if !self.trailing_space.contains('\n') {
            trailing_space.clone_into(&mut self.trailing_space);
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
