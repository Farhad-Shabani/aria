//! The vocabulary of spoken numbers, and what each word means.
//!
//! Every table mirrors `EnglishNumberNormalizer` in Whisper's normalizer,
//! deriving the same shapes instead of listing them. That carries over its
//! quirks: `nineth` is spelled that way upstream, so it is spelled that way
//! here.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

/// Cardinals one through nineteen, in value order.
const ONES: &[&str] = &[
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
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
];

const TENS: &[(&str, i128)] = &[
    ("twenty", 20),
    ("thirty", 30),
    ("forty", 40),
    ("fifty", 50),
    ("sixty", 60),
    ("seventy", 70),
    ("eighty", 80),
    ("ninety", 90),
];

const MULTIPLIERS: &[(&str, i128)] = &[
    ("hundred", 100),
    ("thousand", 1_000),
    ("million", 1_000_000),
    ("billion", 1_000_000_000),
    ("trillion", 1_000_000_000_000),
    ("quadrillion", 1_000_000_000_000_000),
    ("quintillion", 1_000_000_000_000_000_000),
    ("sextillion", 1_000_000_000_000_000_000_000),
    ("septillion", 1_000_000_000_000_000_000_000_000),
    ("octillion", 1_000_000_000_000_000_000_000_000_000),
    ("nonillion", 1_000_000_000_000_000_000_000_000_000_000),
    ("decillion", 1_000_000_000_000_000_000_000_000_000_000_000),
];

const ZEROS: &[&str] = &["o", "oh", "zero"];

/// Ordinals that do not follow from their cardinal.
const IRREGULAR_ORDINALS: &[(&str, i128)] = &[
    ("zeroth", 0),
    ("first", 1),
    ("second", 2),
    ("third", 3),
    ("fifth", 5),
    ("twelfth", 12),
];

/// Values whose ordinal is irregular, so the regular rule must skip them.
const IRREGULAR_ORDINAL_VALUES: &[i128] = &[1, 2, 3, 5, 12];

/// What a word contributes to the number being read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Token {
    /// Digits already, optionally signed or carrying a currency symbol.
    Digits {
        prefix: Option<char>,
        body: String,
    },
    Zero,
    One(i128),
    Ten(i128),
    Multiplier(i128),
    /// A plural or ordinal that ends the number it belongs to, such as
    /// `sixes` or `fourth`.
    OneSuffixed(i128, &'static str),
    TenSuffixed(i128, &'static str),
    MultiplierSuffixed(i128, &'static str),
    /// A sign that binds to the number after it.
    PrecedingPrefix(char),
    /// A currency name that binds to the number before it.
    FollowingPrefix(char),
    /// `percent`, which closes the number it follows.
    Percent,
    /// `per`, which closes the number only when `cent` follows it.
    Per,
    And,
    Double,
    Triple,
    Point,
    /// Anything else, passed through untouched.
    Other,
}

pub(super) struct Lexicon {
    zeros: HashSet<&'static str>,
    ones: HashMap<&'static str, i128>,
    ones_suffixed: HashMap<String, (i128, &'static str)>,
    tens: HashMap<&'static str, i128>,
    tens_suffixed: HashMap<String, (i128, &'static str)>,
    multipliers: HashMap<&'static str, i128>,
    multipliers_suffixed: HashMap<String, (i128, &'static str)>,
    preceding_prefixers: HashMap<&'static str, char>,
    following_prefixers: HashMap<&'static str, char>,
    /// Every word above. A word outside it is not part of any number.
    known: HashSet<String>,
    /// Words that may follow `point`.
    decimals: HashSet<String>,
}

pub(super) static LEXICON: LazyLock<Lexicon> = LazyLock::new(Lexicon::build);

impl Lexicon {
    /// Classifies one word. The order matches the reference implementation:
    /// digits first, then each table in turn.
    pub(super) fn classify(&self, word: &str) -> Token {
        let (prefix, body) = split_prefix(word);
        if is_digits(body) {
            return Token::Digits {
                prefix,
                body: body.to_owned(),
            };
        }
        if !self.known.contains(word) {
            return Token::Other;
        }
        if self.zeros.contains(word) {
            return Token::Zero;
        }
        if let Some(&value) = self.ones.get(word) {
            return Token::One(value);
        }
        if let Some(&(value, suffix)) = self.ones_suffixed.get(word) {
            return Token::OneSuffixed(value, suffix);
        }
        if let Some(&value) = self.tens.get(word) {
            return Token::Ten(value);
        }
        if let Some(&(value, suffix)) = self.tens_suffixed.get(word) {
            return Token::TenSuffixed(value, suffix);
        }
        if let Some(&value) = self.multipliers.get(word) {
            return Token::Multiplier(value);
        }
        if let Some(&(value, suffix)) = self.multipliers_suffixed.get(word) {
            return Token::MultiplierSuffixed(value, suffix);
        }
        if let Some(&symbol) = self.preceding_prefixers.get(word) {
            return Token::PrecedingPrefix(symbol);
        }
        if let Some(&symbol) = self.following_prefixers.get(word) {
            return Token::FollowingPrefix(symbol);
        }
        match word {
            "percent" => Token::Percent,
            "per" => Token::Per,
            "and" => Token::And,
            "double" => Token::Double,
            "triple" => Token::Triple,
            "point" => Token::Point,
            _ => Token::Other,
        }
    }

    pub(super) fn is_known(&self, word: &str) -> bool {
        self.known.contains(word)
    }

    pub(super) fn is_zero(&self, word: &str) -> bool {
        self.zeros.contains(word)
    }

    pub(super) fn is_one(&self, word: &str) -> bool {
        self.ones.contains_key(word)
    }

    pub(super) fn is_ten(&self, word: &str) -> bool {
        self.tens.contains_key(word)
    }

    pub(super) fn is_multiplier(&self, word: &str) -> bool {
        self.multipliers.contains_key(word)
    }

    pub(super) fn is_decimal(&self, word: &str) -> bool {
        self.decimals.contains(word)
    }

    pub(super) fn one_value(&self, word: &str) -> Option<i128> {
        self.ones.get(word).copied()
    }

    fn build() -> Self {
        let zeros: HashSet<&'static str> = ZEROS.iter().copied().collect();
        let ones: HashMap<&'static str, i128> = ONES
            .iter()
            .enumerate()
            .map(|(index, &name)| (name, index as i128 + 1))
            .collect();
        let tens: HashMap<&'static str, i128> = TENS.iter().copied().collect();
        let multipliers: HashMap<&'static str, i128> = MULTIPLIERS.iter().copied().collect();

        let ones_suffixed = one_suffixes(&ones);
        let tens_suffixed = ten_suffixes(&tens);
        let multipliers_suffixed = multiplier_suffixes(&multipliers);
        let preceding_prefixers = HashMap::from([
            ("minus", '-'),
            ("negative", '-'),
            ("plus", '+'),
            ("positive", '+'),
        ]);
        let following_prefixers = HashMap::from([
            ("pound", '£'),
            ("pounds", '£'),
            ("euro", '€'),
            ("euros", '€'),
            ("dollar", '$'),
            ("dollars", '$'),
            ("cent", '¢'),
            ("cents", '¢'),
        ]);

        let decimals = ones
            .keys()
            .chain(tens.keys())
            .chain(zeros.iter())
            .map(|&word| word.to_owned())
            .collect();
        let known = ones
            .keys()
            .chain(tens.keys())
            .chain(multipliers.keys())
            .chain(zeros.iter())
            .chain(preceding_prefixers.keys())
            .chain(following_prefixers.keys())
            .map(|&word| word.to_owned())
            .chain(ones_suffixed.keys().cloned())
            .chain(tens_suffixed.keys().cloned())
            .chain(multipliers_suffixed.keys().cloned())
            .chain(["per", "percent", "and", "double", "triple", "point"].map(str::to_owned))
            .collect();

        Self {
            zeros,
            ones,
            ones_suffixed,
            tens,
            tens_suffixed,
            multipliers,
            multipliers_suffixed,
            preceding_prefixers,
            following_prefixers,
            known,
            decimals,
        }
    }
}

/// `six` pluralizes irregularly; every ordinal above three is regular except
/// the ones listed separately.
fn one_suffixes(ones: &HashMap<&'static str, i128>) -> HashMap<String, (i128, &'static str)> {
    let mut suffixed = HashMap::new();
    for (&name, &value) in ones {
        let plural = if name == "six" {
            "sixes".to_owned()
        } else {
            format!("{name}s")
        };
        suffixed.insert(plural, (value, "s"));

        if !IRREGULAR_ORDINAL_VALUES.contains(&value) {
            let ordinal = if name.ends_with('t') {
                format!("{name}h")
            } else {
                format!("{name}th")
            };
            suffixed.insert(ordinal, (value, "th"));
        }
    }
    for &(name, value) in IRREGULAR_ORDINALS {
        suffixed.insert(name.to_owned(), (value, ordinal_suffix(value)));
    }
    suffixed
}

fn ordinal_suffix(value: i128) -> &'static str {
    match value {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

fn ten_suffixes(tens: &HashMap<&'static str, i128>) -> HashMap<String, (i128, &'static str)> {
    tens.iter()
        .flat_map(|(&name, &value)| {
            [
                (name.replace('y', "ies"), (value, "s")),
                (name.replace('y', "ieth"), (value, "th")),
            ]
        })
        .collect()
}

fn multiplier_suffixes(
    multipliers: &HashMap<&'static str, i128>,
) -> HashMap<String, (i128, &'static str)> {
    multipliers
        .iter()
        .flat_map(|(&name, &value)| {
            [
                (format!("{name}s"), (value, "s")),
                (format!("{name}th"), (value, "th")),
            ]
        })
        .collect()
}

/// Splits a leading sign or currency symbol off a word.
fn split_prefix(word: &str) -> (Option<char>, &str) {
    const PREFIXES: &[char] = &['-', '+', '£', '€', '$', '¢'];
    match word.chars().next() {
        Some(first) if PREFIXES.contains(&first) => (Some(first), &word[first.len_utf8()..]),
        _ => (None, word),
    }
}

/// Whether a word is already written as digits, with at most one decimal part.
pub(super) fn is_digits(word: &str) -> bool {
    let (whole, fraction) = word.split_once('.').unwrap_or((word, "1"));
    !whole.is_empty()
        && !fraction.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardinals_ordinals_and_plurals_are_distinguished() {
        assert_eq!(LEXICON.classify("seven"), Token::One(7));
        assert_eq!(LEXICON.classify("seventh"), Token::OneSuffixed(7, "th"));
        assert_eq!(LEXICON.classify("sevens"), Token::OneSuffixed(7, "s"));
        assert_eq!(LEXICON.classify("sixes"), Token::OneSuffixed(6, "s"));
        assert_eq!(LEXICON.classify("twentieth"), Token::TenSuffixed(20, "th"));
        assert_eq!(LEXICON.classify("thirties"), Token::TenSuffixed(30, "s"));
    }

    #[test]
    fn irregular_ordinals_keep_their_own_suffix() {
        assert_eq!(LEXICON.classify("first"), Token::OneSuffixed(1, "st"));
        assert_eq!(LEXICON.classify("second"), Token::OneSuffixed(2, "nd"));
        assert_eq!(LEXICON.classify("third"), Token::OneSuffixed(3, "rd"));
        assert_eq!(LEXICON.classify("twelfth"), Token::OneSuffixed(12, "th"));
    }

    #[test]
    fn a_signed_or_priced_number_keeps_its_symbol() {
        assert_eq!(
            LEXICON.classify("-3.5"),
            Token::Digits {
                prefix: Some('-'),
                body: "3.5".to_owned(),
            }
        );
        assert_eq!(
            LEXICON.classify("20"),
            Token::Digits {
                prefix: None,
                body: "20".to_owned(),
            }
        );
    }

    #[test]
    fn an_ordinary_word_is_passed_through() {
        assert_eq!(LEXICON.classify("socket"), Token::Other);
    }

    #[test]
    fn digits_need_a_whole_and_a_fractional_part() {
        assert!(is_digits("20"));
        assert!(is_digits("3.5"));
        assert!(!is_digits("20."));
        assert!(!is_digits(".5"));
        assert!(!is_digits("3.5.1"));
        assert!(!is_digits("twenty"));
    }
}
