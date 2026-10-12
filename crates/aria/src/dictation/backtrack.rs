use std::ops::Range;

use super::Corrector;

pub(crate) struct Backtrack;

impl Corrector for Backtrack {
    fn correct(&self, text: &str) -> Option<String> {
        let mut tokens = tokens(text);
        for index in 0..tokens.len() {
            let width = cue_width(&tokens[index..]);
            if width == 0 {
                continue;
            }
            let mut before = index;
            while before > 0 && separator(tokens[before - 1].text) {
                before -= 1;
            }
            let mut after = index + width;
            while after < tokens.len()
                && separator(tokens[after].text)
                && !tokens.get(after + 1).is_some_and(|next| {
                    (tokens[after].text == "-"
                        || tokens[after].start + tokens[after].text.len() == next.start)
                        && next.text.starts_with(|c: char| c.is_ascii_digit())
                })
            {
                after += 1;
            }
            for token in &mut tokens[before..index] {
                token.cue_separator = true;
            }
            for token in &mut tokens[index + width..after] {
                token.cue_separator = true;
            }
        }

        let mut protected = Vec::new();
        let mut unmatched_quote = false;
        let mut blocked = false;
        let mut clause_start = 0;
        let mut copied = 0;
        let mut output = String::new();
        for index in 0..=tokens.len() {
            if let Some(token) = tokens.get(index) {
                unmatched_quote |= matches!(token.text, "”" | "’")
                    && protected
                        .last()
                        .is_none_or(|closing| *closing != token.text);
                blocked |= protection(token.text, &mut protected);
                if !boundary(token.text) || token.cue_separator {
                    continue;
                }
            }
            if !blocked && let Some(removed) = correction(&tokens[clause_start..index]) {
                output.push_str(&text[copied..removed.start]);
                copied = removed.end;
            }
            clause_start = index + 1;
            blocked = unmatched_quote || !protected.is_empty();
        }
        if copied == 0 {
            return None;
        }
        output.push_str(&text[copied..]);
        Some(output)
    }
}

struct Token<'a> {
    text: &'a str,
    start: usize,
    cue_separator: bool,
}

fn tokens(text: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start = 0;
    for (index, character) in text.char_indices() {
        let end = index + character.len_utf8();
        let previous = text[..index].chars().next_back();
        let next = text[end..].chars().next();
        let internal = match character {
            '.' | '?' | '!' | ';' => next.is_some_and(|c| !c.is_whitespace()),
            '\'' | '’' => {
                previous.is_some_and(char::is_alphanumeric)
                    && next.is_some_and(char::is_alphanumeric)
            }
            ',' => next.is_some_and(|c| c.is_ascii_digit()),
            _ => false,
        };
        if !character.is_whitespace()
            && (internal
                || !matches!(
                    character,
                    '.' | ','
                        | '!'
                        | '?'
                        | ';'
                        | '—'
                        | '–'
                        | '"'
                        | '“'
                        | '”'
                        | '\''
                        | '‘'
                        | '’'
                        | '`'
                        | '('
                        | ')'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                ))
        {
            continue;
        }
        if start < index {
            tokens.push(Token {
                text: &text[start..index],
                start,
                cue_separator: false,
            });
        }
        if !character.is_whitespace()
            || matches!(character, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
        {
            tokens.push(Token {
                text: &text[index..end],
                start: index,
                cue_separator: false,
            });
        }
        start = end;
    }
    if start < text.len() {
        tokens.push(Token {
            text: &text[start..],
            start,
            cue_separator: false,
        });
    }
    tokens
}

fn cue_width(tokens: &[Token<'_>]) -> usize {
    let Some(first) = tokens.first() else {
        return 0;
    };
    if first.text.eq_ignore_ascii_case("actually") {
        return 1;
    }
    let Some(second) = tokens.get(1) else {
        return 0;
    };
    for (begin, end) in [("scratch", "that"), ("i", "mean"), ("no", "wait")] {
        if first.text.eq_ignore_ascii_case(begin) && second.text.eq_ignore_ascii_case(end) {
            return 2;
        }
    }
    0
}

fn separator(text: &str) -> bool {
    matches!(text, "," | "—" | "–" | "-")
}

fn boundary(text: &str) -> bool {
    matches!(
        text,
        "." | "," | "!" | "?" | ";" | "\n" | "\r" | "\u{85}" | "\u{2028}" | "\u{2029}"
    )
}

fn protection(text: &str, stack: &mut Vec<&'static str>) -> bool {
    if stack.last().is_some_and(|closing| *closing == text) {
        stack.pop();
        return true;
    }
    let closing = match text {
        "\"" => "\"",
        "'" => "'",
        "`" => "`",
        "“" => "”",
        "‘" => "’",
        "(" => ")",
        "[" => "]",
        "{" => "}",
        ")" | "]" | "}" | "”" | "’" => return true,
        _ => return false,
    };
    if !stack
        .last()
        .is_some_and(|closing| matches!(*closing, "\"" | "'" | "`" | "”" | "’"))
    {
        stack.push(closing);
    }
    true
}

fn correction(tokens: &[Token<'_>]) -> Option<Range<usize>> {
    let mut cues = tokens.iter().enumerate().filter_map(|(index, _)| {
        let width = cue_width(&tokens[index..]);
        (width > 0).then_some((index, width))
    });
    let (cue, width) = cues.next()?;
    if cues.next().is_some() {
        return None;
    }
    let mut before_end = cue;
    while before_end > 0 && tokens[before_end - 1].cue_separator {
        before_end -= 1;
    }
    let mut after_start = cue + width;
    while after_start < tokens.len() && tokens[after_start].cue_separator {
        after_start += 1;
    }
    let before = &tokens[..before_end];
    let after = &tokens[after_start..];
    let old = before.last()?;
    let numeric = |word: &str| !word.is_empty() && word.bytes().all(|c| c.is_ascii_digit());
    // Only adjacent integers and repeated prepositions are corrected. Anything
    // broader needs semantic cleanup.
    let replacement = match after {
        [new] if numeric(old.text) && numeric(new.text) => {
            if before
                .iter()
                .rev()
                .nth(1)
                .is_some_and(|token| separator(token.text) || numeric(token.text))
            {
                return None;
            }
            new
        }
        [preposition, new] => {
            let anchor = before.iter().rev().nth(1)?;
            if !["to", "at", "on", "in", "for", "from", "with"]
                .iter()
                .any(|word| anchor.text.eq_ignore_ascii_case(word))
                || !anchor.text.eq_ignore_ascii_case(preposition.text)
                || !old.text.chars().all(char::is_alphabetic)
                || !new.text.chars().all(char::is_alphabetic)
            {
                return None;
            }
            new
        }
        _ => return None,
    };
    Some(old.start..replacement.start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_replace_integers_and_repeated_prepositions() {
        for cue in ["actually", "scratch that", "I mean", "no wait"] {
            for (heard, expected) in [
                (format!("meet me at 3, {cue} 4"), "meet me at 4"),
                (format!("meet me at 3 {cue} 4."), "meet me at 4."),
                (
                    format!("send it to Ali — {cue} — to Sara"),
                    "send it to Sara",
                ),
                (format!("send it to Ali–{cue}–to Sara!"), "send it to Sara!"),
                (
                    format!("Send it TO Ali, {} TO Sára.", cue.to_uppercase()),
                    "Send it TO Sára.",
                ),
            ] {
                assert_eq!(
                    Backtrack.correct(&heard).as_deref(),
                    Some(expected),
                    "{heard}"
                );
                assert_eq!(Backtrack.correct(expected), None, "{expected}");
            }
        }
        for preposition in ["to", "at", "on", "in", "for", "from", "with"] {
            let heard = format!("send it {preposition} Ali, actually {preposition} Sara");
            assert_eq!(
                Backtrack.correct(&heard),
                Some(format!("send it {preposition} Sara"))
            );
        }
    }

    #[test]
    fn splices_preserve_surrounding_text_and_punctuation() {
        for (heard, expected) in [
            (
                "Leave it here; meet me at 3, actually 4.",
                "Leave it here; meet me at 4.",
            ),
            (
                "At 3, actually 4. To Ali, no wait to Sara!",
                "At 4. To Sara!",
            ),
            (
                "  meet\tme at 3, actually 4!\nGo home.",
                "  meet\tme at 4!\nGo home.",
            ),
            (
                "meet at 3, actually, 4, then leave",
                "meet at 4, then leave",
            ),
            ("meet at 3—actually—4", "meet at 3—actually—4"),
            ("meet at 3—actually 4", "meet at 4"),
            ("meet at 3,actually 4", "meet at 4"),
            ("at 3 - no wait 4?", "at 4?"),
            ("to Али — scratch that — to Сара.", "to Сара."),
            ("to علی، actually to سارا", "to علی، actually to سارا"),
            ("meet at 0003, actually 0004", "meet at 0004"),
        ] {
            let corrected = Backtrack.correct(heard);
            assert_eq!(corrected.as_deref().unwrap_or(heard), expected, "{heard}");
        }
    }

    #[test]
    fn ambiguous_or_unsupported_speech_is_unchanged() {
        for heard in [
            "",
            "I mean it",
            "actually, yes",
            "I mean 4",
            "actually 4",
            "scratch that",
            "meet at 3 actually",
            "meet at 3 scratch that",
            "meet at 3 no wait",
            "at 3. Actually 4.",
            "at 3; actually 4",
            "at 3\nactually 4",
            "at 3, actually 4 tomorrow",
            "to Ali actually Sara",
            "to Ali actually from Sara",
            "to Ali Reza actually to Sara",
            "to Ali actually to Sara Khan",
            "at three actually four",
            "at 3 actually four",
            "at three actually 4",
            "version 1.3, actually 1.4",
            "at 3.5 actually 4",
            "at 3 actually 4.5",
            "at .3 actually 4",
            "at 3 actually .4",
            "at 3 actually 4.\u{200b}5",
            "at 3:30 actually 4:30",
            "at 3 actually 4:30",
            "at 3,000 actually 4,000",
            "at -3 actually 4",
            "at +3 actually 4",
            "at 3 actually -4",
            "at 3 actually - 4",
            "at –3 actually 4",
            "at 3 actually –4",
            "at 3 actually —4",
            "at 3% actually 4",
            "at $3 actually 4",
            "at 3 actually $4",
            "at v3 actually v4",
            "at item_3 actually 4",
            "at 3/4 actually 5",
            "to Ali-Reza actually to Sara",
            "to Ali—Reza actually to Sara",
            "to Ali actually to Sara-Khan",
            "to Ali actually to Sara—Khan",
            "at 3 actually 4 no wait 5",
            "at 3 actually 4, no wait 5",
            "I mean it, at 3 actually 4 no wait 5",
            "Actually, meet at 3 actually 4",
            "at 3 notactually 4",
            "at 3 I-mean 4",
            "at 3 no\nwait 4",
            "write \"at 3 actually 4\"",
            "write 'at 3 actually 4'",
            "write `at 3 actually 4`",
            "write “at 3 actually 4”",
            "write ‘at 3 actually 4’",
            "(at 3 actually 4)",
            "[at 3 actually 4]",
            "{at 3 actually 4}",
            "\"hello. meet at 3 actually 4.\"",
            "\"hello. meet at 3 actually 4",
            "`code; at 3 actually 4",
            "(aside [aside]. at 3 actually 4)",
            "to Ali, I mean it",
            "go to https://example.com actually to Sara",
            "send https://example.com?3 actually 4",
            "send https://example.com;3 actually 4",
            "send https://example.com!3 actually 4",
            "send https://example.com/a,3 actually 4",
            "at 3 actually 4?5",
            "to Ali actually to Sara?Khan",
            "at 1\u{202f}000 actually 2",
            "at 1\u{a0}000 actually 2",
            "at 1 000 actually 2",
            "at 3\u{2028}actually 4",
            "at 3\u{2029}actually 4",
            "at 3\u{85}actually 4",
            "”quoted. Meet at 3 actually 4.",
            "’quoted. Meet at 3 actually 4.",
        ] {
            assert_eq!(Backtrack.correct(heard), None, "{heard}");
        }
    }

    #[test]
    fn closing_quotes_restore_eligibility_only_in_the_next_clause() {
        for quoted in [
            "\"quoted. text\"",
            "'quoted. text'",
            "‘quoted. text’",
            "`quoted; text`",
            "(quoted [text]. text)",
        ] {
            let heard = format!("{quoted}. Meet at 3 actually 4.");
            assert_eq!(
                Backtrack.correct(&heard),
                Some(format!("{quoted}. Meet at 4."))
            );
            let heard = format!("{quoted} meet at 3 actually 4.");
            assert_eq!(Backtrack.correct(&heard), None);
        }
        assert_eq!(
            Backtrack.correct("I'm at 3, actually 4."),
            Some("I'm at 4.".to_owned())
        );
    }

    #[test]
    fn many_clauses_preserve_rejected_text() {
        let heard = "I mean it. Meet at 3 actually 4. At 3 actually 4 no wait 5. ".repeat(4096);
        let expected = "I mean it. Meet at 4. At 3 actually 4 no wait 5. ".repeat(4096);
        assert_eq!(Backtrack.correct(&heard), Some(expected));
    }
}
