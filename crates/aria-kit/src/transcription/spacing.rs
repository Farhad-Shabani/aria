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
    /// The last insert left out its space before `after`, which
    /// [`Caret::close`] types.
    open: bool,
}

/// How inserted text meets the text around the caret.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Spacing {
    pub attachment: Attachment,
    pub ending: Ending,
}

/// Whether a space follows inserted text before a word after the caret.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// Before `notes`, `open src` → `open src `.
    #[default]
    Closed,
    /// Before `notes`, `open src` → `open src`, so the next text can join
    /// it. The next insert or [`Caret::close`] types the space.
    Open,
    /// Like [`Self::Open`]. The next text starts with this one's held-back
    /// mark, as `. Then rest`.
    MarkDeferred,
}

/// Whether a space comes before inserted text.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Attachment {
    /// After `Hello`, `world` → ` world` but `,` → `,`.
    #[default]
    AsWritten,
    /// No space. After `src`, `/main` → `/main`.
    Joined,
    /// Spaced like a word, even with a symbol first. After `cd`, `../src`
    /// → ` ../src`.
    Apart,
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
            open: false,
        }
    }

    /// Returns `text` with any spaces it needs, then moves the caret past it.
    /// To retry a failed typing, insert again from a copy taken before.
    #[must_use = "the caret has moved past text that must be typed"]
    pub fn insert(&mut self, text: &str) -> String {
        self.insert_with(text, Spacing::default())
    }

    /// Like [`Caret::insert`], but leaves `text`'s end open so the next
    /// insertion can attach to it. A later insert supplies the space.
    #[must_use = "the caret has moved past text that must be typed"]
    pub fn insert_open(&mut self, text: &str) -> String {
        let spacing = Spacing {
            ending: Ending::Open,
            ..Spacing::default()
        };
        self.insert_with(text, spacing)
    }

    /// Like [`Caret::insert`], with the spaces `spacing` leaves out.
    #[must_use = "the caret has moved past text that must be typed"]
    pub fn insert_with(&mut self, text: &str, spacing: Spacing) -> String {
        let leading = self.needs_leading_space(text, spacing.attachment);
        let trailing = spacing.ending == Ending::Closed
            && text
                .chars()
                .last()
                .is_some_and(|last| self.needs_space_after(last));
        let mut spaced = String::with_capacity(text.len() + 2);
        if leading {
            spaced.push(' ');
        }
        spaced.push_str(text);
        let mut typed = spaced.chars().rev();
        if let Some(last) = typed.next() {
            self.before_that = typed.next().or(self.before);
            self.before = Some(last);
            self.open = spacing.ending != Ending::Closed;
        }
        if trailing {
            spaced.push(' ');
            self.before_that = self.before;
            self.before = Some(' ');
        }
        spaced
    }

    /// The space an open insert left out before the word after the caret,
    /// once: `open src` before `notes` gets ` `. Empty when none is due.
    #[must_use = "the caret has moved past a space that must be typed"]
    pub fn close(&mut self) -> String {
        let owed = self.open && self.before.is_some_and(|last| self.needs_space_after(last));
        self.open = false;
        if !owed {
            return String::new();
        }
        self.before_that = self.before;
        self.before = Some(' ');
        " ".to_owned()
    }

    fn needs_leading_space(&self, text: &str, attachment: Attachment) -> bool {
        if attachment == Attachment::Joined {
            return false;
        }
        let Some(previous) = self.before else {
            return false;
        };
        let Some(first) = text.chars().next() else {
            return false;
        };
        !previous.is_whitespace()
            && !self.opens_after(previous)
            && !first.is_whitespace()
            && (attachment == Attachment::Apart || !attaches_before(text))
            && !unspaced_script(previous)
            && !unspaced_script(first)
    }

    fn needs_space_after(&self, last: char) -> bool {
        let Some(next) = self.after else {
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
    fn text_that_opens_apart_takes_a_space_before_its_symbol() {
        let apart = Spacing {
            attachment: Attachment::Apart,
            ..Spacing::default()
        };
        assert_eq!(Caret::between("cd", "").insert("../src"), "../src");
        assert_eq!(
            Caret::between("cd", "").insert_with("../src", apart),
            " ../src"
        );
        assert_eq!(Caret::between("", "").insert_with(".env", apart), ".env");
        assert_eq!(Caret::between("(", "").insert_with(".env", apart), ".env");
    }

    #[test]
    fn only_joined_text_attaches_to_the_word_before() {
        let joins = Spacing {
            attachment: Attachment::Joined,
            ..Spacing::default()
        };
        let joins_open = Spacing {
            ending: Ending::Open,
            ..joins
        };
        assert_eq!(
            Caret::between("src", "").insert_with("/main", joins),
            "/main"
        );
        assert_eq!(Caret::between("user", "").insert_with("_id", joins), "_id");
        assert_eq!(
            Caret::between("src", "x").insert_with("/main", joins),
            "/main "
        );
        assert_eq!(
            Caret::between("src", "x").insert_with("/main", joins_open),
            "/main"
        );
        assert_eq!(Caret::between("run", "").insert("/compact"), " /compact");
        assert_eq!(Caret::between("user", "").insert("_id"), " _id");
        assert_eq!(Caret::between("aria", "").insert("-kit"), " -kit");
        assert_eq!(Caret::between("is", "").insert("-5"), " -5");
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
    fn closing_types_the_space_an_open_insert_left_out_once() {
        let mut caret = Caret::between("", "notes");
        assert_eq!(caret.insert_open("open src"), "open src");
        assert_eq!(caret.close(), " ");
        assert_eq!(caret.close(), "");

        let mut closed = Caret::between("", "notes");
        assert_eq!(closed.insert("open src"), "open src ");
        assert_eq!(closed.close(), "");

        let mut at_the_end = Caret::between("", "");
        assert_eq!(at_the_end.insert_open("open src"), "open src");
        assert_eq!(at_the_end.close(), "");

        assert_eq!(Caret::between("key", "board").close(), "");
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
