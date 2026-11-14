//! English number words.

const DIGIT_WORDS: [&str; 10] = [
    "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
];
const TEEN_WORDS: [&str; 10] = [
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
const TENS_WORDS: [&str; 8] = [
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
    /// Reads `word`, which must be in lowercase.
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

fn place(words: &[&str], word: &str) -> Option<u128> {
    (0..)
        .zip(words)
        .find_map(|(place, known)| (*known == word).then_some(place))
}

/// The word for `value`, zero through twenty.
pub(super) fn word_for(value: usize) -> Option<&'static str> {
    DIGIT_WORDS
        .iter()
        .chain(&TEEN_WORDS)
        .chain(TENS_WORDS.first())
        .nth(value)
        .copied()
}

/// Whether `word`, in lowercase, is one [`word_for`] gives.
pub(super) fn is_up_to_twenty(word: &str) -> bool {
    matches!(
        NumberWord::parse(word),
        Some(NumberWord::Digit(_) | NumberWord::Teen(_) | NumberWord::Tens(20))
    )
}

/// The number words `word` holds, in lowercase: "twenty-one" holds two.
/// `None` stands for a part that isn't one.
pub(super) fn number_words_in(word: &str) -> impl Iterator<Item = Option<NumberWord>> + '_ {
    word.split('-').map(NumberWord::parse)
}

/// Whether `word`, in lowercase, is a number word, as in "seven", "hundred",
/// or "twenty-one".
pub(super) fn is_number_word(word: &str) -> bool {
    number_words_in(word).all(|word| word.is_some())
}

/// Whether `word`, in lowercase, is an ordinal, as in "second" or
/// "twenty-second".
pub(super) fn is_ordinal(word: &str) -> bool {
    match word.rsplit_once('-') {
        Some((number, ordinal)) => is_number_word(number) && ORDINALS.contains(&ordinal),
        None => ORDINALS.contains(&word),
    }
}

/// Whether `word` carries on a number that `previous` belongs to: a number
/// word, "point" after one, "and" after "hundred" or a scale, or "o" for a
/// zero once the number is `after_point`. All in lowercase.
pub(super) fn continues(word: &str, previous: &str, after_point: bool) -> bool {
    match word {
        "point" => is_number_word(previous),
        "and" => ends_on_hundred_or_scale(previous),
        // "ten oh my" ends at "ten".
        "o" | "oh" => after_point,
        word => is_number_word(word),
    }
}

fn ends_on_hundred_or_scale(word: &str) -> bool {
    let last = number_words_in(word)
        .collect::<Option<Vec<_>>>()
        .and_then(|words| words.last().copied());
    matches!(last, Some(NumberWord::Hundred | NumberWord::Scale(_)))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::number_after_number("twenty", "one", false, true)]
    #[case::hyphenated("twenty-one", "hundred", false, true)]
    #[case::point_after_number("two", "point", false, true)]
    #[case::number_after_point("point", "five", true, true)]
    #[case::and_after_hundred("hundred", "and", false, true)]
    #[case::and_after_scale("thousand", "and", false, true)]
    #[case::and_after_a_hyphenated_scale("twenty-thousand", "and", false, true)]
    #[case::o_after_point("point", "o", true, true)]
    #[case::o_after_a_digit_past_the_point("five", "oh", true, true)]
    #[case::o_before_the_point("ten", "oh", false, false)]
    #[case::point_after_a_word("the", "point", false, false)]
    #[case::and_after_tens("twenty", "and", false, false)]
    #[case::and_after_a_word_ending_like_a_scale("foo-hundred", "and", false, false)]
    #[case::other_word("twenty", "apples", false, false)]
    #[case::ordinal("twenty", "second", false, false)]
    fn continues_a_number(
        #[case] previous: &str,
        #[case] word: &str,
        #[case] after_point: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(continues(word, previous, after_point), expected);
    }

    #[rstest]
    #[case::plain("second", true)]
    #[case::hyphenated("twenty-second", true)]
    #[case::number_word("twenty", false)]
    #[case::word_before_the_hyphen("split-second", false)]
    fn knows_an_ordinal(#[case] word: &str, #[case] expected: bool) {
        assert_eq!(is_ordinal(word), expected);
    }

    #[test]
    fn words_run_from_zero_through_twenty() {
        assert_eq!(word_for(0), Some("zero"));
        assert_eq!(word_for(13), Some("thirteen"));
        assert_eq!(word_for(20), Some("twenty"));
        assert_eq!(word_for(21), None);
        assert!(is_up_to_twenty("twenty"));
        assert!(!is_up_to_twenty("thirty"));
    }
}
