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
/// words; a match must share its first letter, its digits and about its
/// length before the ratio is consulted, and a tie between two listed words
/// fixes nothing. A listed word heard in another case types its
/// listed capitals, unless it is a real word under three letters, as "it"
/// is for `IT`. No fix reaches into a written name such as
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
        let fix = replacement(rest, start, length, terms, known, check)?;
        let end = start + fix.as_ref().map_or(length, |(length, _)| *length);
        let heard = &rest[start..end];
        match fix {
            Some((_, Fix::Capitals(typed) | Fix::Spelling(typed))) => {
                check(output.len().saturating_add(typed.len()))?;
                output.push_str(&typed);
                substitutions.push((heard.to_owned(), typed));
            }
            None => {
                check(output.len().saturating_add(heard.len()))?;
                output.push_str(heard);
            }
        }
        rest = &rest[end..];
    }
    check(output.len().saturating_add(rest.len()))?;
    output.push_str(rest);
    Ok((output, substitutions))
}

fn replacement<E>(
    text: &str,
    start: usize,
    length: usize,
    terms: &[String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<Option<(usize, Fix)>, E> {
    let before = &text[..start];
    let word = &text[start..start + length];
    let after = &text[start + length..];
    if in_a_written_name(before, after) {
        return Ok(None);
    }
    let (stem, ending) = possessive(word);
    // An entry such as McDonald's owns its ending. Otherwise correct the stem.
    if !ending.is_empty() {
        let full = listed(&word.to_lowercase(), terms, known, check)?;
        if !full.is_empty() {
            return Ok(capitals(&full)
                .filter(|&term| term != word)
                .map(|term| (length, Fix::Capitals(term.to_owned()))));
        }
    }
    match best_term(stem, terms, known, check)? {
        Some(Match::Listed(capitals)) => {
            return Ok(capitals
                .filter(|&term| term != stem)
                .map(|term| (length, Fix::Capitals(format!("{term}{ending}")))));
        }
        Some(Match::NearMiss(term)) => {
            return Ok(Some((length, Fix::Spelling(format!("{term}{ending}")))));
        }
        None => {}
    }
    let merged = merged_term(word, after, terms, known, check)?
        .filter(|(_, second)| !in_a_written_name(before, &after[*second..]));
    Ok(merged.map(|(typed, second)| (length + second, Fix::Spelling(typed))))
}

/// What the fuzzy pass types in place of heard words.
enum Fix {
    /// The listed spelling of a word heard in another case.
    Capitals(String),
    /// The listed word heard as a near miss or as two words.
    Spelling(String),
}

/// What the list makes of one heard word.
enum Match<'a> {
    /// A listed word, typed in its capitals where it has them to restore.
    Listed(Option<&'a str>),
    /// The listed word a heard word nearly is.
    NearMiss(&'a str),
}

/// SCOWL size 35, lowercase words and contractions, as an fst set.
/// `scripts/english-words.sh` rebuilds it from the source in `THIRD-PARTY-NOTICES.md`.
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

/// The next ASCII word, including its internal apostrophes and hyphens.
fn next_token(text: &str) -> Option<(usize, usize)> {
    let start = text.find(|c: char| c.is_ascii_alphanumeric())?;
    let length = text[start..]
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '\'' || c == '-'))
        .unwrap_or(text.len() - start);
    let word = text[start..start + length].trim_end_matches(['\'', '-']);
    Some((start, word.len()))
}

fn best_term<'a, E>(
    token: &str,
    terms: &'a [String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<Option<Match<'a>>, E> {
    check(0)?;
    let folded = token.to_lowercase();
    let listed = listed(&folded, terms, known, check)?;
    let chars: Vec<char> = folded.chars().collect();
    let is_short = chars.len() < MIN_TOKEN_LEN;
    // Listing `Linear` asks for its capitals, so a listed word takes them
    // even where it is also a real word, as in "a linear model". A short
    // real word such as "it" for a listed `IT` is too common to take them.
    if !listed.is_empty() {
        let capitals = (!is_short || !is_english(&folded))
            .then(|| capitals(&listed))
            .flatten();
        return Ok(Some(Match::Listed(capitals)));
    }
    if is_short || is_english(&folded) {
        return Ok(None);
    }
    fn digits(word: &[char]) -> impl Iterator<Item = char> + '_ {
        word.iter().copied().filter(char::is_ascii_digit)
    }
    let mut best: Option<(&str, &[char], f64)> = None;
    let mut tied = false;
    for (term, term_folded) in terms.iter().zip(known) {
        check(0)?;
        // "speaker" against "wespeaker" scores 0.875: the first letter is
        // the guard that keeps it from matching.
        if term_folded.first() != chars.first() {
            continue;
        }
        // A recognizer writes the digits it heard, so `db12` names another
        // thing than `db2` rather than misspelling it.
        if !digits(&chars).eq(digits(term_folded)) {
            continue;
        }
        let shorter = chars.len().min(term_folded.len());
        if chars.len().max(term_folded.len()) > max_match_len(shorter) {
            continue;
        }
        let ratio = ratio(&chars, term_folded, check)?;
        if ratio < FUZZY_RATIO {
            continue;
        }
        match best {
            Some((_, best_folded, best_ratio)) if ratio == best_ratio => {
                tied |= best_folded != term_folded.as_slice();
            }
            Some((.., best_ratio)) if ratio < best_ratio => {}
            _ => {
                best = Some((term, term_folded, ratio));
                tied = false;
            }
        }
    }
    // Two listed words equally near leave no one word it was heard for.
    Ok(best
        .filter(|_| !tied)
        .map(|(term, ..)| Match::NearMiss(term)))
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
    if best_term(second, terms, known, check)?.is_some() {
        return Ok(None);
    }
    let merged = format!("{first}{second}").to_lowercase();
    let typed = match best_term(&merged, terms, known, check)? {
        Some(Match::Listed(capitals)) => capitals.unwrap_or(&merged),
        Some(Match::NearMiss(term)) => term,
        None => return Ok(None),
    };
    Ok(Some((format!("{typed}{ending}"), 1 + length)))
}

/// Whether two heard words may be one listed word. A word under three
/// letters is often one like "is" or "at", which a near miss would swallow,
/// and two real words are what was said.
fn may_merge(first: &str, second: &str) -> bool {
    let real = |word: &str| is_english(&word.to_lowercase());
    first.len() >= MIN_TOKEN_LEN
        && second.len() >= MIN_TOKEN_LEN
        && possessive(first).1.is_empty()
        && !(real(first) && real(second))
}

/// What follows the text a hold reads: the end of the segment, or the
/// words another step already deferred.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AfterCut {
    SegmentEnd,
    DeferredWords,
}

// Read the same replacements left to right as the typing pass. Overlapping
// split-word candidates belong to the first one that matches.
pub(super) fn waiting_from(
    text: &str,
    after: AfterCut,
    terms: &[String],
    known: &[Vec<char>],
) -> Option<usize> {
    let never = &|_| Ok::<_, std::convert::Infallible>(());
    // An earlier word can change the last match only through consecutive
    // possible merges. Ordinary prose usually stops at the previous space.
    let mut words = text.rsplit(' ');
    let mut right = words.next()?;
    let mut from = text.len() - right.len();
    for left in words {
        if !may_merge(left, right) {
            break;
        }
        from -= left.len() + 1;
        right = left;
    }
    let mut rest = &text[from..];
    while let Some((start, length)) = next_token(rest) {
        let word = &rest[start..start + length];
        let fix = replacement(rest, start, length, terms, known, never)
            .unwrap_or_else(|never| match never {});
        let consumed = fix.as_ref().map_or(length, |(length, _)| *length);
        if start + consumed == rest.len() {
            let waits = match fix {
                // Before an already held join the near miss still types. At
                // segment end the next word could claim it as part of a join.
                Some((_, Fix::Capitals(_))) => true,
                Some((_, Fix::Spelling(_))) => after == AfterCut::SegmentEnd,
                None => !in_a_written_name(&rest[..start], "") && begins_a_merge(word, known),
            };
            return waits.then_some(text.len() - rest.len() + start);
        }
        rest = &rest[start + consumed..];
    }
    None
}

fn begins_a_merge(word: &str, known: &[Vec<char>]) -> bool {
    let folded = word.to_lowercase();
    if folded.len() < MIN_TOKEN_LEN || !possessive(&folded).1.is_empty() || is_english(&folded) {
        return false;
    }
    let mut letters = [0; 128];
    for b in folded.bytes() {
        letters[usize::from(b)] += 1;
    }
    !known
        .iter()
        .any(|term| term.iter().copied().eq(folded.chars()))
        && known.iter().any(|term| {
            term.first().copied() == folded.chars().next()
                && can_complete(&letters, folded.len(), term)
        })
}

// At each split in the term, count the letters the fragment could match in
// the prefix, then give the unheard word every possible match in the suffix.
// Ignoring letter order here can only overestimate the score.
fn can_complete(letters: &[usize; 128], heard: usize, term: &[char]) -> bool {
    let longest_second = max_match_len(term.len()).saturating_sub(heard);
    if longest_second < MIN_TOKEN_LEN {
        return false;
    }
    let mut unused = *letters;
    let mut matched = 0;
    for (i, &letter) in term.iter().enumerate() {
        if letter.is_ascii() && unused[letter as usize] > 0 {
            unused[letter as usize] -= 1;
            matched += 1;
        }
        let suffix = term.len() - i - 1;
        let second = suffix.clamp(MIN_TOKEN_LEN, longest_second);
        let most = matched + second.min(suffix);
        let score = 2.0 * most as f64 / (heard + second + term.len()) as f64;
        if score >= FUZZY_RATIO {
            return true;
        }
    }
    false
}

// The lengths can differ by one character or a fifth of the longer word.
fn max_match_len(shorter: usize) -> usize {
    shorter + (shorter / 4).max(1)
}

/// The spellings `terms` lists for the lowercase word.
fn listed<'a, E>(
    folded: &str,
    terms: &'a [String],
    known: &[Vec<char>],
    check: &impl Fn(usize) -> Result<(), E>,
) -> Result<Vec<&'a str>, E> {
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
        .or_else(|| word.strip_suffix("'S"))
        .or_else(|| word.strip_suffix('\''))
        .unwrap_or(word);
    (stem, &word[stem.len()..])
}

/// Whether a word sits inside a written name, as "aria" does in
/// `src/aria.toml`, `aria_kit` and `@aria`. `before` and `after` are the
/// text on either side of it.
fn in_a_written_name(before: &str, after: &str) -> bool {
    const JOINS: [char; 6] = ['.', '/', '\\', '_', '@', ':'];
    let continues = |c: char| !c.is_ascii() && super::is_word(c);
    let mut after = after.chars();
    before.ends_with(JOINS)
        || before.ends_with(continues)
        || after.clone().next().is_some_and(continues)
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
        assert!(is_english("on-call"));
        let mut words = english_words().stream();
        while let Some(word) = words.next() {
            let shaped = word.first().is_some_and(u8::is_ascii_lowercase)
                && word.iter().all(|b| b.is_ascii_lowercase() || *b == b'\'');
            assert!(shaped, "{:?}", String::from_utf8_lossy(word));
        }
    }
}
