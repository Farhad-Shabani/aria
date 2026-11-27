mod clock;
mod date;
mod grammar;
mod unit;

use std::borrow::Cow;
use std::ops::Range;

use self::clock::{Form, Meridiem};
use self::date::Month;
use self::grammar::{Number, Spoken};
use self::unit::Unit;
use super::claim::{Claimed, Word};
use super::number_words::{
    Next, compound_ordinal, continues, is_digit, is_number_word, is_oh, is_ordinal, ordinal_value,
};
use super::symbol_words::{LONGEST_NAME, is_symbol, name_at};

/// Claims each run of number words that has one clear written form as
/// digits: "two point five" → `2.5`, "twenty fifteen" → `2015`, "nine five
/// five" → `955`. Other runs stay words and are kept whole, so a stream
/// never cuts inside them: "nine fifty five", "twenty second".
///
/// A time needs its meridiem after it, "nine fifty five pm" → `9:55 PM`,
/// and a date its month before it, "March fifth" → `March 5`.
///
/// Where a spoken symbol joins two words that both have a joined form
/// (`Word::joined`), each types that form:
///
/// - a number typed in digits, unless a unit follows: `20.5` in "twenty
///   dot five"
/// - a lone digit word, which stays a word elsewhere: `2.5` in "two dot
///   five", but "no one knows"
/// - a run with no written form, read like the words after "point": `3.14`
///   in "three dot one four", with "o" or "oh" as a zero
/// - digits already written: `30.100` in "30 dot one zero zero"
///
/// A number the text ends on is deferred while more speech may go on with
/// it: more number words, a "point" or "and" it ends on, cents after its
/// dollars, or a day and year after a month.
pub(super) fn claim<'a>(text: &str, claimed: &mut Claimed<'a>) {
    let words = claimed.take_words();
    let scan = Scan::new(text, &words);
    let mut index = 0;
    while index < words.len() {
        let whole = |end: usize| words[index].span.start..words[end - 1].span.end;
        if let Some(month) = scan.heard(index).and_then(Month::parse) {
            if scan.day_may_follow(index) {
                claimed.defer_at(words[index].span.start);
            }
            if let Some(date) = scan.date(month, index) {
                claimed.push(Word::claimed(whole(date.end), Cow::Owned(date.text)));
                index = date.end;
                continue;
            }
        }
        let Some(run) = scan.run_from(index) else {
            claimed.push(written_in_digits(text, words[index].clone()));
            index += 1;
            continue;
        };
        if scan.ends_bare() && scan.takes_to(index) == Some(words.len()) {
            claimed.defer_at(words[index].span.start);
        }
        match scan.spelling(&run) {
            Spelling::Written { written, joins } => {
                claimed.push(Word {
                    joined: joins.then(|| Cow::Owned(written.text.clone())),
                    ..Word::claimed(whole(written.end), Cow::Owned(written.text))
                });
                index = written.end;
            }
            Spelling::LoneDigit { digit } => {
                claimed.push(Word {
                    joined: Some(Cow::Owned(digit)),
                    ..words[index].clone()
                });
                index = run.end;
            }
            Spelling::Joinable { digits } => {
                claimed.keep_whole(whole(run.end));
                claimed.push(Word {
                    joined: Some(Cow::Owned(digits)),
                    ..Word::plain(whole(run.end))
                });
                index = run.end;
            }
            Spelling::Words { end } => {
                claimed.keep_whole(whole(end));
                claimed.extend(words[run.clone()].iter().cloned());
                index = run.end;
            }
        }
    }
}

/// `word` with itself as its joined form when it is written in digits.
fn written_in_digits<'a>(text: &str, word: Word<'a>) -> Word<'a> {
    let typed = word.typed(text);
    let in_digits = typed.starts_with(|c: char| c.is_ascii_digit())
        && typed.ends_with(|c: char| c.is_ascii_digit())
        && typed.chars().all(|c| c.is_ascii_digit() || is_symbol(c));
    if word.joined.is_some() || !in_digits {
        return word;
    }
    let joined = Some(Cow::Owned(typed.to_owned()));
    Word { joined, ..word }
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

/// How a run of number words types.
enum Spelling {
    /// As digits. `joins` when a spoken symbol may join it to another
    /// number, which a unit after it rules out.
    Written { written: Written, joins: bool },
    /// As the word, though a spoken symbol joining it to another number
    /// types the digit: "one" alone, `1.2` in "one dot two".
    LoneDigit { digit: String },
    /// As words, though a spoken symbol joining them to a number types
    /// `digits`: `3.14` in "three dot one four".
    Joinable { digits: String },
    /// As words, kept whole up to `end`: "twenty second", "thirteen thirty
    /// pm".
    Words { end: usize },
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

    /// Whether the text ends on a word no step claimed, with no punctuation
    /// after it: only then may more speech go on with a number there.
    fn ends_bare(&self) -> bool {
        self.parts
            .last()
            .and_then(Option::as_ref)
            .is_some_and(|last| last.trail.is_empty())
    }

    fn part(&self, index: usize) -> Option<&Parts<'t>> {
        self.parts.get(index)?.as_ref()
    }

    /// The word at `index`, in lowercase, when no earlier step claimed it.
    fn word(&self, index: usize) -> Option<&str> {
        self.part(index).map(|part| part.core.as_str())
    }

    /// The word at `index` as heard, in its case and without its
    /// punctuation, when no earlier step claimed it.
    fn heard(&self, index: usize) -> Option<&'t str> {
        let part = self.part(index)?;
        let span = &self.words[index].span;
        Some(&self.text[span.start + part.lead.len()..span.end - part.trail.len()])
    }

    /// Whether the word at `index` opens on a lowercase letter.
    fn opens_lowercase(&self, index: usize) -> bool {
        self.words
            .get(index)
            .and_then(|word| {
                self.text[word.span.clone()]
                    .chars()
                    .find(|c| c.is_alphanumeric())
            })
            .is_some_and(char::is_lowercase)
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

    /// What comes after the word at `index`.
    fn next_after(&self, index: usize) -> Next<'_> {
        match self.following(index) {
            Some(word) => Next::Word(word),
            None if index + 1 == self.words.len() && self.ends_bare() => Next::End,
            None => Next::Stop,
        }
    }

    /// The number words from `start` on that read on from each other, with
    /// a "point" or "and" they end on: more speech may still read on from
    /// those. They may not parse as one number, as "one two three" doesn't.
    fn said_from(&self, start: usize) -> Option<Range<usize>> {
        let after_symbol = self.after_symbol(start);
        let first = self.word(start)?;
        if !is_number_word(first) && !(after_symbol && is_oh(first)) {
            return None;
        }
        let mut last = start;
        let mut reads_digits = after_symbol;
        while let Some(next) = self.following(last) {
            if !continues(
                next,
                self.word(last)?,
                self.next_after(last + 1),
                reads_digits,
            ) {
                break;
            }
            reads_digits |= next == "point";
            last += 1;
        }
        Some(start..last + 1)
    }

    /// Whether a spoken symbol reads on into the word at `start`.
    fn after_symbol(&self, start: usize) -> bool {
        let Some(before) = start.checked_sub(1) else {
            return false;
        };
        self.following(before).is_some() && self.ends_a_name(before)
    }

    /// Whether the words ending at `last` name a symbol, as "back slash"
    /// does in "thirty back slash o one" → `30\01`.
    fn ends_a_name(&self, last: usize) -> bool {
        let earliest = (last + 1).saturating_sub(LONGEST_NAME);
        (earliest..=last).rev().any(|first| {
            let said = (first..=last).map_while(|index| self.word(index));
            name_at(said).is_some_and(|name| name.words == last - first + 1)
        })
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

    /// The number `run` says. A year a unit follows has none: "nineteen
    /// ninety nine dollars" may be $19.99.
    fn number(&self, run: &Range<usize>) -> Option<Number> {
        let said: Vec<&str> = run.clone().filter_map(|index| self.word(index)).collect();
        let number = Number::parse(&said)?;
        let year_before_a_unit = number.spoken() == Spoken::Halves
            && self
                .following(run.end - 1)
                .is_some_and(|word| Unit::parse(word, &number).is_some());
        (!year_before_a_unit).then_some(number)
    }

    /// The meridiem said right after `run`, and its form.
    fn meridiem_after(&self, run: &Range<usize>) -> Option<(Meridiem, Form)> {
        self.following(run.end - 1)?;
        Meridiem::parse(self.heard(run.end)?)
    }

    /// `run` as a time of day, with the `meridiem` said after it in `form`.
    fn time(&self, run: &Range<usize>, meridiem: Meridiem, form: Form) -> Option<Written> {
        let said: Vec<&str> = run.clone().filter_map(|index| self.word(index)).collect();
        let (hour, minutes) = grammar::clock(&said)?;
        if form == Form::AlsoAVerb && minutes.is_none() {
            return None;
        }
        let end = run.end + 1;
        let lead = self.part(run.start).map_or("", |part| part.lead);
        let trail = self.part(run.end).map_or("", |part| part.trail);
        // Before a lowercase word, the period is the abbreviation's alone.
        let trail = match form {
            Form::Dotted if self.opens_lowercase(end) => trail.strip_prefix('.').unwrap_or(trail),
            _ => trail,
        };
        let text = format!("{lead}{}{trail}", meridiem.write(hour, minutes));
        Some(Written { end, text })
    }

    /// The date that opens at `start` on `month`'s name: "March fifth" is
    /// `March 5`, and with its year, `March 5, 2015`.
    fn date(&self, month: Month, start: usize) -> Option<Written> {
        self.following(start)?;
        let (day, day_end) = self.day(start + 1)?;
        let (year, end) = self
            .year_at(day_end)
            .map_or((None, day_end), |(year, end)| (Some(year), end));
        let text = month.write(day, year)?;
        Some(Written {
            end,
            text: self.punctuated(start, &text, end),
        })
    }

    /// The day of a month said as an ordinal from `start`, "fifth",
    /// "twenty-first" or "twenty first", and just past it.
    fn day(&self, start: usize) -> Option<(u128, usize)> {
        let word = self.word(start)?;
        if let Some(day) = ordinal_value(word) {
            return Some((day, start + 1));
        }
        let unit = self.following(start)?;
        Some((compound_ordinal(word, unit)?, start + 2))
    }

    /// The year said by the run that opens at `start`, "twenty fifteen" or
    /// "two thousand one", and where it ends.
    fn year_at(&self, start: usize) -> Option<(u128, usize)> {
        self.following(start - 1)?;
        let run = self.run_from(start)?;
        let year = self
            .number(&run)?
            .integer()
            .filter(|year| (1000..10_000).contains(year))?;
        Some((year, run.end))
    }

    /// Whether the text ends past the month at `index` on nothing but
    /// number words or ordinals: more speech may still say its day.
    fn day_may_follow(&self, index: usize) -> bool {
        self.ends_bare()
            && (index + 1..self.words.len()).all(|at| {
                self.following(at - 1)
                    .is_some_and(|word| is_number_word(word) || is_ordinal(word) || is_oh(word))
            })
    }

    /// `run` as typed digits, when it says three or more digit by digit,
    /// with "o" as a zero after the first: "nine oh five" is `905`.
    fn digit_by_digit(&self, run: &Range<usize>) -> Option<String> {
        let said: Vec<&str> = run.clone().filter_map(|index| self.word(index)).collect();
        let (first, rest) = said.split_first()?;
        let spelled = said.len() >= 3
            && is_digit(first)
            && rest.iter().all(|word| is_digit(word) || is_oh(word));
        spelled.then(|| self.digits(run)).flatten()
    }

    /// `run` read the way the words after "point" are, with its punctuation.
    fn digits(&self, run: &Range<usize>) -> Option<String> {
        let said: Vec<&str> = run.clone().filter_map(|index| self.word(index)).collect();
        let digits = grammar::digits(&said)?;
        Some(self.punctuated(run.start, &digits, run.end))
    }

    /// How `run` types: as a time when a meridiem follows it, else as the
    /// number it says, else digit by digit.
    fn spelling(&self, run: &Range<usize>) -> Spelling {
        if let Some((meridiem, form)) = self.meridiem_after(run) {
            match self.time(run, meridiem, form) {
                Some(written) => {
                    return Spelling::Written {
                        written,
                        joins: false,
                    };
                }
                // "which one am I" says no time, but its number may still type.
                None if form == Form::AlsoAVerb => {}
                None => return Spelling::Words { end: run.end },
            }
        }
        let Some(number) = self.number(run) else {
            if let Some(text) = self.digit_by_digit(run) {
                let written = Written { end: run.end, text };
                return Spelling::Written {
                    written,
                    joins: true,
                };
            }
            return match self.digits(run) {
                Some(digits) => Spelling::Joinable { digits },
                None => Spelling::Words { end: run.end },
            };
        };
        if let Some(counted) = self.counted(&number, run) {
            let text = self.punctuated(run.start, &counted.text, counted.end);
            return Spelling::Written {
                written: Written { text, ..counted },
                joins: false,
            };
        }
        if let Some(end) = self.ordinal_end(run) {
            return Spelling::Words { end };
        }
        let text = self.punctuated(run.start, &number.to_string(), run.end);
        // "one" or "nine" alone reads as a word in prose: "no one knows".
        if run.len() == 1 && number.integer().is_some_and(|digit| digit < 10) {
            return Spelling::LoneDigit { digit: text };
        }
        Spelling::Written {
            written: Written { end: run.end, text },
            joins: true,
        }
    }

    /// `text` with the punctuation around the words from `start` up to
    /// `end`.
    fn punctuated(&self, start: usize, text: &str, end: usize) -> String {
        let lead = self.part(start).map_or("", |part| part.lead);
        let trail = self.part(end - 1).map_or("", |part| part.trail);
        format!("{lead}{text}{trail}")
    }

    /// Just past the ordinal after `run`, past an "and": "twenty second"
    /// and "one hundred and first" are ordinals, not a number and a word.
    fn ordinal_end(&self, run: &Range<usize>) -> Option<usize> {
        let (word, at) = match self.following(run.end - 1)? {
            "and" => (self.following(run.end)?, run.end + 1),
            word => (word, run.end),
        };
        is_ordinal(word).then_some(at + 1)
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
    /// words with a "point" or "and" after them, an "and" an ordinal may
    /// follow, or its dollars with the cents after those.
    fn takes_to(&self, start: usize) -> Option<usize> {
        let said = self.said_from(start)?;
        let Some(dollars_at) = self.dollars_after(&said) else {
            let and = self.following(said.end - 1) == Some("and");
            return Some(said.end + usize::from(and));
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

    use super::super::{Vocabulary, Writing};

    fn written(heard: &str) -> String {
        Vocabulary::default()
            .correct_spoken(heard, Writing::Commands)
            .map_or_else(|| heard.to_owned(), |corrected| corrected.text)
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
    #[case::o_before_punctuation("ten oh, my", "10 oh, my")]
    #[case::digit_word_a_symbol_joins_to_a_word("user underscore one", "user_one")]
    #[case::digits_inside_a_longer_join("user underscore one dot two", "user_1.2")]
    #[case::word_after_joined_digits("two dot five underscore user", "2.5_user")]
    #[case::digits_after_a_symbol("thirty dot one zero zero", "30.100")]
    #[case::digit_words_after_a_symbol("three dot one four", "3.14")]
    #[case::zeros_around_a_symbol("zero dot zero one", "0.01")]
    #[case::digits_on_both_sides("one nine two dot one six eight", "192.168")]
    #[case::digits_after_a_version("version two dot one zero", "version 2.10")]
    #[case::oh_opening_digits_after_a_symbol("thirty dot o one", "30.01")]
    #[case::oh_after_a_name_of_two_words("thirty back slash o one", "30\\01")]
    #[case::oh_inside_digits_after_a_symbol("three dot one oh one", "3.101")]
    #[case::digits_the_model_wrote_before_a_symbol("30 dot one zero zero", "30.100")]
    #[case::digit_word_beside_a_scale_word("two million dot five", "2 million dot five")]
    #[case::digit_word_beside_dollars("two dot five dollars", "two dot $5")]
    #[case::digit_word_beside_a_scale_alone("hundred dot five", "hundred.five")]
    #[case::number_before_a_measure("twenty kilograms", "20 kilograms")]
    #[case::number_after_a_hyphen("utf hyphen sixteen", "utf-16")]
    #[case::number_after_a_hyphen_opening_the_dictation("hyphen nineteen", "-19")]
    #[case::dollars_before_other_words("five dollars and twenty people", "$5 and 20 people")]
    // "x minus ten" may be a sum, so "minus" never signs a number.
    #[case::minus_stays_a_word("x minus ten", "x minus 10")]
    #[case::year("in twenty fifteen", "in 2015")]
    #[case::year_with_oh("twenty oh five", "2005")]
    #[case::digit_words("room nine five five", "room 955")]
    #[case::digit_words_with_oh("python one oh one", "python 101")]
    #[case::digit_words_opening_on_zero("zero two one three nine", "02139")]
    #[case::digit_words_a_symbol_joins("user underscore one two three", "user_123")]
    #[case::time("at nine fifty five pm", "at 9:55 PM")]
    #[case::time_with_oh("nine oh five am", "9:05 AM")]
    #[case::time_past_noon("twelve thirty PM", "12:30 PM")]
    #[case::hour("three pm", "3 PM")]
    #[case::hour_with_periods("three p.m.", "3 PM.")]
    #[case::hour_in_capitals("Three AM", "3 AM")]
    #[case::time_before_am("nine fifty five am", "9:55 AM")]
    #[case::periods_before_a_lowercase_word("at three p.m. we left", "at 3 PM we left")]
    #[case::periods_before_a_comma("three p.m., then", "3 PM, then")]
    #[case::periods_ending_a_sentence("three p.m. We left", "3 PM. We left")]
    #[case::date("on March fifth", "on March 5")]
    #[case::date_in_lowercase("june twenty first", "June 21")]
    #[case::date_in_may("May first", "May 1")]
    #[case::date_with_a_hyphenated_ordinal("March twenty-first", "March 21")]
    #[case::date_with_a_year("March fifth twenty fifteen", "March 5, 2015")]
    #[case::date_with_a_counted_year("June fifth two thousand one", "June 5, 2001")]
    #[case::date_with_its_comma("March fifth, twenty fifteen", "March 5, 2015")]
    #[case::date_with_punctuation_after("(March fifth).", "(March 5).")]
    #[case::month_and_year("March twenty fifteen", "March 2015")]
    #[case::last_day_of_a_month("April thirtieth", "April 30")]
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
    #[case::measure("two kilograms")]
    #[case::another_currency("five euros")]
    #[case::scale_word_first("a hundred people")]
    #[case::scale_word_first_before_more("a hundred and fifty")]
    #[case::run_with_no_one_form("fifty fifty")]
    #[case::and_between_digit_words("one and two")]
    #[case::year_before_dollars("nineteen ninety nine dollars")]
    #[case::time_with_a_lead_under_thirteen("twelve thirty")]
    #[case::time_with_oh("ten oh five")]
    #[case::digit_word_then_tens("room nine fifty five")]
    #[case::two_digit_words("a one two punch")]
    #[case::digit_words_opening_on_oh("oh one two")]
    #[case::year_lead_before_oh_and_a_teen("twenty oh fifteen")]
    #[case::hour_out_of_range("thirteen thirty pm")]
    #[case::am_after_a_lone_digit("which one am I")]
    #[case::may_as_a_verb("you may first need to sign in")]
    #[case::cardinal_day_in_prose("in June five of us went")]
    #[case::day_past_the_month("February thirtieth")]
    #[case::month_alone("the end of March")]
    #[case::digit_run_beside_a_word("user underscore one two")]
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
