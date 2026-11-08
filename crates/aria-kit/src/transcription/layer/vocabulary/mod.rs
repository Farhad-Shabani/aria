use std::collections::HashMap;

use crate::cancel::CancelToken;

mod personal;
pub use personal::PersonalRules;

const MAX_TERMS: usize = 100_000;
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 256;
const MAX_ALIASES: usize = 8;

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
    canonical: Option<String>,
    priority: u32,
    protected: bool,
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
                    Some(existing) if existing.protected => {}
                    Some(existing) if protected || term.priority > existing.priority => {
                        bytes += term.canonical.len();
                        *existing = Candidate {
                            canonical: Some(term.canonical.clone()),
                            priority: term.priority,
                            protected,
                        };
                    }
                    Some(existing)
                        if term.priority == existing.priority
                            && existing.canonical.as_deref() != Some(&term.canonical) =>
                    {
                        existing.canonical = None;
                    }
                    Some(_) => {}
                    None => {
                        bytes += alias.capacity() + term.canonical.len();
                        let capacity = bucket.aliases.capacity();
                        bucket.aliases.insert(
                            alias,
                            Candidate {
                                canonical: Some(term.canonical.clone()),
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
                && candidate.canonical.as_deref() != Some(canonical)
            {
                candidate.canonical = None;
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
        check(text.len())?;
        if self.index.is_empty() {
            let (corrected, _) = self.personal.apply_checked(text, check)?;
            return Ok((corrected != text).then_some(corrected));
        }
        let mut output = String::with_capacity(text.len());
        let mut position = 0;
        let mut unmatched = 0;
        while position < text.len() {
            check(output.len())?;
            let rest = &text[position..];
            let matched = if !text[..position].chars().next_back().is_some_and(is_word) {
                match first_word(rest).and_then(|first| self.index.get(&first)) {
                    Some(bucket) => self.find(text, position, bucket, check)?,
                    None => None,
                }
            } else {
                None
            };
            if let Some((end, candidate)) = matched {
                let (personal, _) = self
                    .personal
                    .apply_checked(&text[unmatched..position], &|bytes| {
                        check(output.len().saturating_add(bytes))
                    })?;
                output.push_str(&personal);
                let replacement = if candidate.protected || candidate.canonical.is_none() {
                    &text[position..end]
                } else {
                    candidate.canonical.as_deref().unwrap()
                };
                check(output.len().saturating_add(replacement.len()))?;
                output.push_str(replacement);
                position = end;
                unmatched = end;
            } else {
                position += rest.chars().next().unwrap().len_utf8();
            }
        }
        let (personal, _) = self.personal.apply_checked(&text[unmatched..], &|bytes| {
            check(output.len().saturating_add(bytes))
        })?;
        output.push_str(&personal);
        Ok((output != text).then_some(output))
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
            if !text[end..].chars().next().is_some_and(is_word)
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

fn aliases(canonical: &str) -> Vec<String> {
    let mut aliases = Vec::new();
    for punctuation in [true, false] {
        for spell_letters in [false, true] {
            if !punctuation
                && (!spell_letters
                    || !canonical.split(['-', '_', '.']).any(|part| {
                        !part.is_empty()
                            && part.len() <= 3
                            && part.chars().all(|c| c.is_ascii_alphabetic())
                    }))
            {
                continue;
            }
            let mut parts = Vec::new();
            let mut word = String::new();
            let push_word = |word: &mut String, parts: &mut Vec<String>| {
                if word.is_empty() {
                    return;
                }
                if spell_letters && word.len() <= 3 && word.chars().all(|c| c.is_ascii_alphabetic())
                {
                    parts.extend(word.chars().map(|c| c.to_string()));
                } else {
                    parts.push(word.clone());
                }
                word.clear();
            };
            for character in canonical.chars() {
                if let Some(spoken) = match character {
                    '-' => Some("dash"),
                    '_' => Some("underscore"),
                    '.' => Some("dot"),
                    _ => None,
                } {
                    push_word(&mut word, &mut parts);
                    if punctuation {
                        parts.push(spoken.to_owned());
                    }
                } else {
                    word.extend(character.to_lowercase());
                }
            }
            push_word(&mut word, &mut parts);
            let alias = normalize(&parts.join(" "));
            if alias != canonical.to_lowercase() && !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
    }
    aliases.truncate(MAX_ALIASES);
    aliases
}

#[cfg(test)]
mod tests {
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

        let vocabulary = Vocabulary::new(["aria-stt".to_owned(), "aria_stt".to_owned()]).unwrap();
        assert_eq!(vocabulary.correct("aria s t t"), None);
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
        let matcher = compile(&["aria-stt", "aria_stt"], PersonalRules::default());
        assert_eq!(matcher.correct("aria s t t"), None);
        let matcher = Vocabulary::compile(
            &[
                Term {
                    canonical: "aria-stt".into(),
                    priority: 2,
                },
                Term {
                    canonical: "aria_stt".into(),
                    priority: 1,
                },
            ],
            PersonalRules::default(),
        )
        .unwrap();
        assert_eq!(matcher.correct("aria s t t"), Some("aria-stt".into()));
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
