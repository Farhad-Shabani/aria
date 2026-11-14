use crate::cancel::CancelToken;

use super::claim::Claimed;
use super::{Layer, TranscriptionError, Vocabulary, check_cancelled, check_size, numbers, symbols};

/// Joins the words around a spoken symbol, as in "user underscore id" →
/// `user_id`.
#[derive(Debug, Clone, Copy, Default)]
pub struct JoinSymbols;

impl Layer for JoinSymbols {
    fn apply(&self, text: &mut String, cancel: &CancelToken) -> Result<(), TranscriptionError> {
        let vocabulary = Vocabulary::default();
        let corrected = vocabulary.correct_with(
            text,
            |claimed| claim_each(&[Step::Symbols], text, &vocabulary, claimed),
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
    fn claim<'a>(self, text: &str, vocabulary: &'a Vocabulary, claimed: &mut Claimed<'a>) {
        match self {
            Self::Forms => vocabulary.claim_forms(text, claimed),
            Self::Numbers => numbers::claim(text, claimed),
            Self::Symbols => symbols::claim(text, claimed),
        }
    }

    /// Defers more words, given where the cut now falls.
    fn defer(self, text: &str, vocabulary: &Vocabulary, claimed: &mut Claimed<'_>) {
        match self {
            Self::Forms => vocabulary.defer_waiting_forms(text, claimed),
            Self::Numbers => {}
            Self::Symbols => symbols::defer(text, claimed),
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
    vocabulary: &'a Vocabulary,
    claimed: &mut Claimed<'a>,
) {
    for step in steps {
        step.claim(text, vocabulary, claimed);
    }
}

/// Dictated text as corrected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corrected {
    pub text: String,
    /// `text` opens with a spoken symbol that joins the last word typed
    /// before it, so no space comes between them. This is
    /// [`Spacing::joins_before`](crate::transcription::Spacing::joins_before).
    pub joins_before: bool,
}

impl Vocabulary {
    /// Corrects dictated `text` in one pass: terms, spoken forms, numbers,
    /// symbols, then personal rules on what nothing claimed.
    /// `before` is text already typed: a symbol `text` opens with joins its
    /// last word, unless whitespace ends it.
    pub fn correct_spoken(&self, text: &str, before: &str) -> Option<Corrected> {
        let typed = before
            .split_whitespace()
            .next_back()
            .filter(|_| !before.ends_with(char::is_whitespace));
        let Some(typed) = typed else {
            let text = self.spoken(text, 0)?;
            return Some(Corrected {
                text,
                joins_before: false,
            });
        };
        // The typed word reads as the first word, so a symbol after it joins
        // it as it would any word. Whether a space still follows it says
        // whether one did.
        let read = format!("{typed} {text}");
        let corrected = self.spoken(&read, typed.len())?;
        let after = &corrected[typed.len()..];
        let (after, joins_before) = match after.strip_prefix(' ') {
            Some(after) => (after, false),
            None => (after, true),
        };
        (joins_before || after != text).then(|| Corrected {
            text: after.to_owned(),
            joins_before,
        })
    }

    /// `text` corrected by every step, after its first `typed_len` bytes,
    /// a word already typed.
    fn spoken(&self, text: &str, typed_len: usize) -> Option<String> {
        self.correct_spoken_with(text, typed_len, |claimed| {
            claim_each(&SPOKEN, text, self, claimed);
        })
    }

    /// Where the words at the end of dictated `text` that more speech may
    /// still change begin: the text's length when none may. This is what
    /// [`DeferredWords`](super::DeferredWords) holds.
    ///
    /// Only the steps before the first join claim, because more speech may
    /// still undo a join. Then every step defers more words until the cut
    /// stays put, or until more than `max_words` are deferred, which a hold
    /// types rather than holds.
    pub(super) fn spoken_deferred_from(&self, text: &str, max_words: usize) -> usize {
        let unjoined = SPOKEN.iter().take_while(|step| !step.joins()).count();
        let mut claimed = self.claimed_spoken(text, 0, |claimed| {
            claim_each(&SPOKEN[..unjoined], text, self, claimed);
        });
        loop {
            let from = claimed.deferred_from();
            for step in SPOKEN {
                step.defer(text, self, &mut claimed);
            }
            let to = claimed.deferred_from();
            if to == from || text[to..].split_whitespace().nth(max_words).is_some() {
                return to;
            }
        }
    }
}
