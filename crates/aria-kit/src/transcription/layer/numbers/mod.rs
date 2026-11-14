mod grammar;
mod unit;

use std::borrow::Cow;

use self::grammar::Number;
use self::unit::Unit;
use super::number_words::{continues, is_number_word, is_ordinal};
use super::symbol_words::symbol;
use super::vocabulary::Word;

/// Claims each run of number words that has one clear written form, as in
/// "two point five" → `2.5`. A run with none stays words, whole.
pub(super) fn claim<'a>(text: &str, words: Vec<Word<'a>>) -> Vec<Word<'a>> {
    let scan = Scan {
        text,
        words: &words,
        parts: words
            .iter()
            .map(|word| Parts::of(&text[word.span.clone()]))
            .collect(),
    };
    let mut claimed = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let Some(end) = scan.run_end(index) else {
            claimed.push(words[index].clone());
            index += 1;
            continue;
        };
        let Some((last, typed)) = scan.written(index, end) else {
            claimed.extend_from_slice(&words[index..=end]);
            index = end + 1;
            continue;
        };
        claimed.push(Word {
            span: words[index].span.start..words[last].span.end,
            claim: Some(Cow::Owned(typed)),
        });
        index = last + 1;
    }
    claimed
}

/// How many of the words `words` ends on more speech may still change: a
/// number that may go on, or dollars that cents may follow.
pub(super) fn open_tail(words: &[&str]) -> usize {
    let mut open = 0;
    for index in (0..words.len()).rev() {
        let word = Parts::of(words[index]);
        let follows = index.checked_sub(1).is_some_and(|previous| {
            let previous = Parts::of(words[previous]);
            previous.trail.is_empty()
                && word.lead.is_empty()
                && may_follow(&word.core, &previous.core)
        });
        if !word.trail.is_empty() || !(follows || is_number_word(&word.core)) {
            break;
        }
        open += 1;
        if !follows {
            break;
        }
    }
    // Only a number word opens anything: "the dollars and" is closed.
    let opens_on_a_number =
        |open: usize| is_number_word(&Parts::of(words[words.len() - open]).core);
    while open > 0 && !opens_on_a_number(open) {
        open -= 1;
    }
    open
}

/// Whether `word` may belong to what the number before it writes: more of
/// the number, its dollars, or the cents after them. Everything [`claim`]
/// joins is here, so a stream never types half of it.
fn may_follow(word: &str, previous: &str) -> bool {
    continues(word, previous)
        || (unit::names_dollars(word) && is_number_word(previous))
        || (word == "and" && unit::names_dollars(previous))
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

/// A segment's words, read for the numbers in them.
struct Scan<'t, 'w, 'a> {
    text: &'t str,
    words: &'w [Word<'a>],
    parts: Vec<Parts<'t>>,
}

impl Scan<'_, '_, '_> {
    /// The word after `index`, when it reads on from it: unclaimed, with
    /// only spaces between and no punctuation on either side.
    fn following(&self, index: usize) -> Option<&str> {
        let next = self.words.get(index + 1)?;
        let between = &self.text[self.words[index].span.end..next.span.start];
        let reads_on = next.claim.is_none()
            && self.parts[index].trail.is_empty()
            && self.parts[index + 1].lead.is_empty()
            && !between.is_empty()
            && !between.contains(|c: char| c == '\n' || !c.is_whitespace());
        reads_on.then_some(self.parts[index + 1].core.as_str())
    }

    /// The last word of the number run that opens at `start`. A "point" or
    /// "and" the run would end on stays outside it.
    fn run_end(&self, start: usize) -> Option<usize> {
        if self.words[start].claim.is_some() || !is_number_word(&self.parts[start].core) {
            return None;
        }
        let mut end = start;
        let mut after_point = false;
        while let Some(next) = self.following(end) {
            // "o" is a zero only after the point: "ten oh my" ends at "ten".
            let stray_o = matches!(next, "o" | "oh") && !after_point;
            if stray_o || !continues(next, &self.parts[end].core) {
                break;
            }
            after_point |= next == "point";
            end += 1;
        }
        while matches!(self.parts[end].core.as_str(), "point" | "and") {
            end -= 1;
        }
        Some(end)
    }

    fn number(&self, start: usize, end: usize) -> Option<Number> {
        let said: Vec<&str> = self.parts[start..=end]
            .iter()
            .map(|parts| parts.core.as_str())
            .collect();
        Number::parse(&said)
    }

    /// How the run from `start` through `end` is written, and the last
    /// word that went into it. `None` leaves the run as words.
    fn written(&self, start: usize, end: usize) -> Option<(usize, String)> {
        let number = self.number(start, end)?;
        let (last, written) = match self.counted(&number, end) {
            Some(counted) => counted,
            None if self.stays_words(&number, start, end) => return None,
            None => (end, number.to_string()),
        };
        let (lead, trail) = (self.parts[start].lead, self.parts[last].trail);
        Some((last, format!("{lead}{written}{trail}")))
    }

    /// Whether a number with no unit keeps its words.
    fn stays_words(&self, number: &Number, start: usize, end: usize) -> bool {
        // "twenty second" is an ordinal, not 20 and a word.
        let opens_an_ordinal = self.following(end).is_some_and(is_ordinal);
        // "one" or "nine" alone reads as a word in prose: "no one knows".
        let lone_digit = start == end && number.integer().is_some_and(|digit| digit < 10);
        opens_an_ordinal || (lone_digit && !self.symbol_joins_a_number(start))
    }

    /// Whether a spoken symbol joins the word at `index` to another number
    /// word on either side, as in "two dot five".
    fn symbol_joins_a_number(&self, index: usize) -> bool {
        let joins_the_next = |left: usize| {
            self.words[left].claim.is_none()
                && is_number_word(&self.parts[left].core)
                && self
                    .following(left)
                    .is_some_and(|word| symbol(word).is_some())
                && self.following(left + 1).is_some_and(is_number_word)
        };
        joins_the_next(index) || index.checked_sub(2).is_some_and(joins_the_next)
    }

    /// `number` with the unit said after `end`, and the last word of it.
    fn counted(&self, number: &Number, end: usize) -> Option<(usize, String)> {
        let unit = Unit::parse(self.following(end)?, number)?;
        let unit_at = end + 1;
        let with_cents = match unit {
            Unit::Dollar => self.with_cents(number, unit_at),
            Unit::Cent | Unit::Percent => None,
        };
        with_cents.or_else(|| Some((unit_at, unit.write(number)?)))
    }

    /// `dollars` and the cents said after "dollars" at `dollars_at`, past
    /// an "and", as in `$5.20`. Comes with where "cents" sits.
    fn with_cents(&self, dollars: &Number, dollars_at: usize) -> Option<(usize, String)> {
        let after_and = match self.following(dollars_at)? {
            "and" => dollars_at + 1,
            _ => dollars_at,
        };
        let start = self.following(after_and).map(|_| after_and + 1)?;
        let end = self.run_end(start)?;
        let cents = self.number(start, end)?;
        let unit = Unit::parse(self.following(end)?, &cents)?;
        let written = unit::dollars_and_cents(dollars, &cents).filter(|_| unit == Unit::Cent)?;
        Some((end + 1, written))
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
    #[case::dollars_before_other_words("five dollars and twenty people", "$5 and 20 people")]
    // "x minus ten" may be a sum, so "minus" never signs a number.
    #[case::minus_stays_a_word("x minus ten", "x minus 10")]
    fn writes(#[case] heard: &str, #[case] typed: &str) {
        assert_eq!(written(heard), typed);
    }

    #[rstest]
    #[case::digit_word_alone("one of the best")]
    #[case::pronoun("no one knows")]
    #[case::idiom("on the one hand")]
    #[case::zero_alone("zero")]
    #[case::ordinal_word("give me a second")]
    #[case::first("first of all")]
    #[case::range_of_digit_words("it is a nine to five job")]
    #[case::compound_ordinal("the twenty second of May")]
    #[case::scale_word_first("a hundred people")]
    #[case::scale_word_first_before_more("a hundred and fifty")]
    #[case::digit_string("one two three")]
    #[case::run_with_no_one_form("fifty fifty")]
    #[case::and_between_digit_words("one and two")]
    #[case::point_without_a_number_before("point five")]
    #[case::time_without_a_reading("three thirty")]
    #[case::plural_after_one("one dollars")]
    #[case::comma_between("one, two")]
    #[case::line_break_between("one\ntwo")]
    #[case::digits_the_model_wrote("2 point 5")]
    #[case::empty("")]
    fn leaves_unchanged(#[case] heard: &str) {
        assert_eq!(written(heard), heard);
    }
}
