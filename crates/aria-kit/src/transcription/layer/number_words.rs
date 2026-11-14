//! English number words.

pub(super) const DIGIT_WORDS: [&str; 10] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
];
pub(super) const TEEN_WORDS: [&str; 10] = [
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
pub(super) const TENS_WORDS: [&str; 8] = [
    "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety",
];
/// Each a thousand times the one before.
const SCALES: [&str; 7] = [
    "thousand",
    "million",
    "billion",
    "trillion",
    "quadrillion",
    "quintillion",
    "sextillion",
];
const ORDINALS: [&str; 32] = [
    "first",
    "second",
    "third",
    "fourth",
    "fifth",
    "sixth",
    "seventh",
    "eighth",
    "ninth",
    "tenth",
    "eleventh",
    "twelfth",
    "thirteenth",
    "fourteenth",
    "fifteenth",
    "sixteenth",
    "seventeenth",
    "eighteenth",
    "nineteenth",
    "twentieth",
    "thirtieth",
    "fortieth",
    "fiftieth",
    "sixtieth",
    "seventieth",
    "eightieth",
    "ninetieth",
    "hundredth",
    "thousandth",
    "millionth",
    "billionth",
    "trillionth",
];

/// One word of a spoken number, with its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NumberWord {
    /// Zero through nine.
    Digit(u128),
    /// Ten through nineteen.
    Teen(u128),
    /// Twenty through ninety.
    Tens(u128),
    Hundred,
    Scale(Scale),
}

impl NumberWord {
    /// `word` in lowercase.
    pub(super) fn parse(word: &str) -> Option<Self> {
        if word == "hundred" {
            return Some(Self::Hundred);
        }
        place(&DIGIT_WORDS, word)
            .map(Self::Digit)
            .or_else(|| place(&TEEN_WORDS, word).map(|place| Self::Teen(10 + place)))
            .or_else(|| place(&TENS_WORDS, word).map(|place| Self::Tens(20 + 10 * place)))
            .or_else(|| Scale::parse(word).map(Self::Scale))
    }
}

/// A power of a thousand: "thousand", "million", and up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Scale {
    pub(super) name: &'static str,
    pub(super) value: u128,
}

impl Scale {
    fn parse(word: &str) -> Option<Self> {
        let values = std::iter::successors(Some(1000_u128), |value| value.checked_mul(1000));
        SCALES
            .into_iter()
            .zip(values)
            .find_map(|(name, value)| (name == word).then_some(Self { name, value }))
    }
}

/// Where `word` sits in `words`.
fn place(words: &[&str], word: &str) -> Option<u128> {
    (0..)
        .zip(words)
        .find_map(|(place, known)| (*known == word).then_some(place))
}

/// Whether `word` is a number word, as in "seven", "hundred", or
/// "twenty-one".
pub(super) fn is_number_word(word: &str) -> bool {
    word.to_lowercase()
        .split('-')
        .all(|part| NumberWord::parse(part).is_some())
}

/// Whether `word` is an ordinal, as in "second" or "twentieth".
pub(super) fn is_ordinal(word: &str) -> bool {
    ORDINALS.contains(&word.to_lowercase().as_str())
}

/// Whether `word` carries on a number that `previous` belongs to: a number
/// word, "point" after one, "and" after "hundred" or a scale, or "o" for a
/// zero after the point.
pub(super) fn continues(word: &str, previous: &str) -> bool {
    let previous = previous.to_lowercase();
    match word.to_lowercase().as_str() {
        "point" => is_number_word(&previous),
        "and" => ends_on_hundred_or_scale(&previous),
        "o" | "oh" => {
            matches!(previous.as_str(), "point" | "o" | "oh") || is_number_word(&previous)
        }
        word => is_number_word(word),
    }
}

fn ends_on_hundred_or_scale(word: &str) -> bool {
    matches!(
        word.rsplit('-').next().and_then(NumberWord::parse),
        Some(NumberWord::Hundred | NumberWord::Scale(_))
    )
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::number_after_number("twenty", "one", true)]
    #[case::capitalized("Twenty", "One", true)]
    #[case::hyphenated("twenty-one", "hundred", true)]
    #[case::point_after_number("two", "point", true)]
    #[case::number_after_point("point", "five", true)]
    #[case::and_after_hundred("hundred", "and", true)]
    #[case::and_after_scale("thousand", "and", true)]
    #[case::o_after_point("point", "o", true)]
    #[case::point_after_a_word("the", "point", false)]
    #[case::and_after_tens("twenty", "and", false)]
    #[case::o_after_a_word("the", "o", false)]
    #[case::other_word("twenty", "apples", false)]
    #[case::ordinal("twenty", "second", false)]
    fn continues_a_number(#[case] previous: &str, #[case] word: &str, #[case] expected: bool) {
        assert_eq!(continues(word, previous), expected);
    }
}
