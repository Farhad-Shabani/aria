use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use crate::cancel::CancelToken;

use super::super::claim::{Claimed, Place, Word};
use super::super::number_words::{is_up_to_twenty, word_for};
use super::super::symbol_words::name_at;
use super::fuzzy::{AfterCut, apply_fuzzy, capitals, waiting_from};
use super::{MatchError, is_word};

/// What stored rules may cost in memory.
const MAX_PERSONAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESPELLINGS: usize = 4;

type Forms = HashMap<char, Vec<(Vec<char>, String)>>;

#[derive(Clone, Default, PartialEq)]
pub struct PersonalRules {
    terms: Vec<String>,
    known: Vec<Vec<char>>,
    forms: Forms,
}

impl PersonalRules {
    pub fn new(terms: Vec<String>, learned: Vec<(String, String)>) -> Self {
        let mut pairs: Vec<_> = variant_pairs(&terms).collect();
        let listed = casings(&terms);
        pairs.extend(
            learned
                .into_iter()
                .map(|(spoken, correct)| (spoken, as_listed(&listed, correct))),
        );
        Self::from_pairs(terms, pairs)
    }

    /// A learned pair applies only while `terms` lists the word it types,
    /// so removing a term also stops the pairs learned for it.
    pub fn from_stored(
        terms: Vec<String>,
        learned: Vec<(String, String)>,
        cancel: &CancelToken,
    ) -> Result<Self, MatchError> {
        let listed = casings(&terms);
        let learned = learned
            .into_iter()
            .filter(|(_, correct)| listed.contains_key(&correct.to_lowercase()))
            .map(|(spoken, correct)| (spoken, as_listed(&listed, correct)));
        let forms = Self::forms(variant_pairs(&terms).chain(learned), Some(cancel))?;
        let rules = Self {
            known: known(&terms),
            terms,
            forms,
        };
        if rules.estimated_bytes() > MAX_PERSONAL_BYTES {
            return Err(MatchError::TooLarge);
        }
        Ok(rules)
    }

    fn from_pairs(terms: Vec<String>, pairs: Vec<(String, String)>) -> Self {
        let forms = Self::forms(pairs.into_iter(), None).expect("unbounded construction");
        Self {
            known: known(&terms),
            terms,
            forms,
        }
    }

    fn forms(
        pairs: impl Iterator<Item = (String, String)>,
        cancel: Option<&CancelToken>,
    ) -> Result<Forms, MatchError> {
        let mut unique: HashMap<Vec<char>, (usize, String)> = HashMap::new();
        let mut bytes = 0;
        for (index, (spoken, canonical)) in pairs.enumerate() {
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Err(MatchError::Cancelled);
            }
            let form: Vec<char> = spoken.to_lowercase().chars().collect();
            if form.is_empty() {
                continue;
            }
            bytes += 256 + form.capacity() * 4 + canonical.capacity();
            if cancel.is_some() && bytes > MAX_PERSONAL_BYTES {
                return Err(MatchError::TooLarge);
            }
            unique
                .entry(form)
                .and_modify(|(_, target)| *target = canonical.clone())
                .or_insert((index, canonical));
        }
        let mut ordered: Vec<_> = unique.into_iter().collect();
        ordered.sort_by_key(|(_, (index, _))| *index);
        let mut forms: HashMap<char, Vec<(Vec<char>, String)>> = HashMap::new();
        for (form, (_, target)) in ordered {
            forms.entry(form[0]).or_default().push((form, target));
        }
        for bucket in forms.values_mut() {
            bucket.sort_by_key(|(form, _)| std::cmp::Reverse(form.len()));
        }
        Ok(forms)
    }

    pub fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.terms.capacity() * std::mem::size_of::<String>()
            + self.terms.iter().map(String::capacity).sum::<usize>()
            + self.known.capacity() * std::mem::size_of::<Vec<char>>()
            + self.known.iter().map(|s| s.capacity() * 4).sum::<usize>()
            + self.forms.capacity() * (std::mem::size_of::<(char, Vec<(Vec<char>, String)>)>() + 1)
            + self
                .forms
                .values()
                .map(|v| {
                    v.capacity() * std::mem::size_of::<(Vec<char>, String)>()
                        + v.iter()
                            .map(|(a, b)| a.capacity() * 4 + b.capacity())
                            .sum::<usize>()
                })
                .sum::<usize>()
    }

    pub(super) fn pairs(&self) -> impl Iterator<Item = (String, &str)> {
        self.forms
            .values()
            .flatten()
            .map(|(form, target)| (form.iter().collect(), target.as_str()))
    }

    pub(super) fn respellings(
        &self,
        room: usize,
        cancel: &CancelToken,
    ) -> Result<Respellings, MatchError> {
        let pairs = || self.forms.values().flatten();
        let mut char_count = 0;
        let mut count = 0;
        let mut lowercase_peak = 0;
        for (spoken, target) in pairs() {
            if cancel.is_cancelled() {
                return Err(MatchError::Cancelled);
            }
            count += 1;
            char_count += spoken.len();
            if target.is_ascii() {
                char_count += target.len();
                continue;
            }
            // Contextual final sigma has the same scalar count and UTF-8 width as σ.
            let mut utf8_bytes = 0;
            for c in target.chars().flat_map(char::to_lowercase) {
                char_count += 1;
                utf8_bytes += c.len_utf8();
            }
            // str::to_lowercase starts with the input's byte capacity. If it grows,
            // the old buffer is smaller than the output and its replacement can
            // double that capacity or take the byte vector's eight-byte minimum.
            let peak = if utf8_bytes > target.len() {
                utf8_bytes.saturating_add(utf8_bytes.saturating_mul(2).max(8))
            } else {
                target.len()
            };
            lowercase_peak = lowercase_peak.max(peak);
        }
        let index_bytes = count * size_of::<Spelling>() + char_count * size_of::<char>();
        if index_bytes.saturating_add(lowercase_peak) > room {
            return Err(MatchError::TooLarge);
        }
        // Both buffers are sized before allocation. Sorting the spans in place
        // avoids a second lookup table living beside the first during a build.
        let mut index = Respellings {
            chars: Vec::with_capacity(char_count),
            entries: Vec::with_capacity(count),
        };
        for (spoken, target) in pairs() {
            if cancel.is_cancelled() {
                return Err(MatchError::Cancelled);
            }
            let target = if target.is_ascii() {
                index.push(target.chars().map(|c| c.to_ascii_lowercase()))
            } else {
                index.push(target.to_lowercase().chars())
            };
            let spoken = index.push(spoken.iter().copied());
            if !target.is_empty()
                && !spoken.is_empty()
                && index.chars[target.clone()] != index.chars[spoken.clone()]
            {
                index.entries.push(Spelling { target, spoken });
            }
        }
        if index.entries.is_empty() {
            return Ok(Respellings::default());
        }
        let chars = &index.chars;
        index.entries.sort_unstable_by(|a, b| {
            let left = &chars[a.target.clone()];
            let right = &chars[b.target.clone()];
            left[0]
                .cmp(&right[0])
                .then_with(|| right.len().cmp(&left.len()))
                .then_with(|| left.cmp(right))
                .then_with(|| chars[a.spoken.clone()].cmp(&chars[b.spoken.clone()]))
        });
        index.entries.dedup_by(|a, b| {
            chars[a.target.clone()] == chars[b.target.clone()]
                && chars[a.spoken.clone()] == chars[b.spoken.clone()]
        });
        let mut previous = 0..0;
        let mut count = 0;
        index.entries.retain(|entry| {
            if chars[previous.clone()] != chars[entry.target.clone()] {
                previous = entry.target.clone();
                count = 0;
            }
            count += 1;
            count <= MAX_RESPELLINGS
        });
        Ok(index)
    }

    #[cfg(test)]
    fn apply(&self, text: &str) -> (String, Vec<(String, String)>) {
        self.apply_checked(text, &|_| Ok::<_, std::convert::Infallible>(()))
            .unwrap_or_else(|never| match never {})
    }

    pub(super) fn apply_checked<E>(
        &self,
        text: &str,
        check: &impl Fn(usize) -> Result<(), E>,
    ) -> Result<(String, Vec<(String, String)>), E> {
        check(0)?;
        let chars: Vec<char> = text.chars().collect();
        let mut output = String::with_capacity(text.len());
        let mut substitutions = Vec::new();
        let mut position = 0;
        for (form, correct) in self.forms_typed(&chars, &|| check(0))? {
            let between: String = chars[position..form.start].iter().collect();
            check(output.len().saturating_add(between.len() + correct.len()))?;
            output.push_str(&between);
            let original: String = chars[form.clone()].iter().collect();
            if original != correct {
                substitutions.push((original, correct.to_owned()));
            }
            output.push_str(correct);
            position = form.end;
        }
        let rest: String = chars[position..].iter().collect();
        check(output.len().saturating_add(rest.len()))?;
        output.push_str(&rest);
        let (output, fuzzy) = apply_fuzzy(&output, &self.terms, &self.known, check)?;
        substitutions.extend(fuzzy);
        Ok((output, substitutions))
    }

    /// The longest spoken form that starts at `position` between word
    /// boundaries: its length in characters, and what it types.
    fn form_at<E>(
        &self,
        chars: &[char],
        position: usize,
        check: &impl Fn() -> Result<(), E>,
    ) -> Result<Option<(usize, &str)>, E> {
        let first = chars[position]
            .to_lowercase()
            .next()
            .unwrap_or(chars[position]);
        let Some(forms) = self.forms.get(&first) else {
            return Ok(None);
        };
        for (form, correct) in forms {
            check()?;
            let end = position + form.len();
            if boundary(chars, position)
                && end <= chars.len()
                && boundary(chars, end)
                && chars[position..end]
                    .iter()
                    .zip(form)
                    .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
            {
                return Ok(Some((form.len(), correct)));
            }
        }
        Ok(None)
    }

    /// Claims each spoken form in the text no term claimed. A word a form ends inside is
    /// split around it, as "teh," is. A form that opens or ends on a symbol
    /// word waits: only the symbol's own join can attach it to the word
    /// beside it, as "dot com" attaches to "example".
    pub(super) fn claim<'a>(&'a self, text: &str, place: Place, claimed: &mut Claimed<'a>) {
        if self.forms.is_empty() {
            return;
        }
        let words = claimed.take_words();
        let mut unclaimed = Vec::new();
        let mut start = 0;
        for word in words {
            if word.claim.is_none() {
                unclaimed.push(word);
                continue;
            }
            self.claim_between(
                text,
                place,
                start..word.span.start,
                unclaimed.drain(..),
                claimed,
            );
            start = word.span.end;
            claimed.push(word);
        }
        self.claim_between(text, place, start..text.len(), unclaimed.drain(..), claimed);
    }

    /// The spoken forms [`Self::apply_checked`] types in `chars`, text no
    /// step claimed: at each position the longest form that starts there,
    /// then on past it. Each comes as its char range and what it types.
    fn forms_typed<E>(
        &self,
        chars: &[char],
        check: &impl Fn() -> Result<(), E>,
    ) -> Result<Vec<(Range<usize>, &str)>, E> {
        let mut forms = Vec::new();
        let mut position = 0;
        while position < chars.len() {
            check()?;
            match self.form_at(chars, position, check)? {
                Some((length, typed)) => {
                    forms.push((position..position + length, typed));
                    position += length;
                }
                None => position += 1,
            }
        }
        Ok(forms)
    }

    /// Defers the words of a spoken form that more speech may still change:
    /// a form the cut would split, or one the text ends inside.
    pub(super) fn defer_waiting_forms(&self, text: &str, claimed: &mut Claimed<'_>) {
        self.defer_a_last_word(text, claimed);
        if self.forms.is_empty() {
            return;
        }
        self.defer_a_split_form(text, claimed);
        self.defer_an_unfinished_form(text, claimed);
    }

    fn defer_a_last_word(&self, text: &str, claimed: &mut Claimed<'_>) {
        if self.terms.is_empty() {
            return;
        }
        let cut = claimed.deferred_from();
        let Some(gap) = unclaimed_around(claimed.words(), text.len(), cut) else {
            return;
        };
        let before = text[gap.start..cut].trim_end();
        let after = if cut == text.len() {
            AfterCut::SegmentEnd
        } else {
            AfterCut::DeferredWords
        };
        if let Some(start) = waiting_from(before, after, &self.terms, &self.known) {
            // Keep any opening punctuation with its word across delivery.
            let start = text[..gap.start + start]
                .trim_end_matches(|c: char| !c.is_whitespace())
                .len();
            if start >= gap.start {
                claimed.defer_at(start);
            }
        }
    }

    /// Defers a form the cut would split, as learned "tilde dot" → `~.` in
    /// "cd tilde dot". Forms are read in the text between claims, as
    /// [`Self::apply_checked`] reads them.
    fn defer_a_split_form(&self, text: &str, claimed: &mut Claimed<'_>) {
        let cut = claimed.deferred_from();
        let Some(gap) = unclaimed_around(claimed.words(), text.len(), cut) else {
            return;
        };
        let (offsets, chars): (Vec<usize>, Vec<char>) = text[gap.clone()].char_indices().unzip();
        let cut = offsets.partition_point(|offset| gap.start + offset < cut);
        let forms = self
            .forms_typed(&chars, &|| Ok::<_, std::convert::Infallible>(()))
            .unwrap_or_else(|never| match never {});
        if let Some((form, _)) = forms
            .iter()
            .find(|(form, _)| form.start < cut && cut < form.end)
        {
            claimed.defer_at(gap.start + offsets[form.start]);
        }
    }

    /// Defers the words the text ends on when they begin a longer form:
    /// "detect db" may go on as "db 1" → `db1`, and "use see sharp" as "see
    /// sharp dot" → `C#.`. The words are read as heard from where each claim
    /// starts, whatever claimed them, because the whole form outranks a later step's claim on its
    /// words: "i eighteen" begins "i eighteen n" → `i18n`. A finished form
    /// waits too when it begins another, as "db 1" begins "db 1 backup".
    fn defer_an_unfinished_form(&self, text: &str, claimed: &mut Claimed<'_>) {
        let cut = claimed.deferred_from();
        let (offsets, chars): (Vec<usize>, Vec<char>) = text.trim_end().char_indices().unzip();
        let start = claimed
            .words()
            .iter()
            .map(|word| word.span.start)
            .take_while(|&start| start < cut)
            .find(|&start| {
                let position = offsets.partition_point(|&offset| offset < start);
                self.begins_a_longer_form(&chars, position)
            });
        if let Some(start) = start {
            claimed.defer_at(start);
        }
    }

    /// Whether the words from `position` to the end of `chars` begin a
    /// longer form: "see sharp" begins "see sharp dot", and "db" begins
    /// "db 1".
    fn begins_a_longer_form(&self, chars: &[char], position: usize) -> bool {
        let said = &chars[position..];
        let Some(first) = said.first() else {
            return false;
        };
        let first = first.to_lowercase().next().unwrap_or(*first);
        let begins = |form: &[char]| {
            form.len() > said.len()
                && form[said.len()].is_whitespace()
                && form
                    .iter()
                    .zip(said)
                    .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
        };
        boundary(chars, position)
            && self
                .forms
                .get(&first)
                .is_some_and(|forms| forms.iter().any(|(form, _)| begins(form)))
    }

    /// Claims the forms in `between`, the text from one claim to the next,
    /// and pushes them with what is left of its `words`.
    fn claim_between<'a>(
        &'a self,
        text: &str,
        place: Place,
        between: Range<usize>,
        words: impl Iterator<Item = Word<'a>>,
        claimed: &mut Claimed<'a>,
    ) {
        let mut forms = self.forms_in(text, place, between).into_iter().peekable();
        for word in words {
            let mut from = word.span.start;
            while let Some(form) = forms.peek().filter(|form| form.span.start < word.span.end) {
                if form.span.start > from {
                    claimed.push(Word::plain(from..form.span.start));
                }
                from = form.span.end.min(word.span.end);
                // A form that runs past this word is claimed where it ends.
                if form.span.end > word.span.end {
                    break;
                }
                claimed.extend(forms.next());
            }
            if from < word.span.end {
                claimed.push(Word::plain(from..word.span.end));
            }
        }
    }

    /// Each spoken form in `between` that doesn't wait on a symbol, in
    /// order, as a claim on its bytes.
    fn forms_in<'a>(&'a self, text: &str, place: Place, between: Range<usize>) -> Vec<Word<'a>> {
        let (offsets, chars): (Vec<usize>, Vec<char>) =
            text[between.clone()].char_indices().unzip();
        let byte =
            |index: usize| between.start + offsets.get(index).copied().unwrap_or(between.len());
        let mut forms = Vec::new();
        let mut position = 0;
        while position < chars.len() {
            let form = self
                .form_at(&chars, position, &|| Ok::<_, std::convert::Infallible>(()))
                .unwrap_or_else(|never| match never {});
            let opens = place == Place::First && text[..byte(position)].trim().is_empty();
            let waits =
                |length: usize| waits_on_a_symbol(&chars[position..position + length], opens);
            let Some((length, typed)) = form.filter(|(length, _)| !waits(*length)) else {
                position += 1;
                continue;
            };
            forms.push(Word::claimed(
                byte(position)..byte(position + length),
                Cow::Borrowed(typed),
            ));
            position += length;
        }
        forms
    }
}

/// The text between the claims on either side of `at`, which personal rules
/// read for forms. `None` when `at` falls inside a claim.
fn unclaimed_around(words: &[Word<'_>], len: usize, at: usize) -> Option<Range<usize>> {
    let claims = || words.iter().filter(|word| word.claim.is_some());
    if claims().any(|word| word.span.start < at && at < word.span.end) {
        return None;
    }
    let start = claims()
        .map(|word| word.span.end)
        .filter(|end| *end <= at)
        .max()
        .unwrap_or(0);
    let end = claims()
        .map(|word| word.span.start)
        .filter(|start| *start >= at)
        .min()
        .unwrap_or(len);
    Some(start..end)
}

/// Whether a form said as `spoken` waits for the symbol step: it opens or
/// ends on a symbol's name, which may join the word beside the form, as
/// "dot com" joins "example".
///
/// A form of symbol names alone is the speaker's own name for them, so it
/// never waits. Nor does a symbol the form opens with when the form opens
/// the dictation, since no word comes before it.
fn waits_on_a_symbol(spoken: &[char], opens_the_dictation: bool) -> bool {
    let spoken: String = spoken.iter().collect();
    let words: Vec<&str> = spoken.split_whitespace().collect();
    let name_from = |from: usize| name_at(words[from..].iter().copied());
    let mut named = 0;
    while let Some(name) = name_from(named) {
        named += name.words;
    }
    if named == words.len() {
        return false;
    }
    let opens = named > 0 && !opens_the_dictation;
    let ends = (1..words.len())
        .any(|from| name_from(from).is_some_and(|name| from + name.words == words.len()));
    opens || ends
}

const MAX_VARIANTS: usize = 8;
/// A term with more parts than this is a serial number, not a word, and
/// its spoken forms would number in the millions.
const MAX_SEGMENTS: usize = 8;

/// A variant made only of these, digits, or number words would rewrite
/// ordinary speech ("no1" as "no one"). Small on purpose: only words that
/// plausibly sit next to a digit in a term.
const COMMON_TOKENS: &[&str] = &[
    "a",
    "an",
    "i",
    "in",
    "on",
    "at",
    "to",
    "do",
    "no",
    "go",
    "is",
    "it",
    "of",
    "or",
    "as",
    "by",
    "if",
    "be",
    "we",
    "he",
    "me",
    "my",
    "so",
    "up",
    "any",
    "all",
    "for",
    "the",
    "and",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "ten",
    "zero",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
];

#[derive(Default)]
pub(super) struct Respellings {
    chars: Vec<char>,
    entries: Vec<Spelling>,
}

struct Spelling {
    target: Range<usize>,
    spoken: Range<usize>,
}

struct TargetSpan<'a> {
    span: Range<usize>,
    spellings: &'a [Spelling],
}

impl Respellings {
    fn push(&mut self, word: impl Iterator<Item = char>) -> Range<usize> {
        let start = self.chars.len();
        let mut space = false;
        for c in word {
            if c.is_whitespace() {
                space = self.chars.len() != start;
            } else {
                if space {
                    self.chars.push(' ');
                    space = false;
                }
                self.chars.push(c);
            }
        }
        start..self.chars.len()
    }

    pub(super) fn estimated_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<Spelling>() + self.chars.capacity() * size_of::<char>()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn of(&self, form: &str) -> Vec<String> {
        // With nothing to respell, the scan would only cost the build.
        if self.is_empty() {
            return Vec::new();
        }
        let chars: Vec<char> = form.chars().collect();
        let found = self.targets_in(&chars);
        let mut respellings = Vec::new();
        // One word at a time: every word's first spelling, then every
        // word's second, so pairs onto one word don't crowd out another's.
        for nth in 0..MAX_RESPELLINGS {
            for target in &found {
                if let Some(spoken) = target.spellings.get(nth) {
                    let spoken = &self.chars[spoken.spoken.clone()];
                    respellings.extend(respell(&chars, &[(target.span.clone(), spoken)]));
                    if respellings.len() == MAX_RESPELLINGS {
                        return respellings;
                    }
                }
            }
        }
        if found.len() > 1 {
            let all: Vec<_> = found
                .iter()
                .map(|target| {
                    (
                        target.span.clone(),
                        &self.chars[target.spellings[0].spoken.clone()],
                    )
                })
                .collect();
            respellings.extend(respell(&chars, &all));
        }
        respellings
    }

    fn targets_in<'a>(&'a self, chars: &[char]) -> Vec<TargetSpan<'a>> {
        let mut found = Vec::new();
        let mut position = 0;
        while position < chars.len() {
            match self.target_at(chars, position) {
                Some(target) => {
                    let end = position + target[0].target.len();
                    found.push(TargetSpan {
                        span: position..end,
                        spellings: target,
                    });
                    position = end;
                }
                None => position += 1,
            }
        }
        found
    }

    fn target_at(&self, chars: &[char], position: usize) -> Option<&[Spelling]> {
        let before = position.checked_sub(1).map(|index| chars[index]);
        if before.is_some_and(is_word) && is_word(chars[position]) {
            return None;
        }
        let first = chars[position];
        let start = self
            .entries
            .partition_point(|s| self.chars[s.target.start] < first);
        let entries = &self.entries[start..];
        let end = entries.partition_point(|s| self.chars[s.target.start] == first);
        entries[..end]
            .chunk_by(|a, b| self.chars[a.target.clone()] == self.chars[b.target.clone()])
            .find(|group| {
                let target = &self.chars[group[0].target.clone()];
                let after = chars.get(position + target.len()).copied();
                chars[position..].starts_with(target)
                    && group
                        .iter()
                        .any(|s| fits(before, &self.chars[s.spoken.clone()], after))
            })
    }
}

// A pair finds its spoken side in heard text only at word boundaries, as
// `boundary` decides.
fn fits(before: Option<char>, spoken: &[char], after: Option<char>) -> bool {
    let word = |character: Option<char>| character.is_some_and(is_word);
    word(before) != word(spoken.first().copied()) && word(spoken.last().copied()) != word(after)
}

fn respell(chars: &[char], replacements: &[(Range<usize>, &[char])]) -> Option<String> {
    let mut respelled = Vec::with_capacity(chars.len());
    let mut spans = Vec::new();
    let mut copied = 0;
    for (span, spoken) in replacements {
        respelled.extend_from_slice(&chars[copied..span.start]);
        spans.push(respelled.len()..respelled.len() + spoken.len());
        respelled.extend_from_slice(spoken);
        copied = span.end;
    }
    respelled.extend_from_slice(&chars[copied..]);
    let all_fit = spans.iter().all(|span| {
        let before = span.start.checked_sub(1).map(|index| respelled[index]);
        let after = respelled.get(span.end).copied();
        fits(before, &respelled[span.clone()], after)
    });
    all_fit.then(|| respelled.into_iter().collect())
}

/// How a recognizer renders a compact term: digits as words, a hyphen as
/// a break, a short letter run spelled out. Only a term with a digit
/// expands; "to-do" as "to do" would rewrite everyday speech.
fn expand_term_variants(term: &str) -> Vec<String> {
    let segments = segments(term);
    if segments.len() <= 1
        || segments.len() > MAX_SEGMENTS
        || !segments.iter().any(|segment| is_digits(segment))
    {
        return Vec::new();
    }
    let options: Vec<Vec<String>> = segments
        .iter()
        .map(|segment| {
            if is_digits(segment) {
                let mut options = vec![segment.clone(), spelled_digits(segment)];
                if let Some(word) = one_word(segment) {
                    options.push(word.to_owned());
                }
                options
            } else if segment == "-" || segment == "_" {
                vec![String::new()]
            } else {
                let mut options = vec![segment.clone()];
                if segment.chars().count() <= 3 {
                    options.push(spaced(segment));
                }
                options
            }
        })
        .collect();
    let ok = |variant: &str| {
        !variant.is_empty()
            && !variant.eq_ignore_ascii_case(term)
            && !variant.split_whitespace().all(|token| {
                let token = token.to_lowercase();
                COMMON_TOKENS.contains(&token.as_str())
                    || is_up_to_twenty(&token)
                    || token.chars().all(char::is_numeric)
            })
    };

    // The likeliest rendering first, so the cap never drops it.
    let mut ordered = Vec::new();
    let dehyphenated = collapse(&term.replace(['-', '_'], " "));
    if ok(&dehyphenated) {
        ordered.push(dehyphenated);
    }
    let mut rest = BTreeSet::new();
    for combination in product(&options) {
        let variant = collapse(&combination.join(" "));
        if ok(&variant) && !ordered.contains(&variant) {
            rest.insert(variant);
        }
    }
    ordered.extend(rest);
    ordered.truncate(MAX_VARIANTS);
    ordered
}

/// `[A-Za-z]+`, `[0-9]+`, `-` and `_`; anything else is skipped.
fn segments(term: &str) -> Vec<String> {
    let mut segments: Vec<String> = Vec::new();
    let mut previous: Option<char> = None;
    for c in term.chars() {
        if c == '-' || c == '_' {
            segments.push(c.to_string());
        } else if c.is_ascii_alphanumeric() {
            match (previous, segments.last_mut()) {
                (Some(last), Some(segment))
                    if last.is_ascii_alphanumeric()
                        && last.is_ascii_digit() == c.is_ascii_digit() =>
                {
                    segment.push(c);
                }
                _ => segments.push(c.to_string()),
            }
        } else {
            previous = None;
            continue;
        }
        previous = Some(c);
    }
    segments
}

fn is_digits(segment: &str) -> bool {
    !segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit())
}

/// The one word a recognizer may write for "10" through "20".
fn one_word(segment: &str) -> Option<&'static str> {
    match (segment.len(), segment.parse::<usize>().ok()?) {
        (2, number @ 10..=20) => word_for(number),
        _ => None,
    }
}

fn spelled_digits(segment: &str) -> String {
    segment
        .chars()
        .filter_map(|c| word_for(c.to_digit(10)? as usize))
        .collect::<Vec<_>>()
        .join(" ")
}

fn spaced(segment: &str) -> String {
    segment
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn product(options: &[Vec<String>]) -> Vec<Vec<String>> {
    options
        .iter()
        .fold(vec![Vec::new()], |combinations, choices| {
            combinations
                .iter()
                .flat_map(|combination| {
                    choices.iter().map(move |choice| {
                        let mut next = combination.clone();
                        next.push(choice.clone());
                        next
                    })
                })
                .collect()
        })
}

/// Every term's variants as `(spoken form, term)`. A form that is itself a
/// listed term is dropped: the user's own words are never rewritten. A
/// later term wins a shared form.
fn variant_pairs(terms: &[String]) -> impl Iterator<Item = (String, String)> + '_ {
    let listed: HashSet<String> = terms.iter().map(|term| term.to_lowercase()).collect();
    terms
        .iter()
        .flat_map(|term| {
            expand_term_variants(term)
                .into_iter()
                .map(move |variant| (variant, term.clone()))
        })
        .filter(move |(variant, _)| !listed.contains(&variant.to_lowercase()))
}

/// Every listed casing of each word, by the word in lowercase.
fn casings(terms: &[String]) -> HashMap<String, Vec<&str>> {
    let mut casings: HashMap<String, Vec<&str>> = HashMap::new();
    for term in terms {
        casings.entry(term.to_lowercase()).or_default().push(term);
    }
    casings
}

/// What a learned pair types: its word's listed capitals, so a pair learned
/// as arya → aria types `Aria`.
fn as_listed(listed: &HashMap<String, Vec<&str>>, correct: String) -> String {
    listed
        .get(&correct.to_lowercase())
        .and_then(|listed| capitals(listed))
        .map_or(correct, str::to_owned)
}

/// Each term lowercased, as the fuzzy pass compares it.
fn known(terms: &[String]) -> Vec<Vec<char>> {
    terms
        .iter()
        .map(|term| term.to_lowercase().chars().collect())
        .collect()
}

/// Learned pairs first, then a careful fuzzy pass. Returns the text with
/// each substitution made. A later pair wins a form two pairs share.
#[cfg(test)]
fn apply_corrections<'a>(
    text: &str,
    pairs: impl IntoIterator<Item = &'a (String, String)>,
    terms: &[String],
) -> (String, Vec<(String, String)>) {
    PersonalRules::from_pairs(terms.to_vec(), pairs.into_iter().cloned().collect()).apply(text)
}

/// A regex `\b`: the edge between a word character and anything else.
fn boundary(chars: &[char], position: usize) -> bool {
    let word_at = |index: usize| chars.get(index).copied().is_some_and(is_word);
    let before = position > 0 && word_at(position - 1);
    before != word_at(position)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_workspace_preserves_expanded_and_contextual_lowercase() {
        let rules = PersonalRules::new(vec![], vec![("word".into(), "İΟΣ".into())]);
        let index = rules.respellings(128, &CancelToken::never()).unwrap();
        assert_eq!(index.of("i\u{307}ος dash kit"), ["word dash kit"]);
    }

    #[test]
    fn a_form_gets_at_most_the_bound_of_respellings() {
        let learned = (0..50)
            .map(|index| (format!("arya{index}"), "aria".to_owned()))
            .collect();
        let rules = PersonalRules::new(vec![], learned);
        let respellings = rules
            .respellings(usize::MAX, &CancelToken::never())
            .unwrap()
            .of("aria dash kit");
        assert_eq!(respellings.len(), MAX_RESPELLINGS);
    }

    #[test]
    fn stored_rules_apply_as_new_rules_do() {
        let terms = vec!["db9".to_owned(), "vex16".to_owned()];
        let learned = vec![("db two".to_owned(), "db9".to_owned())];

        let stored = PersonalRules::from_stored(terms, learned, &CancelToken::never()).unwrap();

        let (typed, _) = stored.apply("db two and Vexe16");
        assert_eq!(typed, "db9 and vex16");
    }

    #[test]
    fn a_learned_pair_types_its_word_as_listed() {
        let learned = vec![("arya".to_owned(), "aria".to_owned())];

        let stored =
            PersonalRules::from_stored(strings(&["Aria"]), learned, &CancelToken::never()).unwrap();

        assert_eq!(stored.apply("saying arya now").0, "saying Aria now");
    }

    #[test]
    fn a_learned_pair_for_an_unlisted_word_never_applies() {
        let learned = vec![
            ("ads viser".to_owned(), "Adzviser".to_owned()),
            ("db two".to_owned(), "DB9".to_owned()),
        ];

        let stored =
            PersonalRules::from_stored(strings(&["db9"]), learned, &CancelToken::never()).unwrap();

        let (typed, _) = stored.apply("ads viser and db two");
        assert_eq!(typed, "ads viser and DB9");
    }

    #[test]
    fn loading_stored_rules_stops_when_cancelled() {
        let terms = vec!["db2".to_owned(), "vex16".to_owned()];
        let learned = vec![("db two".to_owned(), "db9".to_owned())];
        let cancelled = CancelToken::new();
        cancelled.cancel();

        assert!(matches!(
            PersonalRules::from_stored(terms, learned, &cancelled),
            Err(MatchError::Cancelled)
        ));
    }

    #[test]
    fn stored_rules_past_the_size_bound_are_refused() {
        let oversized = (0..40_000)
            .map(|index| (format!("name {index}"), "target".repeat(20)))
            .collect();

        assert!(matches!(
            PersonalRules::from_stored(
                strings(&[&"target".repeat(20)]),
                oversized,
                &CancelToken::never()
            ),
            Err(MatchError::TooLarge)
        ));
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    /// Values from Python's difflib, which the thresholds were tuned on.
    #[test]
    fn variants_match_the_python_generator() {
        let cases: [(&str, &[&str]); 7] = [
            ("db2", &["d b 2", "d b two", "db 2", "db two"]),
            (
                "db2-replica",
                &[
                    "db2 replica",
                    "d b 2 replica",
                    "d b two replica",
                    "db 2 replica",
                    "db two replica",
                ],
            ),
            ("no1", &["n o 1", "n o one"]),
            ("to-do", &[]),
            ("a1", &[]),
            ("k8s", &["k 8 s", "k eight s"]),
            ("x12", &["x 12", "x one two", "x twelve"]),
        ];
        for (term, expected) in cases {
            assert_eq!(expand_term_variants(term), strings(expected), "{term}");
        }
    }

    #[test]
    fn the_cap_keeps_the_likeliest_renderings() {
        let variants = expand_term_variants("gpt4-turbo-max");
        assert_eq!(variants.len(), MAX_VARIANTS);
        assert_eq!(variants[0], "gpt4 turbo max");
    }

    #[test]
    fn a_unicode_digit_never_panics() {
        assert!(expand_term_variants("x٣").is_empty());
        assert!(expand_term_variants("x-٣").is_empty());
    }

    #[test]
    fn a_variant_that_is_another_term_is_dropped() {
        assert_eq!(
            variant_pairs(&strings(&["db2", "db two"])).collect::<Vec<_>>(),
            pairs(&[("d b 2", "db2"), ("d b two", "db2"), ("db 2", "db2")])
        );
    }

    #[test]
    fn pairs_apply_whole_words_case_insensitively_in_one_pass() {
        let (text, subs) = apply_corrections(
            "run d b two now and Vexe16 please",
            &pairs(&[("d b two", "db2")]),
            &strings(&["vex16"]),
        );
        assert_eq!(text, "run db2 now and vex16 please");
        assert_eq!(subs, pairs(&[("d b two", "db2"), ("Vexe16", "vex16")]));

        let (text, _) = apply_corrections(
            "D B TWO and db two.",
            &pairs(&[("d b two", "db2"), ("db two", "db2")]),
            &[],
        );
        assert_eq!(text, "db2 and db2.");

        let (text, subs) =
            apply_corrections("db2 ok", &pairs(&[("db2", "sp2"), ("sp2", "sp3")]), &[]);
        assert_eq!((text.as_str(), subs.len()), ("sp2 ok", 1));
        let (text, _) = apply_corrections("midb2 db2x", &pairs(&[("db2", "x")]), &[]);
        assert_eq!(text, "midb2 db2x");
        let accented = "e\u{301}db2 db2e\u{301}";
        let (text, _) = apply_corrections(accented, &pairs(&[("db2", "x")]), &[]);
        assert_eq!(text, accented);
    }

    #[test]
    fn a_later_pair_wins_the_same_form() {
        let (text, _) = apply_corrections(
            "db two",
            &pairs(&[("db two", "db2"), ("DB TWO", "db9")]),
            &[],
        );
        assert_eq!(text, "db9");
    }

    #[test]
    fn fuzzy_fixes_a_near_miss_and_leaves_the_rest() {
        let terms = strings(&[
            "kubernetes",
            "wespeaker",
            "vex16",
            "Flickr",
            "Rivers",
            "Redshift",
            "Colours",
            "Theyre",
        ]);
        let cases = [
            ("kubernets rocks", "kubernetes rocks"),
            ("the speaker is loud", "the speaker is loud"),
            ("kubernetes is fine", "kubernetes is fine"),
            ("ok", "ok"),
            ("vexe16 boots", "vex16 boots"),
            ("a flick of the wrist", "a flick of the wrist"),
            ("the river's edge", "the river's edge"),
            ("a red-shift", "a red-shift"),
            ("colour me", "colour me"),
            ("they're here", "they're here"),
        ];
        for (input, expected) in cases {
            assert_eq!(apply_corrections(input, &[], &terms).0, expected, "{input}");
        }
        assert_eq!(apply_corrections("kubernets", &[], &[]).0, "kubernets");
    }

    #[test]
    fn a_listed_word_heard_as_two_words_merges() {
        let rules = PersonalRules::new(strings(&["PostgreSQL", "kubernetes", "Redshift"]), vec![]);
        let cases = [
            ("use postgre sql now", "use PostgreSQL now"),
            ("deploy kuber nettes", "deploy kubernetes"),
            ("a red shift", "a red shift"),
            ("the postgre sql's config", "the PostgreSQL's config"),
            ("deploy Kuber Netes", "deploy kubernetes"),
            ("in src/postgre sql", "in src/postgre sql"),
            ("postgre sql.conf", "postgre sql.conf"),
        ];
        for (input, expected) in cases {
            assert_eq!(rules.apply(input).0, expected, "{input}");
        }
    }

    #[rstest::rstest]
    #[case("postgresql's config", "PostgreSQL's config")]
    #[case("postgre SQL'S config", "PostgreSQL'S config")]
    #[case("postgre's sql", "postgre's sql")]
    #[case("use 'gitlab' now", "use 'GitLab' now")]
    #[case("use ai, not aix", "use AI, not aix")]
    #[case("an éclair", "an éclair")]
    #[case("an e\u{301}clair", "an e\u{301}clair")]
    #[case("épostgre sql", "épostgre sql")]
    #[case("postgre sqlé", "postgre sqlé")]
    #[case("postgresql-", "PostgreSQL-")]
    #[case("a mcdonald's", "a McDonald's")]
    fn a_correction_keeps_the_rest_of_the_word(#[case] heard: &str, #[case] expected: &str) {
        let rules = PersonalRules::new(
            strings(&["PostgreSQL", "GitLab", "AI", "Clair", "McDonald's"]),
            vec![],
        );
        assert_eq!(rules.apply(heard).0, expected);
    }

    #[test]
    fn a_short_listed_word_leaves_the_real_word_it_spells() {
        let rules = PersonalRules::new(strings(&["IT", "US", "AI", "ID", "MS"]), vec![]);
        assert_eq!(rules.apply("is it with us").0, "is it with us");
        assert_eq!(
            rules.apply("ask ai for my id in ms").0,
            "ask AI for my ID in MS"
        );
    }

    #[test]
    fn a_short_word_never_merges_into_a_listed_word() {
        let rules = PersonalRules::new(strings(&["Postgres", "Atlas"]), vec![]);
        for heard in ["postgr is running", "meet at Las Vegas"] {
            assert_eq!(rules.apply(heard).0, heard);
        }
    }

    #[test]
    fn a_word_the_list_knows_stays_out_of_a_merge() {
        let cases = [
            (["kuber", "kubernetes"], "deploy kuber netes"),
            (["netes", "kubernetes"], "deploy kuber netes"),
            (["Nettes", "kubernetes"], "deploy kuber Nettes"),
        ];
        for (listed, expected) in cases {
            let rules = PersonalRules::new(strings(&listed), vec![]);
            assert_eq!(rules.apply("deploy kuber netes").0, expected, "{listed:?}");
        }
    }

    #[test]
    fn a_listed_capital_types_as_listed() {
        let rules = PersonalRules::new(strings(&["Aria", "kubernetes"]), vec![]);
        let cases = [
            ("saying aria now", "saying Aria now"),
            ("ARIA is here", "Aria is here"),
            ("Kubernetes is fine", "Kubernetes is fine"),
        ];
        for (input, expected) in cases {
            assert_eq!(rules.apply(input).0, expected, "{input}");
        }
        let both = PersonalRules::new(strings(&["Aria", "aria"]), vec![]);
        assert_eq!(both.apply("saying aria now").0, "saying aria now");
    }

    #[test]
    fn a_near_miss_stays_out_of_written_names() {
        let rules = PersonalRules::new(strings(&["kubernetes"]), vec![]);
        assert_eq!(
            rules.apply("in src/kubernets.yaml").0,
            "in src/kubernets.yaml"
        );
        assert_eq!(rules.apply("kubernets rocks").0, "kubernetes rocks");
    }

    #[test]
    fn a_near_miss_keeps_its_own_digits() {
        let rules = PersonalRules::new(strings(&["db2", "Nvidia\u{ff12}"]), vec![]);
        for heard in ["check db12 now", "install nvidia"] {
            assert_eq!(rules.apply(heard).0, heard);
        }
    }

    #[test]
    fn a_fix_stays_out_of_flags_and_variables() {
        let rules = PersonalRules::new(strings(&["ID", "Kubernetes"]), vec![]);
        for heard in [
            "pass --id to it",
            "open src/-id",
            "run -kubernets now",
            "check $id",
            "echo ${id}",
            "pass --flag=id",
            "set id=5 first",
            "export id=\"abc\"",
            "export id=",
        ] {
            assert_eq!(rules.apply(heard).0, heard);
        }
    }

    #[test]
    fn a_near_miss_equally_close_to_two_listed_words_stays_as_heard() {
        let one = PersonalRules::new(strings(&["Lumenforge"]), vec![]);
        assert_eq!(one.apply("call Lumenforg now").0, "call Lumenforge now");
        for listed in [
            ["Lumenforge", "Lumenforga"],
            ["Lumenforge", "LumenForge"],
            ["LumenForge", "Lumenforge"],
        ] {
            let two = PersonalRules::new(strings(&listed), vec![]);
            assert_eq!(two.apply("call Lumenforg now").0, "call Lumenforg now");
        }
    }

    #[test]
    fn a_listed_capital_holds_where_it_is_also_a_real_word() {
        let rules = PersonalRules::new(strings(&["Linear"]), vec![]);
        assert_eq!(rules.apply("a linear model").0, "a Linear model");
    }

    #[test]
    fn a_listed_capital_stays_out_of_written_names() {
        let rules = PersonalRules::new(strings(&["Aria", "Rust"]), vec![]);
        for heard in [
            "open aria-kit and rust-analyzer",
            "in src/rust/aria.toml",
            "call aria_kit and @aria",
        ] {
            assert_eq!(rules.apply(heard).0, heard);
        }
        assert_eq!(rules.apply("I like aria.").0, "I like Aria.");
    }

    #[test]
    fn two_listed_casings_of_a_word_stay_as_heard() {
        let rules = PersonalRules::new(strings(&["Mac", "MAC"]), vec![]);
        for heard in ["the MAC address", "a Mac here", "my mac"] {
            assert_eq!(rules.apply(heard).0, heard);
        }
    }

    #[test]
    fn a_serial_number_gets_no_spoken_forms() {
        assert!(expand_term_variants("a1-b2-c3-d4-e5-f6-g7-h8-i9-j0").is_empty());
        assert_eq!(expand_term_variants("k8s").len(), 2);
    }
}
