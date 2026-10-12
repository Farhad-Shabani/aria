use std::collections::{BTreeSet, HashMap, HashSet};

use crate::cancel::CancelToken;

use super::{MatchError, is_word};

/// What stored rules may cost in memory.
const MAX_PERSONAL_BYTES: usize = 8 * 1024 * 1024;

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
        pairs.extend(learned);
        Self::from_pairs(terms, pairs)
    }

    pub fn from_stored(
        terms: Vec<String>,
        learned: Vec<(String, String)>,
        cancel: &CancelToken,
    ) -> Result<Self, MatchError> {
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
        while position < chars.len() {
            check(output.len())?;
            let first = chars[position]
                .to_lowercase()
                .next()
                .unwrap_or(chars[position]);
            let mut matched = None;
            if let Some(forms) = self.forms.get(&first) {
                for (form, correct) in forms {
                    check(output.len())?;
                    let end = position + form.len();
                    if boundary(&chars, position)
                        && end <= chars.len()
                        && boundary(&chars, end)
                        && chars[position..end]
                            .iter()
                            .zip(form)
                            .all(|(a, b)| a.to_lowercase().eq(b.to_lowercase()))
                    {
                        matched = Some((form, correct));
                        break;
                    }
                }
            }
            if let Some((form, correct)) = matched {
                check(output.len().saturating_add(correct.len()))?;
                let original: String = chars[position..position + form.len()].iter().collect();
                if original != *correct {
                    substitutions.push((original, correct.clone()));
                }
                output.push_str(correct);
                position += form.len();
            } else {
                check(output.len().saturating_add(chars[position].len_utf8()))?;
                output.push(chars[position]);
                position += 1;
            }
        }
        let (output, fuzzy) = apply_fuzzy(&output, &self.terms, &self.known, check)?;
        substitutions.extend(fuzzy);
        Ok((output, substitutions))
    }
}

/// A false substitution reads worse than a missed one.
const FUZZY_RATIO: f64 = 0.85;
const MIN_TOKEN_LEN: usize = 3;
const MAX_VARIANTS: usize = 8;
/// A term with more parts than this is a serial number, not a word, and
/// its spoken forms would number in the millions.
const MAX_SEGMENTS: usize = 8;

const DIGIT_WORDS: [&str; 10] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
];
const NUMBER_WORDS: [(&str, &str); 11] = [
    ("10", "ten"),
    ("11", "eleven"),
    ("12", "twelve"),
    ("13", "thirteen"),
    ("14", "fourteen"),
    ("15", "fifteen"),
    ("16", "sixteen"),
    ("17", "seventeen"),
    ("18", "eighteen"),
    ("19", "nineteen"),
    ("20", "twenty"),
];

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

/// How a transcriber renders a compact term: digits as words, a hyphen as
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
                if let Some((_, word)) = NUMBER_WORDS.iter().find(|(digits, _)| digits == segment) {
                    options.push((*word).to_owned());
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
                    || DIGIT_WORDS.contains(&token.as_str())
                    || NUMBER_WORDS.iter().any(|(_, word)| *word == token)
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

fn spelled_digits(segment: &str) -> String {
    segment
        .chars()
        .filter_map(|c| c.to_digit(10))
        .map(|digit| DIGIT_WORDS[digit as usize])
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

/// A word the user does not have in their list becomes the listed word it
/// nearly is. Tokens are ASCII words; a match must share its first letter
/// and about its length before the ratio is consulted.
fn apply_fuzzy<E>(
    text: &str,
    terms: &[String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<(String, Vec<(String, String)>), E> {
    check(text.len())?;
    if terms.is_empty() {
        return Ok((text.to_owned(), Vec::new()));
    }
    let mut output = String::with_capacity(text.len());
    let mut substitutions = Vec::new();
    let mut rest = text;
    while let Some((start, length)) = next_token(rest) {
        check(output.len().saturating_add(start))?;
        output.push_str(&rest[..start]);
        let token = &rest[start..start + length];
        match best_term(token, terms, known, check)? {
            Some(term) => {
                check(output.len().saturating_add(term.len()))?;
                substitutions.push((token.to_owned(), term.to_owned()));
                output.push_str(term);
            }
            None => {
                check(output.len().saturating_add(token.len()))?;
                output.push_str(token);
            }
        }
        rest = &rest[start + length..];
    }
    check(output.len().saturating_add(rest.len()))?;
    output.push_str(rest);
    Ok((output, substitutions))
}

/// The next `[A-Za-z0-9][A-Za-z0-9'-]*` as byte offset and length.
fn next_token(text: &str) -> Option<(usize, usize)> {
    let start = text.find(|c: char| c.is_ascii_alphanumeric())?;
    let length = text[start..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '\'' || c == '-'))
        .unwrap_or(text.len() - start);
    Some((start, length))
}

fn best_term<'a, E>(
    token: &str,
    terms: &'a [String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<Option<&'a str>, E> {
    check(0)?;
    let folded = token.to_lowercase();
    if token.chars().count() < MIN_TOKEN_LEN {
        return Ok(None);
    }
    for word in known {
        check(0)?;
        if word.iter().copied().eq(folded.chars()) {
            return Ok(None);
        }
    }
    let folded: Vec<char> = folded.chars().collect();
    let mut best: Option<(&str, f64)> = None;
    for (term, term_folded) in terms.iter().zip(known) {
        check(0)?;
        // "speaker" against "wespeaker" scores 0.875: the first letter is
        // the guard that keeps it from matching.
        if term_folded.first() != folded.first() {
            continue;
        }
        let longest = folded.len().max(term_folded.len());
        let allowed = ((0.2 * longest as f64) as usize).max(1);
        if folded.len().abs_diff(term_folded.len()) > allowed {
            continue;
        }
        let ratio = ratio(&folded, term_folded, check)?;
        if ratio >= FUZZY_RATIO && best.is_none_or(|(_, best_ratio)| ratio > best_ratio) {
            best = Some((term, ratio));
        }
    }
    Ok(best.map(|(term, _)| term))
}

/// Python's `difflib.SequenceMatcher.ratio`: twice the characters in the
/// matching blocks over both lengths, the blocks found longest-first and
/// earliest-first, as difflib finds them. The thresholds were tuned
/// against that number, so it is that number.
fn ratio<E>(a: &[char], b: &[char], check: &impl Fn(usize) -> Result<(), E>) -> Result<f64, E> {
    if a.is_empty() && b.is_empty() {
        return Ok(1.0);
    }
    let mut matched = 0;
    let mut windows = vec![(0, a.len(), 0, b.len())];
    while let Some((a_low, a_high, b_low, b_high)) = windows.pop() {
        let (i, j, size) = longest_match(a, a_low, a_high, b, b_low, b_high, check)?;
        if size == 0 {
            continue;
        }
        matched += size;
        if a_low < i && b_low < j {
            windows.push((a_low, i, b_low, j));
        }
        if i + size < a_high && j + size < b_high {
            windows.push((i + size, a_high, j + size, b_high));
        }
    }
    Ok(2.0 * matched as f64 / (a.len() + b.len()) as f64)
}

/// The longest common run within the windows; ties go to the earliest
/// start in `a`, then in `b`.
fn longest_match<E>(
    a: &[char],
    a_low: usize,
    a_high: usize,
    b: &[char],
    b_low: usize,
    b_high: usize,
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<(usize, usize, usize), E> {
    check(0)?;
    let mut best = (a_low, b_low, 0);
    let mut lengths = vec![0; b_high - b_low + 1];
    for (i, a_char) in a.iter().enumerate().take(a_high).skip(a_low) {
        check(0)?;
        let mut next = vec![0; b_high - b_low + 1];
        for (j, b_char) in b.iter().enumerate().take(b_high).skip(b_low) {
            if a_char != b_char {
                continue;
            }
            let size = lengths[j - b_low] + 1;
            next[j - b_low + 1] = size;
            if size > best.2 {
                best = (i + 1 - size, j + 1 - size, size);
            }
        }
        lengths = next;
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_rules_preserve_precedence_and_bound_preparation() {
        let terms = vec!["db2".to_owned(), "vex16".to_owned()];
        let learned = vec![("db two".to_owned(), "db9".to_owned())];
        let stored =
            PersonalRules::from_stored(terms.clone(), learned.clone(), &CancelToken::never())
                .unwrap();
        let previous = PersonalRules::new(terms.clone(), learned.clone());
        assert_eq!(
            stored.apply("db two and Vexe16"),
            previous.apply("db two and Vexe16")
        );
        let cancelled = CancelToken::new();
        cancelled.cancel();
        assert!(matches!(
            PersonalRules::from_stored(terms, learned, &cancelled),
            Err(MatchError::Cancelled)
        ));
        let oversized = (0..40_000)
            .map(|index| (format!("name {index}"), "target".repeat(20)))
            .collect();
        assert!(matches!(
            PersonalRules::from_stored(Vec::new(), oversized, &CancelToken::never()),
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
    fn ratio_matches_difflib() {
        let cases = [
            ("vexe16", "vex16", 0.9090909090909091),
            ("speaker", "wespeaker", 0.875),
            ("zxx9", "zx9", 0.8571428571428571),
            ("kubernets", "kubernetes", 0.9473684210526315),
            ("abcdef", "abdcef", 0.8333333333333334),
            ("abc", "abc", 1.0),
            ("abc", "xyz", 0.0),
            ("", "", 1.0),
        ];
        for (a, b, expected) in cases {
            let a: Vec<char> = a.chars().collect();
            let b: Vec<char> = b.chars().collect();
            let actual = ratio(&a, &b, &|_| Ok::<_, std::convert::Infallible>(())).unwrap();
            assert!(
                (actual - expected).abs() < 1e-12,
                "{a:?} vs {b:?}: {actual} != {expected}"
            );
        }
    }

    #[test]
    fn fuzzy_comparison_propagates_cancellation_during_matching() {
        let checks = std::cell::Cell::new(0);
        let word: Vec<char> = "kubernetes".chars().collect();
        let result = ratio(&word, &word, &|_| {
            checks.set(checks.get() + 1);
            if checks.get() == 3 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err("cancelled"));
    }

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
        let terms = strings(&["kubernetes", "wespeaker", "vex16"]);
        let cases = [
            ("kubernets rocks", "kubernetes rocks"),
            ("the speaker is loud", "the speaker is loud"),
            ("kubernetes is fine", "kubernetes is fine"),
            ("ok", "ok"),
            ("vexe16 boots", "vex16 boots"),
        ];
        for (input, expected) in cases {
            assert_eq!(apply_corrections(input, &[], &terms).0, expected, "{input}");
        }
        assert_eq!(apply_corrections("kubernets", &[], &[]).0, "kubernets");
    }

    #[test]
    fn a_serial_number_gets_no_spoken_forms() {
        assert!(expand_term_variants("a1-b2-c3-d4-e5-f6-g7-h8-i9-j0").is_empty());
        assert_eq!(expand_term_variants("k8s").len(), 2);
    }
}
