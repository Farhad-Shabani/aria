use crate::cancel::CancelToken;

use super::super::Attachment;
use super::claim::{Claimed, Place};
use super::symbols;
use super::{Layer, TranscriptionError, Vocabulary, check_cancelled, check_size, numbers};

/// Joins the words around a spoken symbol, as in "user underscore id" →
/// `user_id`.
///
/// It never sees what came before, so "dot env" stays `dot env`.
/// [`Vocabulary::correct_spoken`] types `.env` at a dictation's start.
#[derive(Debug, Clone, Copy, Default)]
pub struct JoinSymbols;

impl Layer for JoinSymbols {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        let vocabulary = Vocabulary::default();
        let corrected = vocabulary.correct_with(
            text,
            |claimed| {
                let reading = Reading {
                    vocabulary: &vocabulary,
                    writing: Writing::default(),
                    place: Place::Later,
                };
                claim_each(&[Step::Symbols], text, reading, claimed);
            },
            &|bytes| {
                check_cancelled(cancel)?;
                check_size(bytes)
            },
        )?;
        if let Some(corrected) = corrected {
            *text = corrected;
        }
        Ok(())
    }
}

/// What dictated text is for. Only a named slash after a plain English
/// word types differently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Writing {
    /// A terminal or editor. "run forward slash review" → `run /review`.
    #[default]
    Commands,
    /// A document or chat. "and forward slash or" → `and/or`.
    Prose,
}

/// Dictated text as corrected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corrected {
    pub text: String,
    /// Type `text` with this as
    /// [`Spacing::attachment`](crate::transcription::Spacing::attachment).
    pub attachment: Attachment,
}

/// A step that claims dictated words. Reading the words claims vocabulary
/// terms first, and typing applies personal rules last, to what no step
/// claimed. No step rewrites an earlier claim.
#[derive(Clone, Copy)]
pub(super) enum Step {
    /// The spoken forms of personal rules. The user's words come first, so
    /// "i eighteen n" types `i18n`, not `i 18 n`.
    Forms,
    /// Spoken numbers, written as digits.
    Numbers,
    /// Spoken symbols, which join their neighbors, claimed or not.
    Symbols,
}

/// What a join depends on besides the words. "and forward slash or" →
/// `and/or` in prose, `and /or` in commands.
#[derive(Clone, Copy)]
pub(super) struct Reading<'a> {
    pub(super) vocabulary: &'a Vocabulary,
    pub(super) writing: Writing,
    pub(super) place: Place,
}

/// The steps of [`Vocabulary::correct_spoken`], in order.
const SPOKEN: [Step; 3] = [Step::Forms, Step::Numbers, Step::Symbols];

// Only the steps before the first join claim when the deferred words are
// found, so a step that defers as it claims must come before it.
const _: () = {
    let mut index = 0;
    let mut joined = false;
    while index < SPOKEN.len() {
        assert!(
            !(joined && SPOKEN[index].defers_as_it_claims()),
            "a step that defers as it claims comes before the first join"
        );
        joined |= SPOKEN[index].joins();
        index += 1;
    }
};

impl Step {
    fn claim<'a>(self, text: &str, reading: Reading<'a>, claimed: &mut Claimed<'a>) -> Vec<usize> {
        match self {
            Self::Forms => reading.vocabulary.claim_forms(text, reading.place, claimed),
            Self::Numbers => numbers::claim(text, claimed),
            Self::Symbols => return symbols::claim(text, reading, claimed),
        }
        Vec::new()
    }

    /// Defers more words, given where the cut now falls.
    fn defer(self, text: &str, reading: Reading<'_>, claimed: &mut Claimed<'_>) {
        match self {
            Self::Forms => reading.vocabulary.defer_waiting_forms(text, claimed),
            Self::Numbers => {}
            Self::Symbols => symbols::defer(text, reading, claimed),
        }
    }

    /// Whether the step defers its words as it claims them, rather than in
    /// [`Self::defer`].
    const fn defers_as_it_claims(self) -> bool {
        match self {
            Self::Forms | Self::Symbols => false,
            Self::Numbers => true,
        }
    }

    /// Whether the step joins words into one claim, which more speech may
    /// still undo.
    const fn joins(self) -> bool {
        match self {
            Self::Forms | Self::Numbers => false,
            Self::Symbols => true,
        }
    }
}

fn claim_each<'a>(
    steps: &[Step],
    text: &str,
    reading: Reading<'a>,
    claimed: &mut Claimed<'a>,
) -> Vec<usize> {
    steps
        .iter()
        .flat_map(|step| step.claim(text, reading, claimed))
        .collect()
}

/// What sits right before new dictated text. A spoken symbol at the
/// start of the new text joins only a word right before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TypedBefore<'a> {
    /// The dictation starts here. "dot env" → `.env`.
    Nothing,
    /// Text typed earlier. After `src`, "forward slash main" → `src/main`.
    /// A trailing space in the text stops the join. Empty text is
    /// [`Self::Nothing`].
    Text(&'a str),
    /// A finished sentence. "forward slash main" → ` /main`.
    Apart,
}

impl Vocabulary {
    /// Corrects a dictation's whole `text` in one pass: terms, spoken forms,
    /// numbers, symbols, then personal rules on what nothing claimed.
    pub fn correct_spoken(&self, text: &str, writing: Writing) -> Option<Corrected> {
        self.correct_spoken_after(text, TypedBefore::Nothing, writing)
    }

    /// Like [`Self::correct_spoken`], for text typed after `before`.
    pub(super) fn correct_spoken_after(
        &self,
        text: &str,
        before: TypedBefore<'_>,
        writing: Writing,
    ) -> Option<Corrected> {
        let (typed, place) = match before {
            TypedBefore::Nothing | TypedBefore::Text("") => (None, Place::First),
            TypedBefore::Apart => (None, Place::Later),
            TypedBefore::Text(before) => (
                before
                    .split_whitespace()
                    .next_back()
                    .filter(|_| !before.ends_with(char::is_whitespace)),
                Place::Later,
            ),
        };
        let lead = text.len() - text.trim_start().len();
        let attachment = |apart: &[usize], start: usize| match apart.contains(&start) {
            true => Attachment::Apart,
            false => Attachment::AsWritten,
        };
        let Some(typed) = typed else {
            let (text, apart) = self.spoken(text, 0, place, writing)?;
            return Some(Corrected {
                text,
                attachment: attachment(&apart, lead),
            });
        };
        // The typed word reads as the first word, so a symbol after it joins
        // it as it would any word. Whether a space still follows it says
        // whether one did.
        let read = format!("{typed} {text}");
        let (corrected, apart) = self.spoken(&read, typed.len(), place, writing)?;
        let after = &corrected[typed.len()..];
        let (after, attachment) = match after.strip_prefix(' ') {
            Some(after) => (after, attachment(&apart, typed.len() + 1 + lead)),
            None => (after, Attachment::Joined),
        };
        (attachment == Attachment::Joined || after != text).then(|| Corrected {
            text: after.to_owned(),
            attachment,
        })
    }

    /// `text` corrected by every step, after its first `typed_len` bytes,
    /// a word already typed.
    ///
    /// Also returns the start of each claim that begins with a symbol
    /// joined only to the next word, as `--verbose`.
    fn spoken(
        &self,
        text: &str,
        typed_len: usize,
        place: Place,
        writing: Writing,
    ) -> Option<(String, Vec<usize>)> {
        let mut apart = Vec::new();
        let corrected = self.correct_spoken_with(text, typed_len, |claimed| {
            let reading = Reading {
                vocabulary: self,
                writing,
                place,
            };
            apart = claim_each(&SPOKEN, text, reading, claimed);
        })?;
        Some((corrected, apart))
    }

    /// Where the words at the end of dictated `text` that more speech may
    /// still change begin: the text's length when none may. This is what
    /// [`DeferredWords`](super::DeferredWords) holds.
    ///
    /// Only the steps before the first join claim, because more speech may
    /// still undo a join. Next, the vocabulary defers a name the text ends
    /// partway through. Then every step defers more words until the cut
    /// stays put, or until more than `max_words` are deferred, which a hold
    /// types rather than holds.
    pub(super) fn spoken_deferred_from(
        &self,
        text: &str,
        max_words: usize,
        writing: Writing,
    ) -> usize {
        let unjoined = &SPOKEN[..SPOKEN.iter().take_while(|step| !step.joins()).count()];
        // The hold can't see the typed text, so it reads `text` as
        // mid-dictation. A form that starts with a symbol then stays
        // unclaimed, which only holds more words.
        let reading = Reading {
            vocabulary: self,
            writing,
            place: Place::Later,
        };
        let mut claimed = self.claimed_spoken(text, 0, |claimed| {
            claim_each(unjoined, text, reading, claimed);
        });
        // Outside the loop: where a name starts doesn't move with the cut.
        super::vocabulary::defer_unclosed_assignment(text, max_words, &mut claimed);
        self.defer_waiting_names(text, max_words, &mut claimed);
        loop {
            let from = claimed.deferred_from();
            // The words before the cut type as their own text, so a step may
            // defer their end, as "two five oh" waits for more digits.
            let before = text[..from].trim_end();
            if before.len() < text.len() {
                let ended = self.claimed_spoken(before, 0, |claimed| {
                    claim_each(unjoined, before, reading, claimed);
                });
                if ended.deferred_from() < before.len() {
                    claimed.defer_at(ended.deferred_from());
                }
            }
            for step in SPOKEN {
                step.defer(text, reading, &mut claimed);
            }
            let to = claimed.deferred_from();
            if to == from || text[to..].split_whitespace().nth(max_words).is_some() {
                return to;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::vocabulary::PersonalRules;
    use super::super::{Vocabulary, Writing};

    fn corrected(heard: &str, terms: &[&str], learned: &[(&str, &str)]) -> String {
        let terms = terms.iter().map(|term| (*term).to_owned()).collect();
        let learned = learned
            .iter()
            .map(|(spoken, typed)| ((*spoken).to_owned(), (*typed).to_owned()))
            .collect();
        let vocabulary = Vocabulary::compile(&[], PersonalRules::new(terms, learned)).unwrap();
        vocabulary
            .correct_spoken(heard, Writing::Commands)
            .map_or_else(|| heard.to_owned(), |corrected| corrected.text)
    }

    #[rstest]
    #[case::spoken_form_before_a_number("i eighteen n", &["i18n"], &[], "i18n")]
    #[case::learned_pair_before_a_number("open twenty four seven", &[], &[("twenty four seven", "24/7")], "open 24/7")]
    #[case::number_beside_a_spoken_form("i eighteen n twenty one", &["i18n"], &[], "i18n 21")]
    #[case::symbol_joins_a_spoken_form("i eighteen n underscore key", &["i18n"], &[], "i18n_key")]
    #[case::symbol_joins_a_number("version twenty dot twenty one", &[], &[], "version 20.21")]
    #[case::form_inside_punctuation("(teh, twenty one)", &[], &[("teh", "the")], "(the, 21)")]
    #[case::near_miss_beside_a_number("kubernets twenty one", &["Kubernetes"], &[], "Kubernetes 21")]
    #[case::form_with_a_symbol_inside("open cargo dot toml", &[], &[("cargo dot toml", "Cargo.toml")], "open Cargo.toml")]
    #[case::form_opening_on_a_symbol_joins("example dot com", &[], &[("dot com", ".com")], "example.com")]
    #[case::form_opening_on_a_name_said_apart("type back slash n here", &[], &[("back slash n", "newline")], "type \\n here")]
    #[case::form_naming_a_listed_run("it was hyphen hyphen frankly", &[], &[("hyphen hyphen", "—")], "it was — frankly")]
    #[case::form_naming_a_run_between_words("wait dot dot dot okay", &[], &[("dot dot dot", "…")], "wait … okay")]
    #[case::form_opening_on_a_symbol_with_no_word_before("dot com is taken", &[], &[("dot com", ".com")], ".com is taken")]
    #[case::listed_capital_after_a_number("at nine fifty five pm", &["PM"], &[], "at 9:55 PM")]
    #[case::listed_capital_inside_a_join("open aria dot toml", &["Aria"], &[], "open aria.toml")]
    #[case::listed_capital_on_a_pair_target("saying arya now", &["Aria"], &[("arya", "aria")], "saying Aria now")]
    fn each_step_keeps_what_an_earlier_one_claimed(
        #[case] heard: &str,
        #[case] terms: &[&str],
        #[case] learned: &[(&str, &str)],
        #[case] typed: &str,
    ) {
        assert_eq!(corrected(heard, terms, learned), typed);
    }

    #[test]
    fn a_term_keeps_its_number_words() {
        let vocabulary = Vocabulary::new(["Area Fifty One"]).unwrap();
        assert_eq!(
            vocabulary.correct_spoken("area fifty one", Writing::Commands),
            None
        );
        assert_eq!(
            Vocabulary::default()
                .correct_spoken("area fifty one", Writing::Commands)
                .map(|corrected| corrected.text)
                .as_deref(),
            Some("area 51")
        );
    }
}
