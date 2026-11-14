use std::borrow::Cow;
use std::ops::Range;

/// A span of the text: plain text between spaces or claims, or a claim of
/// any length.
#[derive(Clone)]
pub(super) struct Word<'a> {
    pub(super) span: Range<usize>,
    /// What a claimed span types: a term's spelling, a spoken form's, a
    /// number's digits, or joined words.
    pub(super) claim: Option<Cow<'a, str>>,
    /// What the word types where a spoken symbol joins it to another word
    /// that has one, as a number's digits do: "two dot five" → `2.5`.
    pub(super) joined: Option<Cow<'a, str>>,
}

/// A text's words as the steps so far claimed them, and where more speech
/// may still change them.
pub(super) struct Claimed<'a> {
    /// In order, none overlapping.
    words: Vec<Word<'a>>,
    /// Where the words more speech may still change begin: the text's
    /// length when none may. It only moves earlier, and never inside a span
    /// kept whole.
    deferred_from: usize,
    /// Spans a step decided as one, though it left their words unclaimed.
    kept_whole: Vec<Range<usize>>,
}

impl<'a> Claimed<'a> {
    pub(super) fn new(words: Vec<Word<'a>>, len: usize) -> Self {
        Self {
            words,
            deferred_from: len,
            kept_whole: Vec::new(),
        }
    }

    pub(super) fn words(&self) -> &[Word<'a>] {
        &self.words
    }

    /// Takes the words for a step to claim from. The step pushes them back.
    pub(super) fn take_words(&mut self) -> Vec<Word<'a>> {
        std::mem::take(&mut self.words)
    }

    pub(super) fn push(&mut self, word: Word<'a>) {
        debug_assert!(
            self.words
                .last()
                .is_none_or(|last| last.span.end <= word.span.start),
            "words stay in order"
        );
        self.words.push(word);
    }

    pub(super) fn extend(&mut self, words: impl IntoIterator<Item = Word<'a>>) {
        words.into_iter().for_each(|word| self.push(word));
    }

    pub(super) fn deferred_from(&self) -> usize {
        self.deferred_from
    }

    /// Defers the words from `at` on, and the whole of a span kept whole
    /// that `at` falls inside.
    pub(super) fn defer_at(&mut self, at: usize) {
        self.deferred_from = self.deferred_from.min(self.outside_kept_whole(at));
    }

    /// Keeps `span` whole: no cut falls inside it.
    pub(super) fn keep_whole(&mut self, span: Range<usize>) {
        self.kept_whole.push(span);
        self.deferred_from = self.outside_kept_whole(self.deferred_from);
    }

    fn outside_kept_whole(&self, at: usize) -> usize {
        self.kept_whole
            .iter()
            .filter(|whole| whole.start < at && at < whole.end)
            .map(|whole| whole.start)
            .min()
            .unwrap_or(at)
    }
}

impl<'a> Word<'a> {
    pub(super) fn plain(span: Range<usize>) -> Self {
        Self {
            span,
            claim: None,
            joined: None,
        }
    }

    pub(super) fn claimed(span: Range<usize>, typed: Cow<'a, str>) -> Self {
        Self {
            span,
            claim: Some(typed),
            joined: None,
        }
    }

    pub(super) fn typed<'s>(&'s self, text: &'s str) -> &'s str {
        self.claim.as_deref().unwrap_or(&text[self.span.clone()])
    }
}
