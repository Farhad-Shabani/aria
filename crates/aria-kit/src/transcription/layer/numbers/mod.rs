mod grammar;
mod unit;

use std::borrow::Cow;
use std::ops::Range;

use self::grammar::Number;
use self::unit::Unit;
use super::claim::{Claimed, Word};
use super::number_words::{continues, is_number_word, is_ordinal};
use super::symbol_words::{is_symbol, symbol};

/// Claims each run of number words that has one clear written form, as in
/// "two point five" → `2.5`. A run with none stays words, and is kept whole:
/// a stream never cuts inside it, nor between a number and the ordinal that
/// keeps it words. A lone digit word stays a word, as "one" in "no one
/// knows" is, but keeps its digit for a spoken symbol that joins it to
/// another number: `2.5` in "two dot five". So does a run with no form,
/// read the way the words after "point" are: `3.14` in "three dot one
/// four". After a spoken symbol, as after "point", "o" is a zero. A number
/// already written in digits keeps itself: `30.100` in "30 dot one zero
/// zero".
///
/// A number the text ends on is deferred while more speech may go on with
/// it: more number words, a "point" or "and" it ends on, or cents after its
/// dollars.
pub(super) fn claim<'a>(text: &str, claimed: &mut Claimed<'a>) {
    let words = claimed.take_words();
    let scan = Scan::new(text, &words);
    let mut index = 0;
    while index < words.len() {
        let Some(run) = scan.run_from(index) else {
            claimed.push(written_in_digits(text, words[index].clone()));
            index += 1;
            continue;
        };
        if scan.ends_bare() && scan.takes_to(index) == Some(words.len()) {
            claimed.defer_at(words[index].span.start);
        }
        let whole = |end: usize| words[run.start].span.start..words[end - 1].span.end;
        let Some(number) = scan.number(&run) else {
            claimed.keep_whole(whole(run.end));
            match scan.digits(&run) {
                Some(digits) => claimed.push(Word {
                    joined: Some(Cow::Owned(digits)),
                    ..Word::plain(whole(run.end))
                }),
                None => claimed.extend(words[run.clone()].iter().cloned()),
            }
            index = run.end;
            continue;
        };
        match scan.spelling(&number, &run) {
            Spelling::Written { written, joins } => {
                let span = words[index].span.start..words[written.end - 1].span.end;
                claimed.push(Word {
                    joined: joins.then(|| Cow::Owned(written.text.clone())),
                    ..Word::claimed(span, Cow::Owned(written.text))
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
            Spelling::Ordinal { end } => {
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
    /// As words, with the ordinal that ends at `end`: "twenty second".
    Ordinal { end: usize },
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

    /// The number words from `start` on that read on from each other, with
    /// a "point" or "and" they end on: more speech may still read on from
    /// those. They may not parse as one number, as "one two three" doesn't.
    fn said_from(&self, start: usize) -> Option<Range<usize>> {
        let after_symbol = self.after_symbol(start);
        let first = self.word(start)?;
        if !is_number_word(first) && !(after_symbol && matches!(first, "o" | "oh")) {
            return None;
        }
        let mut last = start;
        let mut reads_digits = after_symbol;
        while let Some(next) = self.following(last) {
            if !continues(next, self.word(last)?, reads_digits) {
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
        self.following(before).is_some()
            && self.word(before).is_some_and(|word| symbol(word).is_some())
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

    /// `run` read the way the words after "point" are, with its punctuation.
    fn digits(&self, run: &Range<usize>) -> Option<String> {
        let said: Vec<&str> = run.clone().filter_map(|index| self.word(index)).collect();
        let digits = grammar::digits(&said)?;
        Some(self.punctuated(run.start, &digits, run.end))
    }

    /// How `number`, said by `run`, types.
    fn spelling(&self, number: &Number, run: &Range<usize>) -> Spelling {
        if let Some(counted) = self.counted(number, run) {
            let text = self.punctuated(run.start, &counted.text, counted.end);
            return Spelling::Written {
                written: Written { text, ..counted },
                joins: false,
            };
        }
        if let Some(end) = self.ordinal_end(run) {
            return Spelling::Ordinal { end };
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

    use super::super::Vocabulary;

    fn written(heard: &str) -> String {
        Vocabulary::default()
            .correct_spoken(heard, "")
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
    #[case::digit_word_a_symbol_joins_to_a_word("user underscore one", "user_one")]
    #[case::digits_inside_a_longer_join("user underscore one dot two", "user_1.2")]
    #[case::word_after_joined_digits("two dot five underscore user", "2.5_user")]
    #[case::digits_after_a_symbol("thirty dot one zero zero", "30.100")]
    #[case::digit_words_after_a_symbol("three dot one four", "3.14")]
    #[case::zeros_around_a_symbol("zero dot zero one", "0.01")]
    #[case::digits_on_both_sides("one nine two dot one six eight", "192.168")]
    #[case::digits_after_a_version("version two dot one zero", "version 2.10")]
    #[case::oh_opening_digits_after_a_symbol("thirty dot o one", "30.01")]
    #[case::oh_inside_digits_after_a_symbol("three dot one oh one", "3.101")]
    #[case::digits_the_model_wrote_before_a_symbol("30 dot one zero zero", "30.100")]
    #[case::digit_word_beside_a_scale_word("two million dot five", "2 million dot five")]
    #[case::digit_word_beside_dollars("two dot five dollars", "two dot $5")]
    #[case::digit_word_beside_a_scale_alone("hundred dot five", "hundred.five")]
    #[case::number_before_a_measure("twenty kilograms", "20 kilograms")]
    #[case::number_after_a_hyphen("utf hyphen sixteen", "utf-16")]
    #[case::number_after_a_hyphen_with_nothing_before("hyphen nineteen", "hyphen 19")]
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
    #[case::measure("two kilograms")]
    #[case::time_with_a_meridiem("three thirty pm")]
    #[case::another_currency("five euros")]
    #[case::scale_word_first("a hundred people")]
    #[case::scale_word_first_before_more("a hundred and fifty")]
    #[case::run_with_no_one_form("fifty fifty")]
    #[case::and_between_digit_words("one and two")]
    #[case::digit_words_no_symbol_joins("count one two three")]
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
