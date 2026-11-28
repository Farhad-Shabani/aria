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
    value_up_to_twenty(word).is_some()
}

/// The value of `word`, in lowercase, when it is a number word up to twenty.
pub(super) fn value_up_to_twenty(word: &str) -> Option<u128> {
    match NumberWord::parse(word)? {
        NumberWord::Digit(value) | NumberWord::Teen(value) | NumberWord::Tens(value @ 20) => {
            Some(value)
        }
        _ => None,
    }
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

/// What comes after a word in a run of number words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next<'w> {
    /// A word that reads on from it, in lowercase.
    Word(&'w str),
    /// The bare end of the text, where more speech may go on.
    End,
    /// Punctuation, or a word an earlier step claimed.
    Stop,
}

/// Whether `word` carries on a number that `previous` belongs to, with
/// `next` after it: a number word, "point" after one, "and" after
/// "hundred" or a scale, or "o" for a zero. "o" carries on once the number
/// `reads_digits`, as it does past "point" or a spoken symbol, and before a
/// number word or the end: "twenty oh five", but "ten oh my" and "ten oh,
/// my". All in lowercase.
pub(super) fn continues(word: &str, previous: &str, next: Next<'_>, reads_digits: bool) -> bool {
    match word {
        "point" => is_number_word(previous),
        "and" => ends_on_hundred_or_scale(previous),
        word if is_oh(word) => {
            reads_digits
                || match next {
                    Next::Word(next) => is_number_word(next),
                    Next::End => true,
                    Next::Stop => false,
                }
        }
        word => is_number_word(word),
    }
}

/// Whether `word`, in lowercase, is "o" or "oh", said for a zero.
pub(super) fn is_oh(word: &str) -> bool {
    matches!(word, "o" | "oh")
}

/// Whether `word`, in lowercase, is "zero" through "nine".
pub(super) fn is_digit(word: &str) -> bool {
    matches!(NumberWord::parse(word), Some(NumberWord::Digit(_)))
}

/// The value of `word`, in lowercase, when it is an ordinal under a hundred:
/// "fifth" is 5, "twenty-first" is 21.
pub(super) fn ordinal_value(word: &str) -> Option<u128> {
    if let Some((tens, unit)) = word.rsplit_once('-') {
        return compound_ordinal(tens, unit);
    }
    // "first" through "twentieth", then "thirtieth" through "ninetieth".
    match place(&ORDINALS, word)? {
        place @ 0..20 => Some(place + 1),
        place @ 20..27 => Some((place - 17) * 10),
        _ => None,
    }
}

/// The value of the ordinal said as `tens` then `unit`, in lowercase:
/// "twenty" and "first" are 21.
pub(super) fn compound_ordinal(tens: &str, unit: &str) -> Option<u128> {
    let Some(NumberWord::Tens(tens)) = NumberWord::parse(tens) else {
        return None;
    };
    ordinal_value(unit)
        .filter(|unit| *unit < 10)
        .map(|unit| tens + unit)
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
    #[case::number_after_number("twenty", "one", Next::End, false, true)]
    #[case::hyphenated("twenty-one", "hundred", Next::End, false, true)]
    #[case::point_after_number("two", "point", Next::End, false, true)]
    #[case::number_after_point("point", "five", Next::End, true, true)]
    #[case::and_after_hundred("hundred", "and", Next::End, false, true)]
    #[case::and_after_scale("thousand", "and", Next::End, false, true)]
    #[case::and_after_a_hyphenated_scale("twenty-thousand", "and", Next::End, false, true)]
    #[case::o_after_point("point", "o", Next::Word("my"), true, true)]
    #[case::o_after_a_digit_past_the_point("five", "oh", Next::Word("my"), true, true)]
    #[case::o_before_a_word("ten", "oh", Next::Word("my"), false, false)]
    #[case::o_before_a_digit("twenty", "oh", Next::Word("five"), false, true)]
    #[case::o_before_a_teen("twenty", "oh", Next::Word("fifteen"), false, true)]
    #[case::o_before_nothing("twenty", "oh", Next::End, false, true)]
    #[case::o_before_punctuation("ten", "oh", Next::Stop, false, false)]
    #[case::point_after_a_word("the", "point", Next::End, false, false)]
    #[case::and_after_tens("twenty", "and", Next::End, false, false)]
    #[case::and_after_a_word_ending_like_a_scale("foo-hundred", "and", Next::End, false, false)]
    #[case::other_word("twenty", "apples", Next::End, false, false)]
    #[case::ordinal("twenty", "second", Next::End, false, false)]
    fn continues_a_number(
        #[case] previous: &str,
        #[case] word: &str,
        #[case] next: Next<'_>,
        #[case] reads_digits: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(continues(word, previous, next, reads_digits), expected);
    }

    #[rstest]
    #[case::plain("second", true)]
    #[case::hyphenated("twenty-second", true)]
    #[case::number_word("twenty", false)]
    #[case::word_before_the_hyphen("split-second", false)]
    fn knows_an_ordinal(#[case] word: &str, #[case] expected: bool) {
        assert_eq!(is_ordinal(word), expected);
    }

    #[rstest]
    #[case::first("first", Some(1))]
    #[case::twentieth("twentieth", Some(20))]
    #[case::thirtieth("thirtieth", Some(30))]
    #[case::hyphenated("twenty-first", Some(21))]
    #[case::hyphen_before_a_tens_ordinal("twenty-thirtieth", None)]
    #[case::number_word("twenty", None)]
    #[case::other_word("second-hand", None)]
    fn values_an_ordinal(#[case] word: &str, #[case] expected: Option<u128>) {
        assert_eq!(ordinal_value(word), expected);
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
