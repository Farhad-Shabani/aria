//! Spelled-out numbers rewritten as digits.
//!
//! Makes forms such as `TWENTY TWO` and `22` comparable when scoring transcripts.
//! Follows Whisper's `EnglishNumberNormalizer` for parity with reference scoring.

use std::sync::LazyLock;

use regex::{Captures, Regex};

use super::lexicon::{LEXICON, Token, is_digits};

/// Rewrites every spoken number in the text as digits.
pub(super) fn standardize(text: &str) -> String {
    let prepared = prepare(text);
    let read = Reader::read(&prepared);
    finish(&read)
}

/// The number being read, which is a running integer until something forces it
/// to be treated as a sequence of digits — a decimal point, or a nominal
/// reading like `one oh one`.
enum Value {
    Integer(i128),
    Digits(String),
}

impl Value {
    fn text(&self) -> String {
        match self {
            Self::Integer(value) => value.to_string(),
            Self::Digits(digits) => digits.clone(),
        }
    }
}

/// How much of the input one step consumed. Some words only mean something
/// numeric together — `per cent` is one reading, not two.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Read {
    OneWord,
    WordAndNext,
}

/// Reads a word sequence left to right, emitting each number once it ends.
struct Reader {
    /// A sign or currency symbol waiting for the number it belongs to.
    prefix: Option<char>,
    value: Option<Value>,
    words: Vec<String>,
}

impl Reader {
    fn read(text: &str) -> String {
        let words: Vec<&str> = text.split_whitespace().collect();
        let mut reader = Self {
            prefix: None,
            value: None,
            words: Vec::new(),
        };

        let mut consumed = Read::OneWord;
        for (index, &current) in words.iter().enumerate() {
            if consumed == Read::WordAndNext {
                consumed = Read::OneWord;
                continue;
            }
            let previous = index.checked_sub(1).map(|index| words[index]);
            let next = words.get(index + 1).copied();
            consumed = reader.read_word(current, previous, next);
        }
        reader.emit_pending();
        reader.words.join(" ")
    }

    fn read_word(&mut self, current: &str, previous: Option<&str>, next: Option<&str>) -> Read {
        match LEXICON.classify(current) {
            Token::Digits { prefix, body } => self.read_digits(current, prefix, &body),
            Token::Zero => self.value = Some(Value::Digits(self.accumulated() + "0")),
            Token::One(ones) => self.value = Some(self.extend(ones, previous)),
            Token::OneSuffixed(ones, suffix) => {
                let extended = self.extend(ones, previous);
                self.emit(extended.text() + suffix);
            }
            Token::Ten(tens) => self.value = Some(self.extend_by_ten(tens)),
            Token::TenSuffixed(tens, suffix) => {
                let extended = self.extend_by_ten(tens);
                self.emit(extended.text() + suffix);
            }
            Token::Multiplier(multiplier) => self.read_multiplier(multiplier),
            Token::MultiplierSuffixed(multiplier, suffix) => {
                self.read_multiplier(multiplier);
                let scaled = self
                    .value
                    .take()
                    .map(|value| value.text())
                    .unwrap_or_default();
                self.emit(scaled + suffix);
            }
            Token::PrecedingPrefix(symbol) => self.read_preceding_prefix(current, symbol, next),
            Token::FollowingPrefix(symbol) => self.read_following_prefix(current, symbol),
            Token::Percent => self.close_with(current, "%"),
            Token::Per => return self.read_per(current, next),
            Token::And | Token::Double | Token::Triple | Token::Point => {
                return self.read_special(current, previous, next);
            }
            Token::Other => {
                self.emit_pending();
                self.emit(current.to_owned());
            }
        }
        Read::OneWord
    }

    /// Digits already written as digits. They join a decimal in progress and
    /// otherwise start a fresh number.
    fn read_digits(&mut self, current: &str, prefix: Option<char>, body: &str) {
        if let Some(Value::Digits(digits)) = &self.value
            && digits.ends_with('.')
        {
            self.value = Some(Value::Digits(digits.clone() + current));
            return;
        }
        self.emit_pending();
        if prefix.is_some() {
            self.prefix = prefix;
        }
        self.value = Some(match Rational::parse(body).and_then(Rational::whole) {
            Some(whole) => Value::Integer(whole),
            None => Value::Digits(body.to_owned()),
        });
    }

    /// A cardinal one through nineteen joining the number in progress. It
    /// either adds to a round number, replaces the trailing zero of a tens
    /// word, or is simply appended as another digit.
    fn extend(&self, ones: i128, previous: Option<&str>) -> Value {
        let previous_is_one = previous.is_some_and(|word| LEXICON.is_one(word));
        let previous_is_ten = previous.is_some_and(|word| LEXICON.is_ten(word));

        match &self.value {
            None => Value::Integer(ones),
            // A tens word ends in a zero the cardinal takes the place of, so
            // `twenty` then `two` is 22 rather than 202.
            Some(Value::Digits(digits)) if previous_is_ten && ones < 10 => {
                let kept = digits.strip_suffix('0').unwrap_or(digits);
                Value::Digits(kept.to_owned() + &ones.to_string())
            }
            Some(Value::Digits(digits)) => Value::Digits(digits.clone() + &ones.to_string()),
            Some(Value::Integer(value)) if previous_is_one => {
                Value::Digits(value.to_string() + &ones.to_string())
            }
            Some(Value::Integer(value)) => {
                let round = if ones < 10 { 10 } else { 100 };
                match (value % round == 0).then(|| value.checked_add(ones)) {
                    Some(Some(sum)) => Value::Integer(sum),
                    _ => Value::Digits(value.to_string() + &ones.to_string()),
                }
            }
        }
    }

    /// A tens word joining the number in progress.
    fn extend_by_ten(&self, tens: i128) -> Value {
        match &self.value {
            None => Value::Integer(tens),
            Some(Value::Digits(digits)) => Value::Digits(digits.clone() + &tens.to_string()),
            Some(Value::Integer(value)) if value % 100 == 0 => match value.checked_add(tens) {
                Some(sum) => Value::Integer(sum),
                None => Value::Digits(value.to_string() + &tens.to_string()),
            },
            Some(Value::Integer(value)) => Value::Digits(value.to_string() + &tens.to_string()),
        }
    }

    /// A multiplier scales what precedes it, but only the part below the last
    /// thousand: `two thousand five hundred` is 2000 + 5 * 100, not 2500 * 100.
    fn read_multiplier(&mut self, multiplier: i128) {
        let scaled = match self.value.take() {
            None => Value::Integer(multiplier),
            Some(Value::Integer(0)) => Value::Integer(0),
            Some(Value::Integer(value)) => match scale_residual(value, multiplier) {
                Some(scaled) => Value::Integer(scaled),
                // Only reachable for a number far larger than speech
                // produces. Emitting what is held keeps the digits readable
                // rather than wrapping the arithmetic.
                None => {
                    self.emit(Value::Integer(value).text());
                    Value::Integer(multiplier)
                }
            },
            Some(Value::Digits(digits)) => {
                match Rational::parse(&digits).and_then(|value| value.scaled(multiplier)) {
                    Some(scaled) => Value::Integer(scaled),
                    None => {
                        self.emit(digits);
                        Value::Integer(multiplier)
                    }
                }
            }
        };
        self.value = Some(scaled);
    }

    /// A sign binds to the number after it, and is only a sign when a number
    /// actually follows.
    fn read_preceding_prefix(&mut self, current: &str, symbol: char, next: Option<&str>) {
        self.emit_pending();
        if next.is_some_and(|word| LEXICON.is_known(word) || is_digits(word)) {
            self.prefix = Some(symbol);
        } else {
            self.emit(current.to_owned());
        }
    }

    /// A currency name binds to the number before it, and is only a currency
    /// symbol when there is a number to attach it to.
    fn read_following_prefix(&mut self, current: &str, symbol: char) {
        match self.value.take() {
            Some(value) => {
                self.prefix = Some(symbol);
                self.emit(value.text());
            }
            None => self.emit(current.to_owned()),
        }
    }

    /// `per cent` closes the number the same way `percent` does, consuming the
    /// second word when it is there.
    fn read_per(&mut self, current: &str, next: Option<&str>) -> Read {
        let Some(value) = self.value.take() else {
            self.emit(current.to_owned());
            return Read::OneWord;
        };
        if next == Some("cent") {
            self.emit(value.text() + "%");
            return Read::WordAndNext;
        }
        self.emit(value.text());
        self.emit(current.to_owned());
        Read::OneWord
    }

    fn close_with(&mut self, current: &str, suffix: &str) {
        match self.value.take() {
            Some(value) => self.emit(value.text() + suffix),
            None => self.emit(current.to_owned()),
        }
    }

    /// Words that only mean something numeric when a number follows them.
    fn read_special(&mut self, current: &str, previous: Option<&str>, next: Option<&str>) -> Read {
        let next_is_numeric = next.is_some_and(is_digits);
        if !next.is_some_and(|word| LEXICON.is_known(word)) && !next_is_numeric {
            self.emit_pending();
            self.emit(current.to_owned());
            return Read::OneWord;
        }
        match current {
            // `two hundred and five` is one number; `you and me` is not.
            "and" => {
                if !previous.is_some_and(|word| LEXICON.is_multiplier(word)) {
                    self.emit_pending();
                    self.emit(current.to_owned());
                }
                Read::OneWord
            }
            "double" | "triple" => self.read_repeat(current, next),
            // A decimal point, dropped when what follows cannot be decimal.
            _ => {
                if next.is_some_and(|word| LEXICON.is_decimal(word)) || next_is_numeric {
                    self.value = Some(Value::Digits(self.accumulated() + "."));
                }
                Read::OneWord
            }
        }
    }

    /// `double seven` is 77.
    fn read_repeat(&mut self, current: &str, next: Option<&str>) -> Read {
        let Some(word) = next.filter(|word| LEXICON.is_one(word) || LEXICON.is_zero(word)) else {
            self.emit_pending();
            self.emit(current.to_owned());
            return Read::OneWord;
        };
        let repeats = if current == "double" { 2 } else { 3 };
        let digit = LEXICON.one_value(word).unwrap_or(0).to_string();
        self.value = Some(Value::Digits(self.accumulated() + &digit.repeat(repeats)));
        Read::WordAndNext
    }

    /// The digits accumulated so far. A bare `0` counts as nothing to
    /// append to, so `0 point five` reads as `.5` rather than `0.5`.
    fn accumulated(&self) -> String {
        match &self.value {
            None | Some(Value::Integer(0)) => String::new(),
            Some(value) => value.text(),
        }
    }

    fn emit_pending(&mut self) {
        if let Some(value) = self.value.take() {
            self.emit(value.text());
        }
    }

    /// Emits one finished word, giving it the waiting prefix and clearing the
    /// number in progress.
    fn emit(&mut self, word: String) {
        let word = match self.prefix.take() {
            Some(prefix) => format!("{prefix}{word}"),
            None => word,
        };
        self.value = None;
        self.words.push(word);
    }
}

/// Scales only the part of a value below the last full thousand.
fn scale_residual(value: i128, multiplier: i128) -> Option<i128> {
    let below_thousand = value % 1000;
    let above = value - below_thousand;
    below_thousand.checked_mul(multiplier)?.checked_add(above)
}

/// A decimal written as digits, kept exact so scaling can tell a whole result
/// from a fractional one: `two point five hundred` is 250, but `one point two
/// three four hundred` is not a whole number.
struct Rational {
    numerator: i128,
    denominator: i128,
}

impl Rational {
    fn parse(digits: &str) -> Option<Self> {
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        let numerator = format!("{whole}{fraction}").parse().ok()?;
        let denominator = 10_i128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
        Some(Self {
            numerator,
            denominator,
        })
    }

    fn whole(self) -> Option<i128> {
        self.scaled(1)
    }

    /// The value times `multiplier`, when that product is a whole number.
    fn scaled(self, multiplier: i128) -> Option<i128> {
        let scaled = self.numerator.checked_mul(multiplier)?;
        (scaled % self.denominator == 0).then_some(scaled / self.denominator)
    }
}

/// `<number> and a half` reads as `<number> point five`, but only where the
/// half attaches to a number.
static AND_A_HALF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\band\s+a\s+half\b").expect("the pattern should compile"));
static LETTER_THEN_DIGIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([a-z])([0-9])").expect("the pattern should compile"));
static DIGIT_THEN_LETTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([0-9])([a-z])").expect("the pattern should compile"));
static SPLIT_SUFFIX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([0-9])\s+(st|nd|rd|th|s)\b").expect("the pattern should compile")
});

fn prepare(text: &str) -> String {
    let spelled = spell_out_halves(text);
    // Numbers and letters that ran together are separated so each is read on
    // its own, except where the letters are the number's own suffix.
    let separated = LETTER_THEN_DIGIT.replace_all(&spelled, "$1 $2");
    let separated = DIGIT_THEN_LETTER.replace_all(&separated, "$1 $2");
    SPLIT_SUFFIX.replace_all(&separated, "$1$2").into_owned()
}

fn spell_out_halves(text: &str) -> String {
    let segments: Vec<&str> = AND_A_HALF.split(text).collect();
    let last = segments.len().saturating_sub(1);
    let mut spelled: Vec<&str> = Vec::new();

    for (index, segment) in segments.iter().enumerate() {
        if segment.trim().is_empty() {
            continue;
        }
        spelled.push(segment);
        if index == last {
            continue;
        }
        let attaches_to_a_number = segment
            .split_whitespace()
            .next_back()
            .is_some_and(|word| LEXICON.is_decimal(word) || LEXICON.is_multiplier(word));
        spelled.push(if attaches_to_a_number {
            "point five"
        } else {
            "and a half"
        });
    }
    spelled.join(" ")
}

static CURRENCY_AND_CENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([€£$])([0-9]+) (?:and )?¢([0-9]{1,2})\b").expect("the pattern should compile")
});
static BARE_CENTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[€£$]0.([0-9]{1,2})\b").expect("the pattern should compile"));
static LONE_ONE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b1(s?)\b").expect("the pattern should compile"));

fn finish(text: &str) -> String {
    let combined = CURRENCY_AND_CENTS.replace_all(text, |captures: &Captures<'_>| {
        let cents: u32 = captures[3].parse().unwrap_or_default();
        format!("{}{}.{cents:02}", &captures[1], &captures[2])
    });
    let extracted = BARE_CENTS.replace_all(&combined, |captures: &Captures<'_>| {
        let cents: u32 = captures[1].parse().unwrap_or_default();
        format!("¢{cents}")
    });
    // `1` reads worse than `one` in running text, and both sides of a
    // comparison get the same treatment.
    LONE_ONE.replace_all(&extracted, "one$1").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cardinals_become_digits() {
        assert_eq!(standardize("twenty two"), "22");
        assert_eq!(standardize("two hundred and five"), "205");
        assert_eq!(standardize("two thousand five hundred"), "2500");
    }

    #[test]
    fn successive_digits_read_nominally() {
        assert_eq!(standardize("one oh one"), "101");
        assert_eq!(standardize("double seven"), "77");
    }

    #[test]
    fn ordinals_keep_their_suffix() {
        assert_eq!(standardize("the twenty second"), "the 22nd");
        assert_eq!(standardize("the nineteen sixties"), "the 1960s");
    }

    #[test]
    fn currency_binds_to_the_number_before_it() {
        assert_eq!(standardize("twenty dollars"), "$20");
        assert_eq!(standardize("twenty million dollars"), "$20000000");
    }

    #[test]
    fn percent_closes_the_number() {
        assert_eq!(standardize("five percent"), "5%");
        assert_eq!(standardize("five per cent"), "5%");
    }

    #[test]
    fn decimals_and_halves() {
        assert_eq!(standardize("three point five"), "3.5");
        assert_eq!(standardize("two and a half million"), "2500000");
    }

    #[test]
    fn a_lone_one_stays_a_word() {
        assert_eq!(standardize("one"), "one");
        assert_eq!(standardize("twenty one"), "21");
    }

    #[test]
    fn ordinary_words_pass_through() {
        assert_eq!(standardize("the socket closes"), "the socket closes");
    }

    /// A hypothesis is whatever a model chose to emit, so a digit run longer
    /// than any integer can hold has to degrade to text rather than wrap.
    #[test]
    fn a_number_too_large_to_hold_stays_readable() {
        let huge = "170141183460469231731687303715884105720";
        assert_eq!(standardize(&format!("{huge} eight")), format!("{huge}8"));
        assert_eq!(
            standardize("170141183460469231731687303715884105700 ninety"),
            "17014118346046923173168730371588410570090"
        );
    }
}
