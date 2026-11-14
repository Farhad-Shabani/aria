mod grammar;
mod unit;

use std::borrow::Cow;
use std::ops::Range;

use self::grammar::Number;
use self::unit::Unit;
use super::number_words::{continues, is_number_word, is_ordinal};
use super::symbol_words::symbol;
use super::symbols;
use super::vocabulary::Word;

/// Claims each run of number words that has one clear written form, as in
/// "two point five" → `2.5`. A run with none stays words, whole.
pub(super) fn claim<'a>(text: &str, words: Vec<Word<'a>>) -> Vec<Word<'a>> {
    let scan = Scan::new(text, &words);
    let mut claimed = Vec::with_capacity(words.len());
    let mut previous = None;
    let mut index = 0;
    while index < words.len() {
        let Some(run) = scan.run_from(index) else {
            claimed.push(words[index].clone());
            index += 1;
            continue;
        };
        match scan.written(&run, previous.as_ref()) {
            Some(written) => {
                claimed.push(Word {
                    span: words[index].span.start..words[written.end - 1].span.end,
                    claim: Some(Cow::Owned(written.text)),
                });
                index = written.end;
            }
            None => {
                claimed.extend_from_slice(&words[run.clone()]);
                index = run.end;
            }
        }
        previous = Some(run);
    }
    claimed
}

/// How many of the words `words` ends on more speech may still change: a
/// number that may go on, or dollars that cents may follow.
pub(super) fn open_tail(text: &str, words: &[Word<'_>]) -> usize {
    let scan = Scan::new(text, words);
    let ends_bare = scan
        .parts
        .last()
        .and_then(Option::as_ref)
        .is_some_and(|last| last.trail.is_empty());
    if !ends_bare {
        return 0;
    }
    (0..words.len())
        .find(|start| scan.open_end(*start) == Some(words.len()))
        .map_or(0, |start| words.len() - start)
}

/// One word, split from the punctuation around it.
struct Parts<'t> {
    lead: &'t str,
    /// In lowercase.
    core: String,
    trail: &'t str,
}

impl<'t> Parts<'t> {
    fn of(word: &'t str) -> Self {
        let start = word.find(char::is_alphanumeric).unwrap_or(word.len());
        let end = word
            .char_indices()
            .rfind(|(_, c)| c.is_alphanumeric())
            .map_or(start, |(at, c)| at + c.len_utf8());
        Self {
            lead: &word[..start],
            core: word[start..end].to_lowercase(),
            trail: &word[end..],
        }
    }
}

/// A number as typed, and the word just past the ones that went into it.
struct Written {
    end: usize,
    text: String,
}

/// A segment's words, read for the numbers in them.
struct Scan<'t, 'w, 'a> {
    text: &'t str,
    words: &'w [Word<'a>],
    /// `None` for a word an earlier step claimed: no number takes it.
    parts: Vec<Option<Parts<'t>>>,
}

impl<'t, 'w, 'a> Scan<'t, 'w, 'a> {
    fn new(text: &'t str, words: &'w [Word<'a>]) -> Self {
        let parts = words
            .iter()
            .map(|word| {
                word.claim
                    .is_none()
                    .then(|| Parts::of(&text[word.span.clone()]))
            })
            .collect();
        Self { text, words, parts }
    }

    fn part(&self, index: usize) -> Option<&Parts<'t>> {
        self.parts.get(index)?.as_ref()
    }

    /// The word at `index`, in lowercase, when no earlier step claimed it.
    fn word(&self, index: usize) -> Option<&str> {
        self.part(index).map(|part| part.core.as_str())
    }

    /// The word after `index`, when it reads on from it: neither claimed,
    /// with only spaces between and no punctuation on either side.
    fn following(&self, index: usize) -> Option<&str> {
        let (word, next) = (self.part(index)?, self.part(index + 1)?);
        let between = &self.text[self.words[index].span.end..self.words[index + 1].span.start];
        let reads_on = word.trail.is_empty()
            && next.lead.is_empty()
            && !between.is_empty()
            && !between.contains(|c: char| c == '\n' || !c.is_whitespace());
        reads_on.then_some(next.core.as_str())
    }

    /// The words from `start` on that say one number, with a "point" or
    /// "and" they end on: more speech may still read on from those.
    fn said_from(&self, start: usize) -> Option<Range<usize>> {
        if !is_number_word(self.word(start)?) {
            return None;
        }
        let mut last = start;
        let mut after_point = false;
        while let Some(next) = self.following(last) {
            if !continues(next, self.word(last)?, after_point) {
                break;
            }
            after_point |= next == "point";
            last += 1;
        }
        Some(start..last + 1)
    }

    /// The number run that opens at `start`. A "point" or "and" the run
    /// would end on stays outside it.
    fn run_from(&self, start: usize) -> Option<Range<usize>> {
        let mut run = self.said_from(start)?;
        while matches!(self.word(run.end - 1), Some("point" | "and")) {
            run.end -= 1;
        }
        Some(run)
    }

    fn number(&self, run: &Range<usize>) -> Option<Number> {
        let said: Vec<&str> = run.clone().filter_map(|index| self.word(index)).collect();
        Number::parse(&said)
    }

    /// How `run` is written. `None` leaves it as words. `previous` is the
    /// run before it.
    fn written(&self, run: &Range<usize>, previous: Option<&Range<usize>>) -> Option<Written> {
        let number = self.number(run)?;
        let written = match self.counted(&number, run) {
            Some(counted) => counted,
            None if self.stays_words(&number, run, previous) => return None,
            None => Written {
                end: run.end,
                text: number.to_string(),
            },
        };
        Some(Written {
            text: self.punctuated(run.start, &written.text, written.end),
            ..written
        })
    }

    /// `text` with the punctuation around the words from `start` up to
    /// `end`.
    fn punctuated(&self, start: usize, text: &str, end: usize) -> String {
        let lead = self.part(start).map_or("", |part| part.lead);
        let trail = self.part(end - 1).map_or("", |part| part.trail);
        format!("{lead}{text}{trail}")
    }

    /// Whether a number with no unit keeps its words.
    fn stays_words(
        &self,
        number: &Number,
        run: &Range<usize>,
        previous: Option<&Range<usize>>,
    ) -> bool {
        // "one" or "nine" alone reads as a word in prose: "no one knows".
        let lone_digit = run.len() == 1 && number.integer().is_some_and(|digit| digit < 10);
        self.opens_an_ordinal(run)
            || self.follows_an_opening_hyphen(run)
            || (lone_digit && !self.symbol_joins_a_number(run, previous))
    }

    /// Whether an ordinal follows `run`, past an "and": "twenty second" and
    /// "one hundred and first" are ordinals, not a number and a word.
    fn opens_an_ordinal(&self, run: &Range<usize>) -> bool {
        let next = match self.following(run.end - 1) {
            Some("and") => self.following(run.end),
            next => next,
        };
        next.is_some_and(is_ordinal)
    }

    /// Whether `run` follows a "hyphen" that opens the text. That hyphen
    /// joins what was typed before, and typed text can't take a `-5`.
    fn follows_an_opening_hyphen(&self, run: &Range<usize>) -> bool {
        run.start == 1 && self.word(0).and_then(symbol) == Some('-') && self.following(0).is_some()
    }

    /// Whether a spoken symbol joins the digit word `run` to a number
    /// written on either side, as in "two dot five". `previous` is the run
    /// before it.
    fn symbol_joins_a_number(&self, run: &Range<usize>, previous: Option<&Range<usize>>) -> bool {
        let joins = |left: &Range<usize>, right: &Range<usize>| {
            self.symbol_links(left.end - 1, right.start)
                && self
                    .alone(left)
                    .zip(self.alone(right))
                    .is_some_and(|(left, right)| symbols::joins(&left, &right))
        };
        let next = self.run_from(run.start + 2);
        previous.is_some_and(|previous| joins(previous, run))
            || next.is_some_and(|next| joins(run, &next))
    }

    /// Whether one symbol word sits between the words at `left` and
    /// `right`, reading on from one to the other.
    fn symbol_links(&self, left: usize, right: usize) -> bool {
        left + 2 == right
            && self.following(left).and_then(symbol).is_some()
            && self.following(left + 1).is_some()
    }

    /// What `run` types as a number alone, digit word or not: no unit
    /// counts it and no ordinal follows it.
    fn alone(&self, run: &Range<usize>) -> Option<String> {
        let number = self.number(run)?;
        let alone = self.counted(&number, run).is_none() && !self.opens_an_ordinal(run);
        alone.then(|| self.punctuated(run.start, &number.to_string(), run.end))
    }

    /// `number` with the unit said after `run`.
    fn counted(&self, number: &Number, run: &Range<usize>) -> Option<Written> {
        let unit = Unit::parse(self.following(run.end - 1)?, number)?;
        let unit_at = run.end;
        let with_cents = match unit {
            Unit::Dollar => self.with_cents(number, unit_at),
            Unit::Cent | Unit::Percent => None,
        };
        with_cents.or_else(|| {
            Some(Written {
                end: unit_at + 1,
                text: unit.write(number)?,
            })
        })
    }

    /// `dollars` and the cents said after "dollars" at `dollars_at`, as in
    /// `$5.20`.
    fn with_cents(&self, dollars: &Number, dollars_at: usize) -> Option<Written> {
        let start = self.cents_start(dollars_at);
        self.following(start - 1)?;
        let run = self.run_from(start)?;
        let cents = self.number(&run)?;
        let unit = Unit::parse(self.following(run.end - 1)?, &cents)?;
        let text = unit::dollars_and_cents(dollars, &cents).filter(|_| unit == Unit::Cent)?;
        Some(Written {
            end: run.end + 1,
            text,
        })
    }

    /// Where cents open after "dollars" at `dollars_at`: the next word, or
    /// the one past an "and".
    fn cents_start(&self, dollars_at: usize) -> usize {
        match self.following(dollars_at) {
            Some("and") => dollars_at + 2,
            _ => dollars_at + 1,
        }
    }

    /// Where what the number opening at `start` may still take ends: its
    /// words with a "point" or "and" after them, or its dollars with the
    /// cents after those. Everything [`claim`] joins is here, so a stream
    /// never types half of it.
    fn open_end(&self, start: usize) -> Option<usize> {
        let said = self.said_from(start)?;
        let Some(dollars_at) = self.dollars_after(&said) else {
            return Some(said.end);
        };
        let cents_at = self.cents_start(dollars_at);
        let cents = self
            .following(cents_at - 1)
            .and_then(|_| self.said_from(cents_at));
        Some(cents.map_or(cents_at, |cents| cents.end))
    }

    /// Where "dollars" sits when it counts the number `said` says.
    fn dollars_after(&self, said: &Range<usize>) -> Option<usize> {
        let number = self.number(said)?;
        let unit = Unit::parse(self.following(said.end - 1)?, &number)?;
        (unit == Unit::Dollar).then_some(said.end)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::Vocabulary;

    fn written(heard: &str) -> String {
        Vocabulary::default()
            .correct_spoken(heard, "")
            .unwrap_or_else(|| heard.to_owned())
    }

    #[rstest]
    #[case::decimal("two point five", "2.5")]
    #[case::compound("twenty one apples", "21 apples")]
    #[case::hyphenated_compound("twenty-one apples", "21 apples")]
    #[case::capitalized("Twenty One apples", "21 apples")]
    #[case::hundreds_with_and("one hundred and five", "105")]
    #[case::millions_keep_the_word("two million people", "2 million people")]
    #[case::single_word_of_ten_or_more("top twelve", "top 12")]
    #[case::digit_with_dollars("it costs five dollars", "it costs $5")]
    #[case::one_dollar("one dollar", "$1")]
    #[case::singular_after_a_count("a twenty dollar bill", "a $20 bill")]
    #[case::dollars_with_one_decimal("two point five dollars", "$2.50")]
    #[case::dollars_with_a_scale("two point five billion dollars", "$2.5 billion")]
    #[case::dollars_and_cents("thirteen dollars and forty two cents", "$13.42")]
    #[case::dollars_then_cents("eighteen dollars thirty cents", "$18.30")]
    #[case::dollars_and_one_cent("nine dollars and one cent", "$9.01")]
    #[case::cents("twenty five cents", "$0.25")]
    #[case::digit_with_percent("five percent", "5%")]
    #[case::decimal_percent("two point five percent", "2.5%")]
    #[case::keeps_punctuation_around("(twenty one).", "(21).")]
    #[case::unit_keeps_its_punctuation("five dollars.", "$5.")]
    #[case::trailing_and_stays_a_word("one hundred and then", "100 and then")]
    #[case::trailing_point_stays_a_word("twenty point", "20 point")]
    #[case::each_side_of_or("twenty or thirty", "20 or 30")]
    #[case::and_between_tens_splits("twenty and thirty", "20 and 30")]
    #[case::digit_words_a_symbol_joins("version two dot five", "version 2.5")]
    #[case::digit_word_a_symbol_joins_to_a_number("twenty dot five", "20.5")]
    #[case::digit_words_in_a_chain("one dot two dot three", "1.2.3")]
    #[case::o_outside_a_decimal("ten oh my", "10 oh my")]
    #[case::digit_word_a_symbol_joins_to_a_word("user underscore one", "user_one")]
    #[case::digit_word_beside_a_scale_word("two million dot five", "2 million dot five")]
    #[case::digit_word_beside_dollars("two dot five dollars", "two dot $5")]
    #[case::digit_word_beside_digit_words("five dot one two", "five.one two")]
    #[case::digit_word_beside_a_scale_alone("hundred dot five", "hundred.five")]
    #[case::number_before_a_measure("twenty kilograms", "20 kilograms")]
    #[case::number_after_a_hyphen("covid hyphen nineteen", "covid-19")]
    #[case::dollars_before_other_words("five dollars and twenty people", "$5 and 20 people")]
    // "x minus ten" may be a sum, so "minus" never signs a number.
    #[case::minus_stays_a_word("x minus ten", "x minus 10")]
    fn writes(#[case] heard: &str, #[case] typed: &str) {
        assert_eq!(written(heard), typed);
    }

    #[rstest]
    #[case::pronoun("no one knows")]
    #[case::zero_alone("zero")]
    #[case::ordinal_word("give me a second")]
    #[case::first("first of all")]
    #[case::range_of_digit_words("it is a nine to five job")]
    #[case::compound_ordinal("the twenty second of May")]
    #[case::ordinal_past_an_and("one hundred and first")]
    #[case::hyphenated_ordinal("the one hundred twenty-second time")]
    #[case::digit_words_in_curly_quotes("\u{201c}two dot five\u{201d}")]
    #[case::number_after_an_opening_hyphen("hyphen nineteen")]
    #[case::measure("two kilograms")]
    #[case::time_with_a_meridiem("three thirty pm")]
    #[case::another_currency("five euros")]
    #[case::scale_word_first("a hundred people")]
    #[case::scale_word_first_before_more("a hundred and fifty")]
    #[case::run_with_no_one_form("fifty fifty")]
    #[case::and_between_digit_words("one and two")]
    #[case::point_without_a_number_before("point five")]
    #[case::plural_after_one("one dollars")]
    #[case::comma_between("one, two")]
    #[case::line_break_between("one\ntwo")]
    #[case::digits_the_model_wrote("2 point 5")]
    #[case::empty("")]
    fn leaves_unchanged(#[case] heard: &str) {
        assert_eq!(written(heard), heard);
    }
}
