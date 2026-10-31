//! Spacing between inserted text and the text already beside the caret.

/// The cursor in a text field, which adds the spaces text needs when typed
/// there.
///
/// Build it with [`Caret::between`] from the text on each side of the cursor,
/// then type what [`Caret::insert`] returns. Use [`Caret::default`] for a
/// field you can't read; text then goes in unchanged.
///
/// ```
/// use aria_kit::transcription::Caret;
///
/// // The field holds "Hello." and the cursor is at its end.
/// let mut caret = Caret::between("Hello.", "");
/// assert_eq!(caret.insert("How are you"), " How are you");
/// assert_eq!(caret.insert("today?"), " today?");
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Caret {
    before: Option<char>,
    /// The character before `before`, which tells an opening straight quote
    /// from a closing one.
    before_that: Option<char>,
    after: Option<char>,
}

impl Caret {
    /// A caret with `before` to its left and `after` to its right. Pass the
    /// whole field split at the cursor; only the characters next to it count.
    pub fn between(before: &str, after: &str) -> Self {
        let mut preceding = before.chars().rev();
        Self {
            before: preceding.next(),
            before_that: preceding.next(),
            after: after.chars().next(),
        }
    }

    /// Returns `text` with any spaces it needs, then moves the caret past it.
    /// To retry a failed typing, insert again from a copy taken before.
    #[must_use = "the caret has moved past text that must be typed"]
    pub fn insert(&mut self, text: &str) -> String {
        let trailing = self.needs_trailing_space(text);
        let mut spaced = self.insert_open(text);
        if trailing {
            spaced.push(' ');
            self.before_that = self.before;
            self.before = Some(' ');
        }
        spaced
    }

    /// Like [`Caret::insert`], but leaves out the space before the text
    /// after the caret, so the next insertion can attach to `text`'s end.
    /// A later [`Caret::insert`] supplies that space.
    #[must_use = "the caret has moved past text that must be typed"]
    pub fn insert_open(&mut self, text: &str) -> String {
        let mut spaced = String::with_capacity(text.len() + 2);
        if self.needs_leading_space(text) {
            spaced.push(' ');
        }
        spaced.push_str(text);
        let mut typed = spaced.chars().rev();
        if let Some(last) = typed.next() {
            self.before_that = typed.next().or(self.before);
            self.before = Some(last);
        }
        spaced
    }

    fn needs_leading_space(&self, text: &str) -> bool {
        let Some(previous) = self.before else {
            return false;
        };
        let Some(first) = text.chars().next() else {
            return false;
        };
        !previous.is_whitespace()
            && !self.opens_after(previous)
            && !first.is_whitespace()
            && !attaches_before(text)
            && !unspaced_script(previous)
            && !unspaced_script(first)
    }

    fn needs_trailing_space(&self, text: &str) -> bool {
        let Some(next) = self.after else {
            return false;
        };
        let Some(last) = text.chars().last() else {
            return false;
        };
        next.is_alphanumeric()
            && !unspaced_script(next)
            && !last.is_whitespace()
            && !OPENERS.contains(&last)
            && !unspaced_script(last)
    }

    /// A straight quote opens when nothing or whitespace comes before it.
    fn opens_after(&self, previous: char) -> bool {
        if OPENERS.contains(&previous) {
            return true;
        }
        if !matches!(previous, '"' | '\'') {
            return false;
        }
        self.before_that.is_none_or(char::is_whitespace)
    }
}

const OPENERS: [char; 9] = ['(', '[', '{', '/', '-', '@', '#', '“', '‘'];

/// Contraction endings an apostrophe joins to the word before, as in `it's`.
const CONTRACTION_TAILS: [&str; 7] = ["s", "t", "d", "m", "re", "ll", "ve"];

/// Whether `text` begins with something that attaches to the text before it:
/// closing punctuation, or an apostrophe that starts a contraction ending.
/// An apostrophe before anything else opens a word, as in `'90s` or `'cause`.
fn attaches_before(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some('.' | ',' | '!' | '?' | ':' | ';' | ')' | ']' | '}' | '%' | '”') => true,
        Some('\'' | '’') => {
            let rest = chars.as_str();
            CONTRACTION_TAILS.iter().any(|tail| {
                rest.strip_prefix(tail)
                    .is_some_and(|after| !after.starts_with(char::is_alphanumeric))
            })
        }
        _ => false,
    }
}

// Hand-listed blocks for scripts written without spaces. Rarer ones, such as
// Tibetan or Balinese, still get a space.
fn unspaced_script(character: char) -> bool {
    matches!(
        character,
        '\u{0E00}'..='\u{0EFF}'       // Thai, Lao
        | '\u{1000}'..='\u{109F}'     // Myanmar
        | '\u{1780}'..='\u{17FF}'     // Khmer
        | '\u{3000}'..='\u{30FF}'     // CJK punctuation, Hiragana, Katakana
        | '\u{3400}'..='\u{4DBF}'     // CJK Extension A
        | '\u{4E00}'..='\u{9FFF}'     // CJK Unified Ideographs
        | '\u{F900}'..='\u{FAFF}'     // CJK Compatibility Ideographs
        | '\u{FF00}'..='\u{FF9F}'     // fullwidth forms, halfwidth Katakana
        | '\u{20000}'..='\u{2FA1F}' // CJK Extensions B and later
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spaced(before: &str, text: &str, after: &str) -> String {
        Caret::between(before, after).insert(text)
    }

    #[test]
    fn a_word_after_text_gets_one_space() {
        assert_eq!(spaced("Hello.", "How are you", ""), " How are you");
        assert_eq!(spaced("and", "then", ""), " then");
        assert_eq!(spaced("🙂", "then", ""), " then");
    }

    #[test]
    fn unknown_or_empty_fields_get_no_space() {
        assert_eq!(spaced("", "Hello", ""), "Hello");
        assert_eq!(Caret::default().insert("Hello"), "Hello");
    }

    #[test]
    fn whitespace_and_openers_take_no_space_after_them() {
        for before in [
            "Hello ", "line\n", "(", "[", "{", "a/", "re-", "@", "#", "“",
        ] {
            assert_eq!(spaced(before, "word", ""), "word", "after {before:?}");
        }
    }

    #[test]
    fn punctuation_attaches_to_the_text_before_it() {
        for text in [
            ".", ", right?", "!", "?", ":", ";", ")", "%", "”", "'s", " already",
        ] {
            assert_eq!(spaced("word", text, ""), text, "for {text:?}");
        }
    }

    #[test]
    fn an_apostrophe_attaches_only_a_contraction_ending() {
        assert_eq!(spaced("it", "'s", ""), "'s");
        assert_eq!(spaced("they", "’re here", ""), "’re here");
        assert_eq!(spaced("the", "'90s", ""), " '90s");
        assert_eq!(spaced("said", "'cause", ""), " 'cause");
        assert_eq!(spaced("said", "'til then", ""), " 'til then");
    }

    #[test]
    fn a_straight_quote_opens_only_after_whitespace_or_the_start() {
        assert_eq!(spaced("\"", "hello", ""), "hello");
        assert_eq!(spaced(" \"", "hello", ""), "hello");
        assert_eq!(spaced("i\"", "then", ""), " then");
        assert_eq!(spaced("n'", "then", ""), " then");
    }

    #[test]
    fn scripts_without_spaces_join_directly() {
        assert_eq!(spaced("你好", "世界", ""), "世界");
        assert_eq!(spaced("Hello", "世界", ""), "世界");
        assert_eq!(spaced("こんにちは", "Aria", ""), "Aria");
        assert_eq!(spaced("สวัสดี", "ครับ", ""), "ครับ");
        assert_eq!(spaced("你好", "world", "世"), "world");
    }

    #[test]
    fn a_word_after_the_caret_gets_a_trailing_space() {
        assert_eq!(spaced("", "Hello", "w"), "Hello ");
        assert_eq!(spaced("say", "hello", "7"), " hello ");
        assert_eq!(spaced("plan ", "number", "B is ready"), "number ");
        assert_eq!(spaced("", "Hello", " "), "Hello");
        assert_eq!(spaced("", "Hello", "."), "Hello");
        assert_eq!(spaced("", "(", "w"), "(");
    }

    #[test]
    fn an_open_insert_lets_the_next_text_attach_before_the_caret() {
        let mut caret = Caret::between("foo", "bar");

        let typed = [
            caret.insert_open("we should"),
            caret.insert_open(". Ship it"),
            caret.insert("."),
        ];

        assert_eq!(typed, [" we should", ". Ship it", ". "]);
    }

    #[test]
    fn insert_moves_the_caret_past_the_text() {
        let mut caret = Caret::between("", "w");
        assert_eq!(caret.insert("hello"), "hello ");
        assert_eq!(caret, Caret::between("o ", "w"));
        assert_eq!(caret.insert("there"), "there ");

        let mut sentence = Caret::between("He said", "");
        assert_eq!(sentence.insert("."), ".");
        assert_eq!(sentence, Caret::between("d.", ""));

        let mut unknown = Caret::default();
        assert_eq!(unknown.insert("I"), "I");
        assert_eq!(unknown.insert("agree"), " agree");
    }
}
