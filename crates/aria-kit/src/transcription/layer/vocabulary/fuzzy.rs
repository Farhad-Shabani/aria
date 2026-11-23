use std::sync::OnceLock;

/// A false substitution reads worse than a missed one.
const FUZZY_RATIO: f64 = 0.85;
const MIN_TOKEN_LEN: usize = 3;

/// The capitals a word's `listed` spellings restore wherever it is typed.
/// A word listed without capitals types as heard, and one listed in two
/// casings has no one spelling to restore.
pub(super) fn capitals<'a>(listed: &[&'a str]) -> Option<&'a str> {
    let [term, others @ ..] = listed else {
        return None;
    };
    (others.iter().all(|other| other == term) && term.chars().any(char::is_uppercase))
        .then_some(term)
}

/// A word the user does not have in their list becomes the listed word it
/// nearly is, unless it is a real English word. Two words heard for one
/// listed word merge into it, unless both are real words. Tokens are ASCII
/// words; a match must share its first letter and about its length before
/// the ratio is consulted. A listed word heard in another case types its
/// listed capitals. No fix reaches into a written name such as
/// `src/aria.toml`.
pub(super) fn apply_fuzzy<E>(
    text: &str,
    terms: &[String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<(String, Vec<(String, String)>), E> {
    check(text.len())?;
    if terms.is_empty() {
        return Ok((text.to_owned(), Vec::new()));
    }
    let mut output = String::with_capacity(text.len());
    let mut substitutions = Vec::new();
    let mut rest = text;
    while let Some((start, length)) = next_token(rest) {
        check(output.len().saturating_add(start))?;
        output.push_str(&rest[..start]);
        let before = &rest[..start];
        let token = &rest[start..start + length];
        let after = &rest[start + length..];
        let single = if in_a_written_name(before, after) {
            None
        } else {
            best_term(token, terms, known, check)?
        };
        let merged = if single.is_none() {
            merged_term(token, after, terms, known, check)?
                .filter(|(_, second)| !in_a_written_name(before, &after[*second..]))
        } else {
            None
        };
        let (heard, typed) = match merged {
            Some((typed, second)) => (&rest[start..start + length + second], Some(typed)),
            None => (
                token,
                single.map(|(Fix::Capitals(term) | Fix::NearMiss(term))| term.to_owned()),
            ),
        };
        match typed {
            Some(typed) => {
                check(output.len().saturating_add(typed.len()))?;
                output.push_str(&typed);
                substitutions.push((heard.to_owned(), typed));
            }
            None => {
                check(output.len().saturating_add(heard.len()))?;
                output.push_str(heard);
            }
        }
        rest = &rest[start + heard.len()..];
    }
    check(output.len().saturating_add(rest.len()))?;
    output.push_str(rest);
    Ok((output, substitutions))
}

/// SCOWL size 35, lowercase words and contractions of three or more
/// letters, as an fst set. `scripts/english-words.sh` rebuilds it, and
/// `THIRD-PARTY-NOTICES.md` names the source and its terms.
const ENGLISH_WORDS: &[u8] = include_bytes!("english.fst");

/// "river's" and "red-shift" are real because "river", "red" and "shift"
/// are, though the list holds no possessives or compounds.
fn is_english(folded: &str) -> bool {
    folded
        .split('-')
        .all(|part| english_words().contains(part) || english_words().contains(possessive(part).0))
}

fn english_words() -> &'static fst::Set<&'static [u8]> {
    static WORDS: OnceLock<fst::Set<&'static [u8]>> = OnceLock::new();
    // The folding test opens these same bytes, so no build that fails here
    // passes the tests.
    WORDS.get_or_init(|| fst::Set::new(ENGLISH_WORDS).expect("the embedded words are an fst set"))
}

/// The next `[A-Za-z0-9][A-Za-z0-9'-]*` as byte offset and length.
fn next_token(text: &str) -> Option<(usize, usize)> {
    let start = text.find(|c: char| c.is_ascii_alphanumeric())?;
    let length = text[start..]
        .find(|c: char| !is_token_char(c))
        .unwrap_or(text.len() - start);
    Some((start, length))
}

pub(super) fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '\'' || c == '-'
}

pub(super) fn best_term<'a, E>(
    token: &str,
    terms: &'a [String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<Option<Fix<'a>>, E> {
    check(0)?;
    let folded = token.to_lowercase();
    if token.chars().count() < MIN_TOKEN_LEN {
        return Ok(None);
    }
    let listed = listed(&folded, terms, known, check)?;
    // Listing `Linear` asks for its capitals, so a listed word takes them
    // even where it is also a real word, as in "a linear model".
    if !listed.is_empty() {
        let capitals = capitals(&listed).filter(|term| *term != token);
        return Ok(capitals.map(Fix::Capitals));
    }
    let chars: Vec<char> = folded.chars().collect();
    let mut best: Option<(&str, f64)> = None;
    for (term, term_folded) in terms.iter().zip(known) {
        check(0)?;
        // "speaker" against "wespeaker" scores 0.875: the first letter is
        // the guard that keeps it from matching.
        if term_folded.first() != chars.first() {
            continue;
        }
        let longest = chars.len().max(term_folded.len());
        let allowed = ((0.2 * longest as f64) as usize).max(1);
        if chars.len().abs_diff(term_folded.len()) > allowed {
            continue;
        }
        let ratio = ratio(&chars, term_folded, check)?;
        if ratio >= FUZZY_RATIO && best.is_none_or(|(_, best_ratio)| ratio > best_ratio) {
            best = Some((term, ratio));
        }
    }
    // A real word the recognizer wrote is what was said: "flick" is not a
    // near miss of "Flickr".
    if best.is_some() && is_english(&folded) {
        return Ok(None);
    }
    Ok(best.map(|(term, _)| Fix::NearMiss(term)))
}

/// The listed word that `first` and the word one space into `after` were
/// heard for, as "postgre sql" for `PostgreSQL`, and where that word ends.
fn merged_term<E>(
    first: &str,
    after: &str,
    terms: &[String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<Option<(String, usize)>, E> {
    let Some(Some((0, length))) = after.strip_prefix(' ').map(next_token) else {
        return Ok(None);
    };
    let (second, ending) = possessive(&after[1..1 + length]);
    if !may_merge(first, second) {
        return Ok(None);
    }
    // A word the list already knows keeps its own fix: with `kuber` listed,
    // "kuber netes" keeps its "kuber".
    if !listed(first, terms, known, check)?.is_empty()
        || !listed(second, terms, known, check)?.is_empty()
        || best_term(second, terms, known, check)?.is_some()
    {
        return Ok(None);
    }
    let merged = format!("{first}{second}").to_lowercase();
    let listed = listed(&merged, terms, known, check)?;
    let term = if listed.is_empty() {
        match best_term(&merged, terms, known, check)? {
            Some(Fix::NearMiss(term)) => term,
            _ => return Ok(None),
        }
    } else {
        capitals(&listed).unwrap_or(&merged)
    };
    Ok(Some((format!("{term}{ending}"), 1 + length)))
}

/// Whether two heard words may be one listed word. A word under three
/// letters is often one like "is" or "at", which a near miss would swallow,
/// and two real words are what was said.
fn may_merge(first: &str, second: &str) -> bool {
    let real = |word: &str| is_english(&word.to_lowercase());
    first.len() >= MIN_TOKEN_LEN && second.len() >= MIN_TOKEN_LEN && !(real(first) && real(second))
}

/// Whether more speech may merge `word` and the next word into a listed
/// word. Only a word that isn't real waits, so "ads" ending a segment types
/// at once, though "ads viser" merges within one.
pub(super) fn begins_a_merge(word: &str, terms: &[String], known: &[Vec<char>]) -> bool {
    let never = &|_| Ok::<_, std::convert::Infallible>(());
    let folded = word.to_lowercase();
    let longer = |listed: &Vec<char>| {
        listed.len() > folded.chars().count() && listed.first() == folded.chars().next().as_ref()
    };
    let fixed = || {
        let listed = listed(word, terms, known, never).unwrap_or_else(|never| match never {});
        let fix = best_term(word, terms, known, never).unwrap_or_else(|never| match never {});
        !listed.is_empty() || fix.is_some()
    };
    word.len() >= MIN_TOKEN_LEN && !is_english(&folded) && known.iter().any(longer) && !fixed()
}

/// The spellings `terms` lists for `word`, in any case.
fn listed<'a, E>(
    word: &str,
    terms: &'a [String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<Vec<&'a str>, E> {
    let folded = word.to_lowercase();
    let mut listed = Vec::new();
    for (term, listed_word) in terms.iter().zip(known) {
        check(0)?;
        if listed_word.iter().copied().eq(folded.chars()) {
            listed.push(term.as_str());
        }
    }
    Ok(listed)
}

/// `word` without its possessive ending, and the ending: "sql's" is "sql"
/// and "'s".
fn possessive(word: &str) -> (&str, &str) {
    let stem = word
        .strip_suffix("'s")
        .or_else(|| word.strip_suffix('\''))
        .unwrap_or(word);
    (stem, &word[stem.len()..])
}

/// What the fuzzy pass types in place of a heard word.
pub(super) enum Fix<'a> {
    /// The listed spelling of a word heard in another case.
    Capitals(&'a str),
    /// The listed word a heard word nearly is.
    NearMiss(&'a str),
}

/// Whether a word sits inside a written name, as "aria" does in
/// `src/aria.toml`, `aria_kit` and `@aria`. `before` and `after` are the
/// text on either side of it.
fn in_a_written_name(before: &str, after: &str) -> bool {
    const JOINS: [char; 6] = ['.', '/', '\\', '_', '@', ':'];
    let mut after = after.chars();
    before.ends_with(JOINS)
        || (after.next().is_some_and(|next| JOINS.contains(&next))
            && after.next().is_some_and(char::is_alphanumeric))
}

/// Python's `difflib.SequenceMatcher.ratio`: twice the characters in the
/// matching blocks over both lengths, the blocks found longest-first and
/// earliest-first, as difflib finds them. The thresholds were tuned
/// against that number, so it is that number.
fn ratio<E>(a: &[char], b: &[char], check: &impl Fn(usize) -> Result<(), E>) -> Result<f64, E> {
    if a.is_empty() && b.is_empty() {
        return Ok(1.0);
    }
    let mut matched = 0;
    let mut windows = vec![(0, a.len(), 0, b.len())];
    while let Some((a_low, a_high, b_low, b_high)) = windows.pop() {
        let (i, j, size) = longest_match(a, a_low, a_high, b, b_low, b_high, check)?;
        if size == 0 {
            continue;
        }
        matched += size;
        if a_low < i && b_low < j {
            windows.push((a_low, i, b_low, j));
        }
        if i + size < a_high && j + size < b_high {
            windows.push((i + size, a_high, j + size, b_high));
        }
    }
    Ok(2.0 * matched as f64 / (a.len() + b.len()) as f64)
}

/// The longest common run within the windows; ties go to the earliest
/// start in `a`, then in `b`.
fn longest_match<E>(
    a: &[char],
    a_low: usize,
    a_high: usize,
    b: &[char],
    b_low: usize,
    b_high: usize,
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<(usize, usize, usize), E> {
    check(0)?;
    let mut best = (a_low, b_low, 0);
    let mut lengths = vec![0; b_high - b_low + 1];
    for (i, a_char) in a.iter().enumerate().take(a_high).skip(a_low) {
        check(0)?;
        let mut next = vec![0; b_high - b_low + 1];
        for (j, b_char) in b.iter().enumerate().take(b_high).skip(b_low) {
            if a_char != b_char {
                continue;
            }
            let size = lengths[j - b_low] + 1;
            next[j - b_low + 1] = size;
            if size > best.2 {
                best = (i + 1 - size, j + 1 - size, size);
            }
        }
        lengths = next;
    }
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_matches_difflib() {
        let cases = [
            ("vexe16", "vex16", 0.9090909090909091),
            ("speaker", "wespeaker", 0.875),
            ("zxx9", "zx9", 0.8571428571428571),
            ("kubernets", "kubernetes", 0.9473684210526315),
            ("abcdef", "abdcef", 0.8333333333333334),
            ("abc", "abc", 1.0),
            ("abc", "xyz", 0.0),
            ("", "", 1.0),
        ];
        for (a, b, expected) in cases {
            let a: Vec<char> = a.chars().collect();
            let b: Vec<char> = b.chars().collect();
            let actual = ratio(&a, &b, &|_| Ok::<_, std::convert::Infallible>(())).unwrap();
            assert!(
                (actual - expected).abs() < 1e-12,
                "{a:?} vs {b:?}: {actual} != {expected}"
            );
        }
    }

    #[test]
    fn fuzzy_comparison_propagates_cancellation_during_matching() {
        let checks = std::cell::Cell::new(0);
        let word: Vec<char> = "kubernetes".chars().collect();
        let result = ratio(&word, &word, &|_| {
            checks.set(checks.get() + 1);
            if checks.get() == 3 {
                Err("cancelled")
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err("cancelled"));
    }

    #[test]
    fn the_english_words_are_folded_for_lookup() {
        use fst::Streamer;
        let mut words = english_words().stream();
        while let Some(word) = words.next() {
            let shaped = word.len() >= MIN_TOKEN_LEN
                && word[0].is_ascii_lowercase()
                && word.iter().all(|b| b.is_ascii_lowercase() || *b == b'\'');
            assert!(shaped, "{:?}", String::from_utf8_lossy(word));
        }
    }
}
