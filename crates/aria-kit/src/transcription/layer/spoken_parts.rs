use super::super::sentences::ends_sentence;
use super::{number_words, symbol_words};

/// A written word cut into the parts a speaker says: `QA2-step` is "qa",
/// "2", "-", "step". A number word up to twenty is its digits and a
/// symbol's name is its symbol, so "one" is `1` and "dash" is `-`.
///
/// Ordinary words never split: "cannot" is not "can" and "not". A word
/// with a symbol that has no spoken name, as `C++17`, has no parts.
pub(in crate::transcription) struct SpokenParts {
    parts: Vec<Part>,
    word: String,
    ends_sentence: bool,
}

struct Part {
    /// In lowercase.
    text: String,
    /// Letters all in capitals, as a recognizer writes letters said one
    /// by one.
    capitals: bool,
}

impl Part {
    fn is_one_capital(&self) -> bool {
        self.capitals && self.text.chars().nth(1).is_none()
    }

    fn is_symbol(&self, symbol: char) -> bool {
        self.text.chars().eq([symbol])
    }

    /// The part a run of letters or digits says: "one" → `1`, "dash" →
    /// `-`.
    fn named(run: &str) -> Option<Self> {
        if run.is_empty() {
            return None;
        }
        // Letter by letter, so a final sigma matches a sigma said alone.
        let text: String = run.chars().flat_map(char::to_lowercase).collect();
        let digits = number_words::value_up_to_twenty(&text);
        let renamed = match (digits, symbol_words::named_by(&text)) {
            (Some(value), _) => Some(value.to_string()),
            (None, symbol) => symbol.map(String::from),
        };
        Some(Self {
            capitals: renamed.is_none() && run.chars().all(char::is_uppercase),
            text: renamed.unwrap_or(text),
        })
    }
}

impl Part {
    /// The parts `said` is said in. None when it holds a character that is
    /// neither a letter, a digit, nor a spoken symbol.
    fn split(said: &str) -> Vec<Self> {
        let mut parts = Vec::new();
        let mut run = String::new();
        for character in said.chars() {
            let symbol = symbol_words::is_symbol(character);
            if !symbol && !character.is_alphanumeric() {
                return Vec::new();
            }
            let other_kind = run
                .chars()
                .next_back()
                .is_some_and(|last| last.is_numeric() != character.is_numeric());
            if symbol || other_kind {
                parts.extend(Self::named(&std::mem::take(&mut run)));
            }
            if symbol {
                parts.push(Self {
                    text: character.to_string(),
                    capitals: false,
                });
            } else {
                run.push(character);
            }
        }
        parts.extend(Self::named(&run));
        parts
    }
}

impl SpokenParts {
    pub(in crate::transcription) fn of(word: &str) -> Self {
        // A closing dot ends a sentence far more often than it is said.
        let said = word
            .trim_start_matches(['"', '\'', '(', '[', '{'])
            .trim_end_matches(['.', ',', ';', ':', '!', '?', '"', '\'', ')', ']', '}']);
        Self {
            parts: Part::split(said),
            word: word.to_owned(),
            ends_sentence: ends_sentence(word),
        }
    }

    /// How many of `words`, from the first, say this word: three for
    /// `2-step` before "two", "dash", "step". `None` when none do, or when
    /// a sentence ends inside the run.
    ///
    /// Capitals said one by one spell only a part in capitals: "T L D R"
    /// says `TLDR`, but "F A R" never says "far".
    pub(in crate::transcription) fn said_in(&self, words: &[Self]) -> Option<usize> {
        let mut unsaid = Unsaid {
            parts: self.parts.iter(),
            spelling: "",
        };
        let mut count = 0;
        while count < words.len() {
            if count > 0 && words[count - 1].ends_sentence {
                return None;
            }
            count += match unsaid.symbol_name(&words[count..]) {
                Some(name) => {
                    unsaid.say_symbols(name.typed)?;
                    name.word_count
                }
                None => {
                    unsaid.say_word(&words[count])?;
                    1
                }
            };
            if unsaid.is_done() {
                return Some(count);
            }
        }
        None
    }
}

/// The parts of a word that the words so far have not said.
struct Unsaid<'a> {
    parts: std::slice::Iter<'a, Part>,
    /// What is left of a part in capitals that letters are spelling.
    spelling: &'a str,
}

impl<'a> Unsaid<'a> {
    fn is_done(&self) -> bool {
        self.spelling.is_empty() && self.parts.as_slice().is_empty()
    }

    /// The name `words` open with for the symbols said next, when it takes
    /// several words, as "forward slash" does.
    fn symbol_name(&self, words: &[SpokenParts]) -> Option<symbol_words::SymbolName> {
        let symbols_next = self.spelling.is_empty()
            && self
                .parts
                .as_slice()
                .first()
                .is_some_and(|next| next.text.chars().all(symbol_words::is_symbol));
        if !symbols_next {
            return None;
        }
        symbol_words::leading_name(words.iter().map(|word| word.word.as_str()))
            .filter(|name| name.word_count > 1)
    }

    fn say_symbols(&mut self, typed: &str) -> Option<()> {
        for symbol in typed.chars() {
            self.parts.next().filter(|part| part.is_symbol(symbol))?;
        }
        Some(())
    }

    fn say_word(&mut self, word: &SpokenParts) -> Option<()> {
        if word.parts.is_empty() {
            return None;
        }
        word.parts.iter().try_for_each(|part| self.say(part))
    }

    fn say(&mut self, part: &Part) -> Option<()> {
        self.spelling = if !self.spelling.is_empty() {
            self.spelling
                .strip_prefix(part.text.as_str())
                .filter(|_| part.is_one_capital())?
        } else {
            let whole: &'a Part = self.parts.next()?;
            if whole.text == part.text {
                ""
            } else {
                whole
                    .text
                    .strip_prefix(part.text.as_str())
                    .filter(|_| whole.capitals && part.is_one_capital())?
            }
        };
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::SpokenParts;

    fn parts(words: &[&str]) -> Vec<SpokenParts> {
        words.iter().map(|word| SpokenParts::of(word)).collect()
    }

    #[rstest]
    #[case::a_named_symbol_and_digit("2-step.", &["two", "dash", "step"])]
    #[case::a_symbol_by_its_other_name("2-step", &["2", "hyphen", "step."])]
    #[case::letters_one_by_one("TLDR", &["T", "L", "D", "R"])]
    #[case::letters_before_a_symbol("QA2-step", &["Q", "A", "two", "dash", "step"])]
    #[case::letters_opening_a_longer_word("QA2-step", &["Q", "A2-step"])]
    #[case::a_digit_inside("i18n", &["i", "eighteen", "n"])]
    #[case::a_name_of_two_words("src/main", &["src", "forward", "slash", "main"])]
    #[case::marks_around_it("(2-step),", &["two", "dash", "step"])]
    #[case::a_final_sigma("ΑΣ", &["Α", "Σ"])]
    fn a_word_is_said_in_the_words_that_spell_it_out(#[case] word: &str, #[case] run: &[&str]) {
        let followed: Vec<&str> = run.iter().copied().chain(["next"]).collect();

        assert_eq!(
            SpokenParts::of(word).said_in(&parts(&followed)),
            Some(run.len())
        );
    }

    #[rstest]
    #[case::a_symbol_the_word_lacks("src", &["src", "forward", "slash"])]
    #[case::a_closing_dot_named("end.", &["end.", "Dot"])]
    #[case::a_digit_named("1", &["one", "more"])]
    fn a_word_said_whole_takes_no_word_after_it(#[case] word: &str, #[case] run: &[&str]) {
        assert_eq!(SpokenParts::of(word).said_in(&parts(run)), Some(1));
    }

    #[rstest]
    #[case::words_run_together("cannot", &["can", "not"])]
    #[case::a_symbol_never_said("e-mail", &["e", "mail"])]
    #[case::a_sentence_end_inside("B2", &["B.", "Two"])]
    #[case::a_symbol_with_no_name("C++17", &["C", "seventeen"])]
    #[case::another_symbol_with_no_name("C17", &["C#", "seventeen"])]
    #[case::letters_in_pairs("TLDR", &["TL", "DR"])]
    #[case::the_letters_of_a_plain_word("far", &["F", "A", "R"])]
    #[case::small_letters("TLDR", &["t", "l", "d", "r"])]
    #[case::letters_of_a_word_in_small_letters("qa2-step", &["Q", "A", "two", "dash", "step"])]
    #[case::too_few_words("2-step", &["two", "dash"])]
    #[case::a_word_of_marks("TLDR", &["T", "...", "L", "D", "R"])]
    fn a_word_is_not_said_in_other_words(#[case] word: &str, #[case] run: &[&str]) {
        assert_eq!(SpokenParts::of(word).said_in(&parts(run)), None);
    }
}
