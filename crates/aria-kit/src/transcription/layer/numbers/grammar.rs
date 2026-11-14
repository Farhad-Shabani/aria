//! The numbers English number words spell.
//!
//! ```text
//! number   := whole ["point" fraction [scale]]
//! whole    := "zero" | group (scale [["and"] group])*  scales descending
//! group    := under100 | under100 "hundred" [["and"] under100]
//! under100 := digit | teen | tens [digit]               digit is 1 to 9
//! fraction := ("zero" | "o" | under100)+                each as its digits
//! ```
//!
//! A scale counts fewer than a thousand, and so does the group after the
//! last scale. No multiple of ten comes before "hundred".

use std::fmt;

use super::super::number_words::{NumberWord, Scale, number_words_in};

const MILLION: u128 = 1_000_000;

/// A spoken number. Its `Display` is its one written form: `21`, `2.5`,
/// `2 million`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Number {
    whole: u128,
    /// The digits said after "point".
    fraction: Option<String>,
    /// A scale said last and kept as a word, as in `2.5 million`.
    scale: Option<Scale>,
}

impl Number {
    /// Reads `words`, in lowercase, as one number. Every word must belong to it: number
    /// words, "point", "and", and "o" for a zero after the point. Anything
    /// English doesn't say as one number is `None`, as "one two three" is.
    pub(super) fn parse(words: &[&str]) -> Option<Self> {
        let tokens = Token::parse_all(words)?;
        let mut parser = Parser {
            tokens: &tokens,
            at: 0,
        };
        let number = parser.number()?;
        (parser.at == tokens.len()).then_some(number)
    }

    /// The value, when digits alone write it.
    pub(super) fn integer(&self) -> Option<u128> {
        (self.fraction.is_none() && self.scale.is_none()).then_some(self.whole)
    }

    /// How many digits follow the point, when the number ends on them.
    pub(super) fn decimal_places(&self) -> Option<usize> {
        match (&self.fraction, self.scale) {
            (Some(fraction), None) => Some(fraction.len()),
            _ => None,
        }
    }
}

impl fmt::Display for Number {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.whole)?;
        if let Some(fraction) = &self.fraction {
            write!(formatter, ".{fraction}")?;
        }
        if let Some(scale) = self.scale {
            write!(formatter, " {}", scale.name)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Word(NumberWord),
    Point,
    And,
    /// "o" or "oh", a zero after the point.
    Oh,
}

/// Reads `words`, in lowercase, the way the words after "point" are: "one
/// zero zero" is `100`, "fifty fifty" is `5050`.
pub(super) fn digits(words: &[&str]) -> Option<String> {
    let tokens = Token::parse_all(words)?;
    let mut parser = Parser {
        tokens: &tokens,
        at: 0,
    };
    let digits = parser.fraction()?;
    (parser.at == tokens.len()).then_some(digits)
}

impl Token {
    fn parse_all(words: &[&str]) -> Option<Vec<Self>> {
        Some(
            words
                .iter()
                .map(|word| Self::parse(word))
                .collect::<Option<Vec<_>>>()?
                .concat(),
        )
    }

    /// The tokens one word in lowercase holds: "twenty-one" holds two.
    fn parse(word: &str) -> Option<Vec<Self>> {
        match word {
            "point" => Some(vec![Self::Point]),
            "and" => Some(vec![Self::And]),
            "o" | "oh" => Some(vec![Self::Oh]),
            word => number_words_in(word)
                .map(|word| word.map(Self::Word))
                .collect(),
        }
    }

    fn opens_under_100(self) -> bool {
        matches!(
            self,
            Self::Word(NumberWord::Digit(1..) | NumberWord::Teen(_) | NumberWord::Tens(_))
        )
    }
}

/// The whole part of a number, built a group at a time.
#[derive(Default)]
struct Whole {
    value: u128,
    /// The last scale said, and the count before it.
    scaled: Option<(u128, Scale)>,
}

impl Whole {
    /// Adds `count` of `scale`, which must be under the scale before it.
    fn add_scaled(&mut self, count: u128, scale: Scale) -> Option<()> {
        let descends = self.scaled.is_none_or(|(_, last)| scale.value < last.value);
        if count >= 1000 || !descends {
            return None;
        }
        self.value = self.value.checked_add(count.checked_mul(scale.value)?)?;
        self.scaled = Some((count, scale));
        Some(())
    }

    /// Adds the group no scale follows.
    fn add_last(&mut self, group: u128) -> Option<()> {
        // "two thousand fifteen hundred" is not a number.
        if self.scaled.is_some() && group >= 1000 {
            return None;
        }
        self.value = self.value.checked_add(group)?;
        Some(())
    }

    fn into_number(self) -> Number {
        match self.scaled {
            // "two million" reads better than 2000000.
            Some((count, scale)) if scale.value >= MILLION && self.value == count * scale.value => {
                Number {
                    whole: count,
                    fraction: None,
                    scale: Some(scale),
                }
            }
            _ => Number {
                whole: self.value,
                fraction: None,
                scale: None,
            },
        }
    }
}

struct Parser<'t> {
    tokens: &'t [Token],
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<Token> {
        self.tokens.get(self.at).copied()
    }

    fn take(&mut self, token: Token) -> bool {
        let taken = self.peek() == Some(token);
        self.at += usize::from(taken);
        taken
    }

    fn number(&mut self) -> Option<Number> {
        let whole = self.whole()?;
        if !self.take(Token::Point) {
            return Some(whole.into_number());
        }
        Some(Number {
            whole: whole.value,
            fraction: Some(self.fraction()?),
            scale: self.scale(),
        })
    }

    fn whole(&mut self) -> Option<Whole> {
        let mut whole = Whole::default();
        if self.take(Token::Word(NumberWord::Digit(0))) {
            return Some(whole);
        }
        loop {
            let group = self.group()?;
            let Some(scale) = self.scale() else {
                whole.add_last(group)?;
                return Some(whole);
            };
            whole.add_scaled(group, scale)?;
            self.skip_and();
            if !self.peek().is_some_and(Token::opens_under_100) {
                return Some(whole);
            }
        }
    }

    fn group(&mut self) -> Option<u128> {
        let count = self.under_100()?;
        if !self.take(Token::Word(NumberWord::Hundred)) {
            return Some(count);
        }
        // "fifteen hundred" is said, "twenty hundred" is not.
        if count >= 10 && count % 10 == 0 {
            return None;
        }
        self.skip_and();
        Some(count * 100 + self.under_100().unwrap_or(0))
    }

    /// Takes nothing when no number under a hundred is next.
    fn under_100(&mut self) -> Option<u128> {
        let Token::Word(word) = self.peek()? else {
            return None;
        };
        let value = match word {
            NumberWord::Digit(value @ 1..) | NumberWord::Teen(value) | NumberWord::Tens(value) => {
                value
            }
            _ => return None,
        };
        self.at += 1;
        let digit_after_tens = match (word, self.peek()) {
            (NumberWord::Tens(_), Some(Token::Word(NumberWord::Digit(digit @ 1..)))) => digit,
            _ => return Some(value),
        };
        self.at += 1;
        Some(value + digit_after_tens)
    }

    /// Passes an "and" that a number under a hundred follows.
    fn skip_and(&mut self) {
        let after = self.tokens.get(self.at + 1).copied();
        if self.peek() == Some(Token::And) && after.is_some_and(Token::opens_under_100) {
            self.at += 1;
        }
    }

    fn scale(&mut self) -> Option<Scale> {
        let Some(Token::Word(NumberWord::Scale(scale))) = self.peek() else {
            return None;
        };
        self.at += 1;
        Some(scale)
    }

    fn fraction(&mut self) -> Option<String> {
        let mut digits = String::new();
        while let Some(next) = self.fraction_digits() {
            digits.push_str(&next);
        }
        (!digits.is_empty()).then_some(digits)
    }

    /// The digits the next word or two say after the point: "o" is 0,
    /// "seventeen" is 17.
    fn fraction_digits(&mut self) -> Option<String> {
        if self.take(Token::Oh) || self.take(Token::Word(NumberWord::Digit(0))) {
            return Some("0".to_owned());
        }
        self.under_100().map(|value| value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn written(said: &str) -> Option<String> {
        let words: Vec<&str> = said.split(' ').collect();
        Number::parse(&words).map(|number| number.to_string())
    }

    #[rstest]
    #[case::digit("seven", "7")]
    #[case::zero("zero", "0")]
    #[case::teen("seventeen", "17")]
    #[case::tens("seventy", "70")]
    #[case::tens_and_digit("twenty one", "21")]
    #[case::hyphenated("twenty-one", "21")]
    #[case::hundreds("three hundred", "300")]
    #[case::hundreds_and_tens("one hundred five", "105")]
    #[case::hundreds_with_and("six hundred and thirty one", "631")]
    #[case::teen_hundreds("fourteen hundred", "1400")]
    #[case::compound_hundreds("thirty two hundred and twelve", "3212")]
    #[case::thousand("twelve thousand", "12000")]
    #[case::thousand_and_digit("three thousand two", "3002")]
    #[case::thousand_with_and("one thousand and five", "1005")]
    #[case::year("two thousand twenty six", "2026")]
    #[case::thousands_and_hundreds(
        "two hundred sixteen thousand seven hundred eighty three",
        "216783"
    )]
    #[case::skipped_scale("five million four", "5000004")]
    #[case::every_scale(
        "four billion two hundred six million thirty one thousand five hundred eighteen",
        "4206031518"
    )]
    #[case::sextillions(
        "three sextillion twelve quintillion four hundred quadrillion fifty trillion six billion seven hundred million eighty thousand nine",
        "3012400050006700080009"
    )]
    #[case::million_keeps_its_word("two million", "2 million")]
    #[case::hundreds_of_millions("six hundred million", "600 million")]
    #[case::trillion_keeps_its_word("forty trillion", "40 trillion")]
    #[case::decimal("two point five", "2.5")]
    #[case::decimal_digits("one point two three", "1.23")]
    #[case::decimal_teen("four point seventeen", "4.17")]
    #[case::decimal_tens("two point twenty five", "2.25")]
    #[case::decimal_below_one("zero point five", "0.5")]
    #[case::decimal_zero_as_o("two point o five", "2.05")]
    #[case::decimal_zero_as_oh("two point oh five", "2.05")]
    #[case::decimal_zero_word("two point zero five", "2.05")]
    #[case::decimal_million("seven point three million", "7.3 million")]
    #[case::decimal_thousand("three hundred twelve point four six thousand", "312.46 thousand")]
    #[case::thousands_before_the_point(
        "one thousand six hundred nine point two five one billion",
        "1609.251 billion"
    )]
    fn writes(#[case] said: &str, #[case] digits: &str) {
        assert_eq!(written(said).as_deref(), Some(digits));
    }

    #[rstest]
    #[case::digit_string("one two three")]
    #[case::two_digits("two one")]
    #[case::two_tens("fifty fifty")]
    #[case::year_pair("nineteen ninety nine")]
    #[case::tens_then_teen("twenty eleven")]
    #[case::hundred_first("hundred fifty")]
    #[case::scale_first("thousand")]
    #[case::round_tens_of_hundreds("twenty hundred")]
    #[case::zero_in_a_group("twenty zero")]
    #[case::zero_then_more("zero five")]
    #[case::scales_ascending("two thousand three million")]
    #[case::scale_twice("two thousand three thousand")]
    #[case::hundreds_after_a_scale("two thousand fifteen hundred")]
    #[case::point_first("point five")]
    #[case::point_last("two point")]
    #[case::two_points("two point five point six")]
    #[case::and_between_digits("one and two")]
    #[case::and_last("one hundred and")]
    #[case::o_before_the_point("two o five")]
    #[case::other_word("two apples")]
    #[case::empty("")]
    fn has_no_written_form(#[case] said: &str) {
        assert_eq!(written(said), None);
    }
}
