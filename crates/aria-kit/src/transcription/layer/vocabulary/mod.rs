use std::borrow::Cow;
use std::collections::HashMap;
use std::convert::Infallible;

use crate::cancel::CancelToken;

mod personal;
pub use personal::PersonalRules;

use super::claim::{Claimed, Word};
use super::symbol_words;

const MAX_TERMS: usize = 100_000;
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub canonical: String,
    pub priority: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum MatchError {
    #[error("vocabulary compilation was cancelled")]
    Cancelled,
    #[error("vocabulary exceeds the snapshot limit")]
    TooLarge,
    #[error("vocabulary contains an invalid name")]
    InvalidName,
}

struct Candidate {
    name: Name,
    priority: u32,
    protected: bool,
}

enum Name {
    Spelled(String),
    // Only a symbol shows that words are a name, so a name without one
    // keeps the speaker's casing.
    Heard(String),
    // No name wins a form two names share.
    Shared,
}

impl Name {
    fn new(canonical: &str, protected: bool) -> Self {
        if protected && !has_symbol(canonical) {
            Self::Heard(canonical.to_owned())
        } else {
            Self::Spelled(canonical.to_owned())
        }
    }

    fn canonical(&self) -> Option<&str> {
        match self {
            Self::Spelled(canonical) | Self::Heard(canonical) => Some(canonical),
            Self::Shared => None,
        }
    }

    fn typed(&self) -> Option<&str> {
        match self {
            Self::Spelled(canonical) => Some(canonical),
            Self::Heard(_) | Self::Shared => None,
        }
    }
}

#[derive(Default)]
struct Bucket {
    aliases: HashMap<String, Candidate>,
    max_chars: usize,
}

#[derive(Default)]
pub struct Vocabulary {
    index: HashMap<String, Bucket>,
    personal: PersonalRules,
    term_count: usize,
    estimated_bytes: usize,
}

impl Vocabulary {
    pub fn new(terms: impl IntoIterator<Item = impl AsRef<str>>) -> Result<Self, MatchError> {
        let mut ranked = Vec::new();
        for term in terms {
            if ranked.len() == MAX_TERMS {
                return Err(MatchError::TooLarge);
            }
            let canonical = term.as_ref();
            validate_name(canonical)?;
            ranked.push(Term {
                canonical: canonical.to_owned(),
                priority: 1,
            });
        }
        Self::compile(&ranked, PersonalRules::default())
    }

    pub fn compile(terms: &[Term], personal: PersonalRules) -> Result<Self, MatchError> {
        Self::compile_with(terms, personal, &CancelToken::never())
    }

    pub fn compile_with(
        terms: &[Term],
        personal: PersonalRules,
        cancel: &CancelToken,
    ) -> Result<Self, MatchError> {
        if cancel.is_cancelled() {
            return Err(MatchError::Cancelled);
        }
        if terms.len() > MAX_TERMS || personal.estimated_bytes() > MAX_SNAPSHOT_BYTES {
            return Err(MatchError::TooLarge);
        }
        let mut matcher = Self {
            index: HashMap::new(),
            personal,
            term_count: terms.len(),
            estimated_bytes: 0,
        };
        let mut bytes = std::mem::size_of::<Self>() + matcher.personal.estimated_bytes();
        for term in terms {
            if cancel.is_cancelled() {
                return Err(MatchError::Cancelled);
            }
            validate_name(&term.canonical)?;
            for (index, alias) in matching_forms(&term.canonical).enumerate() {
                let protected = index == 0;
                if cancel.is_cancelled() {
                    return Err(MatchError::Cancelled);
                }
                let Some(first) = first_word(&alias) else {
                    continue;
                };
                let capacity = matcher.index.capacity();
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    matcher.index.entry(first.clone())
                {
                    bytes += first.capacity();
                    entry.insert(Bucket::default());
                }
                bytes += (matcher.index.capacity() - capacity)
                    * 2
                    * (std::mem::size_of::<(String, Bucket)>() + 1);
                let bucket = matcher.index.get_mut(&first).expect("inserted bucket");
                bucket.max_chars = bucket.max_chars.max(alias.chars().count());
                match bucket.aliases.get_mut(&alias) {
                    Some(existing) if existing.protected => {
                        if protected && existing.name.canonical() != Some(&term.canonical) {
                            existing.name = Name::Shared;
                        }
                    }
                    Some(existing) if protected || term.priority > existing.priority => {
                        bytes += term.canonical.len();
                        *existing = Candidate {
                            name: Name::new(&term.canonical, protected),
                            priority: term.priority,
                            protected,
                        };
                    }
                    Some(existing)
                        if term.priority == existing.priority
                            && existing.name.canonical() != Some(&term.canonical) =>
                    {
                        existing.name = Name::Shared;
                    }
                    Some(_) => {}
                    None => {
                        bytes += alias.capacity() + term.canonical.len();
                        let capacity = bucket.aliases.capacity();
                        bucket.aliases.insert(
                            alias,
                            Candidate {
                                name: Name::new(&term.canonical, protected),
                                priority: term.priority,
                                protected,
                            },
                        );
                        bytes += (bucket.aliases.capacity() - capacity)
                            * 2
                            * (std::mem::size_of::<(String, Candidate)>() + 1);
                    }
                }
                if bytes > MAX_SNAPSHOT_BYTES {
                    return Err(MatchError::TooLarge);
                }
            }
        }
        for (alias, canonical) in matcher.personal.pairs() {
            if cancel.is_cancelled() {
                return Err(MatchError::Cancelled);
            }
            let alias = normalize(&alias);
            if let Some(candidate) = first_word(&alias)
                .and_then(|first| matcher.index.get_mut(&first))
                .and_then(|bucket| bucket.aliases.get_mut(&alias))
                && !candidate.protected
                && candidate.name.canonical() != Some(canonical)
            {
                candidate.name = Name::Shared;
            }
        }
        if cancel.is_cancelled() {
            return Err(MatchError::Cancelled);
        }
        matcher.estimated_bytes = bytes;
        Ok(matcher)
    }

    pub fn term_count(&self) -> usize {
        self.term_count
    }
    pub fn estimated_bytes(&self) -> usize {
        self.estimated_bytes
    }

    pub fn correct(&self, text: &str) -> Option<String> {
        self.correct_checked(text, &|_| Ok::<_, std::convert::Infallible>(()))
            .unwrap_or_else(|never| match never {})
    }

    pub(super) fn correct_checked<E>(
        &self,
        text: &str,
        check: &impl Fn(usize) -> Result<(), E>,
    ) -> Result<Option<String>, E> {
        if self.index.is_empty() {
            check(text.len())?;
            let (corrected, _) = self.personal.apply_checked(text, check)?;
            return Ok((corrected != text).then_some(corrected));
        }
        self.correct_with(text, |_| {}, check)
    }

    /// `text` corrected by `claim`, between reading its words, which claims
    /// vocabulary terms, and typing them, which applies personal rules to
    /// what nothing claimed.
    pub(super) fn correct_with<'a, E>(
        &'a self,
        text: &'a str,
        claim: impl FnOnce(&mut Claimed<'a>),
        check: &impl Fn(usize) -> Result<(), E>,
    ) -> Result<Option<String>, E> {
        let claimed = self.claimed(text, claim, check)?;
        self.typed(text, claimed.words(), check)
    }

    pub(super) fn claimed<'a, E>(
        &'a self,
        text: &'a str,
        claim: impl FnOnce(&mut Claimed<'a>),
        check: &impl Fn(usize) -> Result<(), E>,
    ) -> Result<Claimed<'a>, E> {
        check(text.len())?;
        let mut claimed = Claimed::new(self.words(text, check)?, text.len());
        claim(&mut claimed);
        Ok(claimed)
    }

    /// Like [`Self::correct_with`], for dictated text, which nothing
    /// cancels. See [`Self::claimed_spoken`] for `typed_len`.
    pub(super) fn correct_spoken_with<'a>(
        &'a self,
        text: &'a str,
        typed_len: usize,
        claim: impl FnOnce(&mut Claimed<'a>),
    ) -> Option<String> {
        let claimed = self.claimed_spoken(text, typed_len, claim);
        self.typed(text, claimed.words(), &no_check)
            .unwrap_or_else(|never| match never {})
    }

    /// Like [`Self::claimed`], for dictated text, which nothing cancels.
    /// Its first `typed_len` bytes are a word already typed. That word types
    /// as it stands: no term reads across it, and no step changes it.
    pub(super) fn claimed_spoken<'a>(
        &'a self,
        text: &'a str,
        typed_len: usize,
        claim: impl FnOnce(&mut Claimed<'a>),
    ) -> Claimed<'a> {
        let mut words = Vec::new();
        if typed_len > 0 {
            words.push(Word::claimed(
                0..typed_len,
                Cow::Borrowed(&text[..typed_len]),
            ));
        }
        let heard = self
            .words(&text[typed_len..], &no_check)
            .unwrap_or_else(|never| match never {});
        words.extend(heard.into_iter().map(|word| Word {
            span: word.span.start + typed_len..word.span.end + typed_len,
            ..word
        }));
        let mut claimed = Claimed::new(words, text.len());
        claim(&mut claimed);
        claimed
    }

    pub(super) fn claim_forms<'a>(&'a self, text: &str, claimed: &mut Claimed<'a>) {
        self.personal.claim(text, claimed);
    }

    pub(super) fn defer_waiting_forms(&self, text: &str, claimed: &mut Claimed<'_>) {
        self.personal.defer_waiting_forms(text, claimed);
    }

    /// `text` with each claim typed, and personal rules applied to the
    /// spans nothing claimed.
    fn typed<E>(
        &self,
        text: &str,
        words: &[Word<'_>],
        check: &impl Fn(usize) -> Result<(), E>,
    ) -> Result<Option<String>, E> {
        let mut output = String::with_capacity(text.len());
        let mut unclaimed = 0;
        for word in words {
            let Some(typed) = &word.claim else {
                continue;
            };
            let (personal, _) = self
                .personal
                .apply_checked(&text[unclaimed..word.span.start], &|bytes| {
                    check(output.len().saturating_add(bytes))
                })?;
            output.push_str(&personal);
            check(output.len().saturating_add(typed.len()))?;
            output.push_str(typed);
            unclaimed = word.span.end;
        }
        let (personal, _) = self.personal.apply_checked(&text[unclaimed..], &|bytes| {
            check(output.len().saturating_add(bytes))
        })?;
        output.push_str(&personal);
        Ok((output != text).then_some(output))
    }

    fn words<'a, E>(
        &'a self,
        text: &'a str,
        check: &impl Fn(usize) -> Result<(), E>,
    ) -> Result<Vec<Word<'a>>, E> {
        let plain = Word::plain;
        let mut words = Vec::new();
        let mut word_start = None;
        let mut position = 0;
        while position < text.len() {
            check(0)?;
            let rest = &text[position..];
            let character = rest.chars().next().unwrap();
            if character.is_whitespace() {
                if let Some(start) = word_start.take() {
                    words.push(plain(start..position));
                }
                position += character.len_utf8();
                continue;
            }
            let matched = if !continues_word(text[..position].chars().rev()) {
                match first_word(rest).and_then(|first| self.index.get(&first)) {
                    Some(bucket) => self.find(text, position, bucket, check)?,
                    None => None,
                }
            } else {
                None
            };
            let Some((end, candidate)) = matched else {
                word_start.get_or_insert(position);
                position += character.len_utf8();
                continue;
            };
            if let Some(start) = word_start.take() {
                words.push(plain(start..position));
            }
            let term = candidate.name.typed().unwrap_or(&text[position..end]);
            words.push(Word::claimed(position..end, Cow::Borrowed(term)));
            position = end;
        }
        if let Some(start) = word_start {
            words.push(plain(start..text.len()));
        }
        Ok(words)
    }

    fn find<'a, E>(
        &self,
        text: &str,
        start: usize,
        bucket: &'a Bucket,
        check: &impl Fn(usize) -> Result<(), E>,
    ) -> Result<Option<(usize, &'a Candidate)>, E> {
        let mut normalized = String::new();
        let mut count = 0;
        let mut whitespace = false;
        let mut longest = None;
        for (offset, character) in text[start..].char_indices() {
            check(0)?;
            if character.is_whitespace() {
                if whitespace {
                    continue;
                }
                normalized.push(' ');
                count += 1;
            } else {
                for folded in character.to_lowercase() {
                    normalized.push(folded);
                    count += 1;
                }
            }
            whitespace = character.is_whitespace();
            if count > bucket.max_chars {
                break;
            }
            let end = start + offset + character.len_utf8();
            if !continues_word(text[end..].chars())
                && let Some(candidate) = bucket.aliases.get(&normalized)
            {
                longest = Some((end, candidate));
            }
        }
        Ok(longest)
    }
}

fn validate_name(name: &str) -> Result<(), MatchError> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name.chars().any(char::is_control)
        || name.trim() != name
    {
        return Err(MatchError::InvalidName);
    }
    Ok(())
}

// A symbol with no word beyond it, as a sentence's final dot, ends the word.
fn continues_word(mut outward: impl Iterator<Item = char>) -> bool {
    match outward.next() {
        Some(character) if is_word(character) => true,
        Some(character) if symbol_words::is_symbol(character) => {
            outward.next().is_some_and(is_word)
        }
        _ => false,
    }
}

fn is_word(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn first_word(text: &str) -> Option<String> {
    let first = text.chars().next()?;
    if first.is_whitespace() {
        return None;
    }
    if !is_word(first) {
        return Some(first.to_string());
    }
    Some(
        text.chars()
            .take_while(|c| is_word(*c))
            .flat_map(char::to_lowercase)
            .take_while(|c| is_word(*c))
            .collect(),
    )
}

pub fn matching_forms(canonical: &str) -> impl Iterator<Item = String> {
    std::iter::once(normalize(canonical)).chain(aliases(canonical))
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// A recognizer writes "dash" for `-`, but dictation's symbol words call it
// "hyphen".
fn spoken_symbol(character: char, hyphen: &'static str) -> Option<&'static str> {
    match character {
        '-' => Some(hyphen),
        _ => symbol_words::word(character),
    }
}

fn no_check(_: usize) -> Result<(), Infallible> {
    Ok(())
}

#[derive(Clone, Copy)]
enum Said {
    Whole,
    Words,
    Letters,
}

// Every form says a symbol, so words alone never match a name.
fn aliases(canonical: &str) -> Vec<String> {
    if !has_symbol(canonical) {
        return Vec::new();
    }
    let mut aliases = Vec::new();
    let hyphens = if canonical.contains('-') { 2 } else { 1 };
    for hyphen in ["dash"]
        .into_iter()
        .chain(symbol_words::word('-'))
        .take(hyphens)
    {
        for said in [Said::Whole, Said::Words, Said::Letters] {
            let mut words: Vec<String> = Vec::new();
            let mut part = String::new();
            for character in canonical.chars() {
                let Some(symbol) = spoken_symbol(character, hyphen) else {
                    part.push(character);
                    continue;
                };
                push_part(&part, said, &mut words);
                part.clear();
                words.push(symbol.to_owned());
            }
            push_part(&part, said, &mut words);
            let alias = normalize(&words.join(" "));
            if alias != canonical.to_lowercase() && !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
    }
    aliases
}

fn has_symbol(name: &str) -> bool {
    name.chars().any(symbol_words::is_symbol)
}

fn push_part(part: &str, said: Said, words: &mut Vec<String>) {
    if part.is_empty() {
        return;
    }
    match said {
        Said::Letters if part.len() <= 3 && part.chars().all(|c| c.is_ascii_alphabetic()) => {
            words.extend(part.chars().map(|c| c.to_ascii_lowercase().to_string()));
        }
        Said::Words => words.extend(spoken_words(part).into_iter().map(str::to_lowercase)),
        Said::Whole | Said::Letters => words.push(part.to_lowercase()),
    }
}

fn spoken_words(part: &str) -> Vec<&str> {
    let characters: Vec<(usize, char)> = part.char_indices().collect();
    let mut words = Vec::new();
    let mut start = 0;
    for index in 1..characters.len() {
        let (offset, character) = characters[index];
        let previous = characters[index - 1].1;
        let lower_follows = characters
            .get(index + 1)
            .is_some_and(|(_, next)| next.is_lowercase());
        if previous.is_numeric() != character.is_numeric()
            || (previous.is_lowercase() && character.is_uppercase())
            || (previous.is_uppercase() && character.is_uppercase() && lower_follows)
        {
            words.push(&part[start..offset]);
            start = offset;
        }
    }
    words.push(&part[start..]);
    words
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[test]
    fn checked_correction_bounds_personal_fuzzy_and_accumulated_output() {
        let check = |bytes| if bytes > 16 { Err("too large") } else { Ok(()) };
        let personal = Vocabulary::compile(
            &[],
            PersonalRules::new(vec![], vec![("x".into(), "y".repeat(32))]),
        )
        .unwrap();
        assert_eq!(personal.correct_checked("x", &check), Err("too large"));
        assert_eq!(personal.correct("x"), Some("y".repeat(32)));

        let fuzzy = Vocabulary::compile(&[], PersonalRules::new(vec!["kubernetes".into()], vec![]))
            .unwrap();
        assert_eq!(
            fuzzy.correct_checked("kubernets rocks!!", &check),
            Err("too large")
        );

        let combined = Vocabulary::compile(
            &[Term {
                canonical: "aria-stt".into(),
                priority: 1,
            }],
            PersonalRules::new(vec![], vec![("x".into(), "y".repeat(8))]),
        )
        .unwrap();
        assert_eq!(
            combined.correct_checked("aria-stt x", &check),
            Err("too large")
        );
        assert_eq!(
            combined.correct("aria-stt x"),
            Some(format!("aria-stt {}", "y".repeat(8)))
        );
    }

    #[test]
    fn canonical_lists_correct_structural_forms_without_fuzzy_matching() {
        let vocabulary = Vocabulary::new(["aria-stt", "Cargo.toml", "kubernetes"]).unwrap();
        assert_eq!(vocabulary.term_count(), 3);
        assert_eq!(
            vocabulary.correct("aria dash stt and cargo dot toml"),
            Some("aria-stt and Cargo.toml".into())
        );
        assert_eq!(vocabulary.correct("kubernets"), None);
    }

    #[rstest]
    #[case::hyphen_names_a_dash("aria hyphen stt", "aria-stt")]
    #[case::camel_case_part_as_words("open app delegate dot swift", "open AppDelegate.swift")]
    #[case::acronym_part_as_words("html parser dot swift", "HTMLParser.swift")]
    fn spoken_forms_type_the_stored_name(#[case] heard: &str, #[case] typed: &str) {
        let vocabulary =
            Vocabulary::new(["aria-stt", "AppDelegate.swift", "HTMLParser.swift"]).unwrap();
        assert_eq!(vocabulary.correct(heard).as_deref(), Some(typed));
    }

    #[rstest]
    #[case::spelled_letters("aria s t t")]
    #[case::words_alone("app delegate swift")]
    fn a_form_with_no_symbol_word_stays_as_heard(#[case] heard: &str) {
        let vocabulary = Vocabulary::new(["aria-stt", "AppDelegate.swift"]).unwrap();
        assert_eq!(vocabulary.correct(heard), None);
    }

    #[rstest]
    #[case::hyphen_after_a_capital("Aria hyphen kit", "aria-kit")]
    #[case::extension_in_capitals("main dot RS", "main.rs")]
    fn dictated_names_keep_the_stored_casing(#[case] heard: &str, #[case] typed: &str) {
        let vocabulary = Vocabulary::new(["aria-kit", "main.rs"]).unwrap();
        let corrected = vocabulary.correct_spoken(heard, "").map(|c| c.text);
        assert_eq!(corrected.as_deref(), Some(typed));
    }

    #[rstest]
    #[case::joined_by_the_recognizer("open appdelegate.swift", "open AppDelegate.swift")]
    #[case::other_casing("ARIA-kit.", "aria-kit.")]
    fn a_written_name_takes_the_stored_casing(#[case] heard: &str, #[case] typed: &str) {
        let vocabulary = Vocabulary::new(["AppDelegate.swift", "aria-kit"]).unwrap();
        assert_eq!(vocabulary.correct(heard).as_deref(), Some(typed));
    }

    #[rstest]
    #[case::inside_a_hyphen_join("My-Aria-Kit")]
    #[case::after_a_slash("src/Main.rs")]
    #[case::before_a_dot_join("Main.rs.bak")]
    fn a_name_inside_a_longer_written_name_stays_as_heard(#[case] heard: &str) {
        let vocabulary = Vocabulary::new(["aria-kit", "main.rs"]).unwrap();
        assert_eq!(vocabulary.correct(heard), None);
    }

    #[test]
    fn names_that_differ_only_by_case_keep_the_heard_casing() {
        let vocabulary = Vocabulary::new(["aria-kit", "Aria-Kit"]).unwrap();
        assert_eq!(vocabulary.correct("ARIA-kit"), None);
        assert_eq!(vocabulary.correct("aria dash kit"), None);
    }

    #[test]
    fn canonical_lists_accept_empty_input_and_reject_invalid_names() {
        let vocabulary = Vocabulary::new(std::iter::empty::<&str>()).unwrap();
        assert_eq!(vocabulary.term_count(), 0);
        assert_eq!(vocabulary.correct("unchanged"), None);
        for invalid in ["", " padded", "padded ", "bad\nname", "bad\0name"] {
            assert!(matches!(
                Vocabulary::new([invalid]),
                Err(MatchError::InvalidName)
            ));
        }
        assert!(matches!(
            Vocabulary::new(["é".repeat(MAX_NAME_BYTES / 2 + 1)]),
            Err(MatchError::InvalidName)
        ));
    }

    #[test]
    fn canonical_lists_bound_iteration_and_compiled_snapshot_size() {
        let consumed = std::cell::Cell::new(0);
        let terms = (0..MAX_TERMS + 2).map(|_| {
            consumed.set(consumed.get() + 1);
            "aria-stt"
        });
        assert!(matches!(Vocabulary::new(terms), Err(MatchError::TooLarge)));
        assert_eq!(consumed.get(), MAX_TERMS + 1);

        let terms = (0..20_000).map(|index| format!("project{index:05}-{}-stt", "x".repeat(230)));
        assert!(matches!(Vocabulary::new(terms), Err(MatchError::TooLarge)));
    }

    fn compile(names: &[&str], personal: PersonalRules) -> Vocabulary {
        let terms: Vec<_> = names
            .iter()
            .map(|name| Term {
                canonical: (*name).to_owned(),
                priority: 1,
            })
            .collect();
        Vocabulary::compile(&terms, personal).expect("valid vocabulary")
    }

    #[test]
    fn structural_aliases_preserve_punctuation_and_original_bytes() {
        let matcher = compile(
            &["aria-stt", "Cargo.toml", "écho-stt"],
            PersonalRules::default(),
        );
        assert_eq!(
            matcher.correct("Look: ARIA dash S T T; cargo dot toml!"),
            Some("Look: aria-stt; Cargo.toml!".to_owned())
        );
        assert_eq!(
            matcher.correct("écho\t dash   stt 😊"),
            Some("écho-stt 😊".to_owned())
        );
        assert_eq!(matcher.correct("aria dot stt"), None);
        let matcher = compile(&["CI/CD"], PersonalRules::default());
        assert_eq!(
            matcher.correct("run c i slash c d"),
            Some("run CI/CD".to_owned())
        );
        assert_eq!(matcher.correct("run c i c d"), None);
        assert_eq!(matcher.correct("xaria dash stt aria dash sttx"), None);
    }

    #[test]
    fn exact_identifiers_and_project_outputs_do_not_enter_personal_fuzzy_matching() {
        let personal = PersonalRules::new(
            vec!["aria-sttt".into()],
            vec![("aria-stt".into(), "wrong".into())],
        );
        let matcher = compile(&["aria-stt"], personal);
        assert_eq!(matcher.correct("aria-stt"), None);
        assert_eq!(matcher.correct("aria dash stt"), Some("aria-stt".into()));
    }

    #[test]
    fn unicode_case_expansion_and_scoped_identifiers_keep_their_boundaries() {
        let matcher = compile(
            &["İstanbul-stt", "@scope/kubernets"],
            PersonalRules::new(vec!["kubernetes".into()], vec![]),
        );
        assert_eq!(
            matcher.correct("İstanbul dash stt!"),
            Some("İstanbul-stt!".into())
        );
        assert_eq!(matcher.correct("@scope/kubernets"), None);
        assert_eq!(matcher.correct("éİstanbul dash stt"), None);
        let matcher = compile(&["to-do", "Cargo.toml"], PersonalRules::default());
        assert_eq!(matcher.correct("I have things to do with cargo toml"), None);
    }

    #[test]
    fn collisions_are_protected_and_specific_scope_wins() {
        let names = ["AppDelegate.swift", "app delegate.swift"];
        let matcher = compile(&names, PersonalRules::default());
        assert_eq!(matcher.correct("app delegate dot swift"), None);
        let matcher = Vocabulary::compile(
            &[
                Term {
                    canonical: names[0].into(),
                    priority: 2,
                },
                Term {
                    canonical: names[1].into(),
                    priority: 1,
                },
            ],
            PersonalRules::default(),
        )
        .unwrap();
        assert_eq!(
            matcher.correct("app delegate dot swift"),
            Some(names[0].into())
        );
        let personal = PersonalRules::new(vec![], vec![("aria dash stt".into(), "another".into())]);
        let matcher = compile(&["aria-stt"], personal);
        assert_eq!(matcher.correct("aria dash stt"), None);
    }

    #[test]
    fn rejected_longer_boundary_does_not_hide_valid_shorter_match() {
        let matcher = compile(&["aria-stt", "aria-stt-extra"], PersonalRules::default());
        assert_eq!(
            matcher.correct("aria dash stt dash extras"),
            Some("aria-stt dash extras".into())
        );
        assert_eq!(
            matcher.correct("aria dash stt dash extra"),
            Some("aria-stt-extra".into())
        );
    }

    #[test]
    fn each_segment_split_keeps_original_text_until_a_complete_alias_exists() {
        let matcher = compile(&["aria-stt"], PersonalRules::default());
        let words = ["aria", "dash", "stt"];
        for split in 1..words.len() {
            assert_eq!(matcher.correct(&words[..split].join(" ")), None);
            assert_eq!(matcher.correct(&words[split..].join(" ")), None);
        }
        assert_eq!(matcher.correct(&words.join(" ")), Some("aria-stt".into()));
    }

    #[test]
    fn personal_only_snapshot_preserves_pair_order_and_fuzzy_behavior() {
        let personal = PersonalRules::new(
            vec!["db2".into(), "vex16".into()],
            vec![("db two".into(), "db9".into())],
        );
        let matcher = compile(&[], personal);
        assert_eq!(
            matcher.correct("db two and Vexe16"),
            Some("db9 and vex16".into())
        );
    }

    #[test]
    fn cancellable_compilation_checks_cancellation_during_aliases() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let checks = AtomicUsize::new(0);
        let cancel = CancelToken::from_fn(move || checks.fetch_add(1, Ordering::Relaxed) >= 3);
        assert!(matches!(
            Vocabulary::compile_with(
                &[Term {
                    canonical: "aria-stt".into(),
                    priority: 1
                }],
                PersonalRules::default(),
                &cancel,
            ),
            Err(MatchError::Cancelled)
        ));
    }

    #[test]
    fn invalid_and_oversized_vocabularies_fail_instead_of_truncating() {
        assert!(matches!(
            Vocabulary::compile(
                &[Term {
                    canonical: "bad\nname".into(),
                    priority: 0
                }],
                PersonalRules::default()
            ),
            Err(MatchError::InvalidName)
        ));
        assert!(matches!(
            Vocabulary::compile(
                &vec![
                    Term {
                        canonical: "name".into(),
                        priority: 0
                    };
                    MAX_TERMS + 1
                ],
                PersonalRules::default()
            ),
            Err(MatchError::TooLarge)
        ));
    }
}
