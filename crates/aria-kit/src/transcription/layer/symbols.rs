use std::borrow::Cow;

use super::Vocabulary;
use super::claim::{Claimed, Place, Word};
use super::correction::Writing;
use super::symbol_words::{
    Joins, Name, is_name, is_symbol, joins_always, name_at, opens_a_join_after, opens_a_name,
};
use super::vocabulary::is_english;

/// What decides how a dictation's symbols join besides its words: the
/// vocabulary, with the project's path names, how the text is written, and
/// where it sits in the dictation.
#[derive(Clone, Copy)]
pub(super) struct Reading<'a> {
    pub(super) vocabulary: &'a Vocabulary,
    pub(super) writing: Writing,
    pub(super) place: Place,
}

/// Claims each run of words that spoken symbols join as one word, as in
/// "user underscore id" → `user_id`. Two words that both have a joined
/// form, as numbers do, join in that form. A symbol that
/// [joins always](Joins::Always) is claimed even with no word to join, as
/// `src/` or `/`.
///
/// Returns where each claim starts that opens on its symbol, which stands
/// apart from the word before it, as `--verbose` does.
pub(super) fn claim<'a>(text: &str, reading: Reading<'_>, claimed: &mut Claimed<'a>) -> Vec<usize> {
    let (words, names) = merge_names(text, claimed.take_words());
    let joining = joining(text, &words, &names, reading);
    let link = |index: usize| joining.get(index).copied().flatten().and_then(Glue::link);
    let in_joined_form = |index: usize| {
        let pair = |at: usize| {
            link(at).is_some() && words[at - 1].joined.is_some() && words[at + 1].joined.is_some()
        };
        (index >= 2 && pair(index - 1)) || (index + 2 < words.len() && pair(index + 1))
    };
    let word_typed = |index: usize| match &words[index].joined {
        Some(joined) if in_joined_form(index) => joined.as_ref(),
        _ => words[index].typed(text),
    };
    let mut apart = Vec::new();
    let mut index = 0;
    while index < words.len() {
        if joining[index] == Some(Glue::Neither) || run_said_last(text, &words, &names, index) {
            let span = words[index].span.clone();
            apart.push(span.start);
            claimed.push(Word::claimed(span, Cow::Owned(typed(names[index]))));
            index += 1;
            continue;
        }
        let opening = match joining[index] {
            Some(Glue::After(opening)) => Some(opening),
            _ => None,
        };
        let opens = opening.is_some();
        let first = index + usize::from(opens);
        let mut last = first;
        while link(last + 1).is_some() {
            last += 2;
        }
        let closing = joining.get(last + 1) == Some(&Some(Glue::Before));
        if last == index && !closing {
            claimed.push(words[index].clone());
            index += 1;
            continue;
        }
        let mut typed_run = opening.unwrap_or_default().to_owned();
        typed_run.push_str(word_typed(first));
        for at in (first + 1..last).step_by(2) {
            typed_run.push_str(link(at).unwrap_or_default());
            typed_run.push_str(word_typed(at + 1));
        }
        if closing {
            last += 1;
            typed_run.push_str(&typed(names[last]));
        }
        let span = words[index].span.start..words[last].span.end;
        if opens {
            apart.push(span.start);
        }
        claimed.push(Word::claimed(span, Cow::Owned(typed_run)));
        index = last + 1;
    }
    apart
}

/// Which words beside it a symbol word glues to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Glue {
    /// Links the words on both sides, typing this.
    Both(&'static str),
    /// Opens the word after it, typing this, as "dot" opens `.env`.
    After(&'static str),
    /// A symbol that [joins always](Joins::Always), glued only to the word
    /// before it, as in `src/`.
    Before,
    /// A symbol that joins always, glued to neither word beside it.
    Neither,
}

impl Glue {
    fn link(self) -> Option<&'static str> {
        match self {
            Self::Both(link) => Some(link),
            Self::After(_) | Self::Before | Self::Neither => None,
        }
    }
}

/// Decides each join from the first word on, so a slash judges the word
/// before it as the text before it types. A slash glues to the plain
/// word before it only when the text is prose, the word has a joined form,
/// as a number does, or the word, as typed with the symbols already joining
/// it, [reads as part of a path](reads_as_a_path). Nothing else depends on
/// the words before.
fn joining(
    text: &str,
    words: &[Word<'_>],
    names: &[Option<Name>],
    reading: Reading<'_>,
) -> Vec<Option<Glue>> {
    let count = words.len();
    let mut joining = vec![None; count];
    // What the last plain word types with the symbols joining it, and what
    // the next plain word goes on from.
    let mut typed_so_far = String::new();
    let mut goes_on: Option<String> = None;
    for index in 0..count {
        let Some(name) = names[index] else {
            typed_so_far = goes_on.take().unwrap_or_default() + words[index].typed(text);
            continue;
        };
        goes_on = None;
        if name.joins == Joins::Always {
            let before = sits_before(text, words, names, index)
                && (reading.writing == Writing::Prose
                    || words[index - 1].joined.is_some()
                    || reads_as_a_path(last_word(&typed_so_far), reading));
            let after = glues_after(text, words, names, index);
            joining[index] = Some(match (before, after) {
                (true, true) => Glue::Both(name.typed),
                (false, true) => Glue::After(name.typed),
                (true, false) => Glue::Before,
                (false, false) => Glue::Neither,
            });
            if after {
                goes_on = Some(match before {
                    true => format!("{typed_so_far}{}", name.typed),
                    false => name.typed.to_owned(),
                });
            }
            continue;
        }
        if let Some(link) = link(text, words, names, index) {
            joining[index] = Some(Glue::Both(link));
            goes_on = Some(format!("{typed_so_far}{link}"));
        } else if let Some(opening) = opening(text, words, names, reading.place, index) {
            joining[index] = Some(Glue::After(opening));
            goes_on = Some(opening.to_owned());
        }
    }
    joining
}

/// Whether the name at `index` glues to the word after it, when it joins
/// always: a plain word follows, with no line break or mark between.
fn glues_after(text: &str, words: &[Word<'_>], names: &[Option<Name>], index: usize) -> bool {
    index + 1 < words.len()
        && names[index + 1].is_none()
        && names[index].is_some_and(|name| name.mark.is_none())
        && !line_break(text, words, index)
        && !words[index + 1]
            .typed(text)
            .starts_with(|c: char| c.is_ascii_punctuation())
}

/// Whether a plain word sits right before the word at `index`, with no line
/// break or mark between them.
fn sits_before(text: &str, words: &[Word<'_>], names: &[Option<Name>], index: usize) -> bool {
    index > 0
        && names[index - 1].is_none()
        && !line_break(text, words, index - 1)
        && !words[index - 1]
            .typed(text)
            .ends_with(|c: char| c.is_ascii_punctuation())
}

/// The last word of `typed`, which is all the next segment reads of it
/// once typed: "Bucket" of `S3 Bucket`.
fn last_word(typed: &str) -> &str {
    typed.split_whitespace().next_back().unwrap_or_default()
}

/// Whether `typed`, a word as it types with the symbols joining it to the
/// words before, reads as part of a path, so a slash after it glues to it:
/// `src`, `/users` and `.local` do, and `aria-kit` when it names a project
/// path. An English word reads as a word, so "run forward slash review"
/// types `run /review`.
fn reads_as_a_path(typed: &str, reading: Reading<'_>) -> bool {
    // Only the typed text decides, so a word typed before the slash reads
    // the same when the next segment reads it again.
    let word = typed
        .trim_start_matches(['(', '[', '{', '"', '\'', '’'])
        .replace('’', "'");
    let letter = |c: char| c.is_alphabetic() || matches!(c, '\'' | '-');
    reading.vocabulary.names_a_path(&word)
        || !word.chars().all(letter)
        || !is_english(&word.to_lowercase())
}

/// What a name types: its symbol, then the mark that ends a sentence on it.
fn typed(name: Option<Name>) -> String {
    let Some(name) = name else {
        return String::new();
    };
    let mut typed = name.typed.to_owned();
    typed.extend(name.mark);
    typed
}

/// What the name at `index` types when it opens the word after it: its name
/// says so, as `--` does, or it opens the dictation. A mark that ends a
/// sentence on it opens nothing.
fn opening(
    text: &str,
    words: &[Word<'_>],
    names: &[Option<Name>],
    place: Place,
    index: usize,
) -> Option<&'static str> {
    let name = names[index]?;
    let opens = name.joins == Joins::After || (index == 0 && place == Place::First);
    let next = words.get(index + 1)?;
    let joins = !line_break(text, words, index) && joinable_after_a_symbol(next.typed(text));
    (opens && joins && name.mark.is_none()).then_some(name.typed)
}

/// Whether the word at `index` names a run that opens what follows and ends
/// on a symbol that joins always, as "dot dot forward slash" names `../`,
/// with no word after it to open: none follows, or a mark ends the sentence
/// on it. Like a named `/` said alone, it types.
fn run_said_last(text: &str, words: &[Word<'_>], names: &[Option<Name>], index: usize) -> bool {
    let Some(name) = names[index].filter(|name| name.joins == Joins::After) else {
        return false;
    };
    let nothing_after =
        index + 1 == words.len() || line_break(text, words, index) || name.mark.is_some();
    nothing_after && name.typed.ends_with(joins_always)
}

/// `words` with each name of several words, as "back slash" or "dot dot",
/// merged into one word. Beside them, the name each word says, if any.
fn merge_names<'a>(text: &str, words: Vec<Word<'a>>) -> (Vec<Word<'a>>, Vec<Option<Name>>) {
    let mut whole = Vec::with_capacity(words.len());
    let mut names = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let Some(name) = name_at(nameable(text, &words, index)) else {
            whole.push(words[index].clone());
            names.push(None);
            index += 1;
            continue;
        };
        let last = index + name.words - 1;
        whole.push(Word::plain(words[index].span.start..words[last].span.end));
        names.push(Some(name));
        index = last + 1;
    }
    (whole, names)
}

/// The words from `index` that may say one name together, as heard: no
/// step claimed them and no line break parts them.
fn nameable<'s>(
    text: &'s str,
    words: &'s [Word<'_>],
    index: usize,
) -> impl Iterator<Item = &'s str> + Clone {
    words[index..]
        .iter()
        .enumerate()
        .take_while(move |(offset, word)| {
            word.claim.is_none()
                && word.joined.is_none()
                && (*offset == 0 || !line_break(text, words, index + offset - 1))
        })
        .map(move |(_, word)| word.typed(text))
}

/// Defers the symbol words at the edge of the deferred words that may
/// still link, as "underscore" in "user underscore". Each one defers the
/// word before it too when that word has a joined form, as "two" in "two
/// dot": it can't be typed alone yet. When the word before can join, it
/// defers the same words whether the cut falls before the symbol word or
/// after it.
///
/// A deferred symbol word whose left word can't join defers that word as
/// well: once typed, its words would read as if they could.
///
/// The name a text ends on defers from its start while more words decide
/// what it is: the first words of a longer name, as "back" opens "back
/// slash", or a run that joins the word after it, said whole or in part,
/// as "hyphen" opens "hyphen hyphen". A cut inside a name moves to its
/// start too: a name's first words typed alone would read as plain words.
pub(super) fn defer(text: &str, reading: Reading<'_>, claimed: &mut Claimed<'_>) {
    let heard = claimed.words();
    let open = name_open_at_end(text, heard);
    let (named, open) = heard.split_at(open.unwrap_or(heard.len()));
    let (mut words, mut names) = merge_names(text, named.to_vec());
    let mut names_a_symbol: Vec<bool> = names.iter().map(Option::is_some).collect();
    let open = open.first().zip(open.last());
    let open = open.map(|(first, last)| first.span.start..last.span.end);
    if let Some(open) = &open {
        words.push(Word::plain(open.clone()));
        names_a_symbol.push(true);
        names.push(None);
    }
    let cut = claimed.deferred_from();
    let cut_inside = words
        .iter()
        .zip(&names_a_symbol)
        .find(|(word, names)| **names && word.span.start < cut && cut < word.span.end)
        .map(|(word, _)| word.span.start);
    for at in cut_inside.into_iter().chain(open.map(|open| open.start)) {
        claimed.defer_at(at);
    }
    let joining = joining(text, &words, &names, reading);
    while let Some(at) = symbol_to_defer(
        text,
        &words,
        &names,
        &names_a_symbol,
        &joining,
        claimed.deferred_from(),
    ) {
        claimed.defer_at(at);
    }
}

/// The first of the words the text ends on that say a name more words
/// decide. Read name by name, so the words of a whole name never open
/// another.
fn name_open_at_end(text: &str, words: &[Word<'_>]) -> Option<usize> {
    let mut first = 0;
    while first < words.len() {
        let said = nameable(text, words, first);
        let left = words.len() - first;
        let named = name_at(said.clone()).map_or(0, |name| name.words);
        let open = (named < left && opens_a_name(said.clone())) || opens_a_join_after(said.clone());
        if open && said.count() == left {
            return Some(first);
        }
        first += named.max(1);
    }
    None
}

/// Where the symbol word at the edge of the words from `deferred_from` on
/// defers the text from, when it must: the first deferred word, else the
/// last word before them. A symbol that joins always defers whatever is
/// before it, since the next text's first word glues to it.
fn symbol_to_defer(
    text: &str,
    words: &[Word<'_>],
    names: &[Option<Name>],
    names_a_symbol: &[bool],
    joining: &[Option<Glue>],
    deferred_from: usize,
) -> Option<usize> {
    let first = words.partition_point(|word| word.span.start < deferred_from);
    let at_cut = names_a_symbol.get(first).is_some_and(|names| *names);
    let index = if at_cut { first } else { first.checked_sub(1)? };
    if !names_a_symbol[index] {
        return None;
    }
    let Some(left) = index.checked_sub(1) else {
        return (!at_cut).then(|| words[index].span.start);
    };
    let always = names[index].is_some_and(|name| name.joins == Joins::Always);
    let joins = match always {
        true => matches!(joining[index], Some(Glue::Both(_) | Glue::Before)),
        false => joins_after(text, words, left),
    };
    // A word the symbol stands apart from only because it reads as a word
    // types alone: the next text reads it again and parts them the same way.
    let reads_apart = always && sits_before(text, words, names, index);
    let defers_left = match joins {
        false => at_cut && !reads_apart,
        true => words[left].joined.is_some(),
    };
    if defers_left {
        return Some(words[left].span.start);
    }
    ((joins || always) && !at_cut).then(|| words[index].span.start)
}

/// What the name at `index` types when it links the words beside it, as a
/// symbol that joins both does: a word on each side that can sit in a
/// join, with no line break between.
fn link(
    text: &str,
    words: &[Word<'_>],
    names: &[Option<Name>],
    index: usize,
) -> Option<&'static str> {
    let symbol = names[index]
        .filter(|name| name.joins == Joins::Both)
        .map(|name| name.typed)?;
    let (left, right) = (words.get(index.checked_sub(1)?)?, words.get(index + 1)?);
    // Each side types its joined form only when both sides have one.
    let joined = left.joined.is_some() && right.joined.is_some();
    let form = |word| {
        if joined {
            in_join(text, word)
        } else {
            word.typed(text)
        }
    };
    let breaks = line_break(text, words, index - 1) || line_break(text, words, index);
    (!breaks && joinable_before_a_symbol(form(left)) && joinable_after_a_symbol(form(right)))
        .then_some(symbol)
}

/// Whether the word at `left` may join through the symbol word after it,
/// whatever comes next: no line break between them, and the word can sit
/// in a join.
fn joins_after(text: &str, words: &[Word<'_>], left: usize) -> bool {
    !line_break(text, words, left) && joinable_before_a_symbol(in_join(text, &words[left]))
}

/// Whether a line break separates the word at `index` from the next.
fn line_break(text: &str, words: &[Word<'_>], index: usize) -> bool {
    text[words[index].span.end..words[index + 1].span.start].contains('\n')
}

/// What `word` types in a join beside another word with a joined form:
/// "one four" types `14`.
fn in_join<'s>(text: &'s str, word: &'s Word<'_>) -> &'s str {
    word.joined.as_deref().unwrap_or_else(|| word.typed(text))
}

/// Whether `word` can sit before a symbol in a join, punctuation before it
/// aside.
fn joinable_before_a_symbol(word: &str) -> bool {
    is_joinable(word.trim_start_matches(|c: char| c.is_ascii_punctuation()))
}

/// Whether `word` can sit after a symbol in a join, punctuation after it
/// aside.
fn joinable_after_a_symbol(word: &str) -> bool {
    is_joinable(word.trim_end_matches(|c: char| c.is_ascii_punctuation()))
}

fn is_joinable(word: &str) -> bool {
    word.starts_with(char::is_alphanumeric)
        && word.ends_with(char::is_alphanumeric)
        && word.chars().all(|c| c.is_alphanumeric() || is_symbol(c))
        && !is_name(word)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::super::super::Attachment;
    use super::super::vocabulary::PersonalRules;
    use super::super::{JoinSymbols, Layer, TypedBefore, Vocabulary, Writing};
    use crate::cancel::CancelToken;

    fn applied(heard: &str) -> String {
        let mut text = heard.to_owned();
        JoinSymbols.apply(&mut text, &CancelToken::never()).unwrap();
        text
    }

    #[rstest]
    #[case::underscore_between_words("user underscore id", "user_id")]
    #[case::underscore_chain("max underscore retry underscore count", "max_retry_count")]
    #[case::hyphen_between_words("user hyphen id", "user-id")]
    #[case::dot_in_a_name("docs dot local", "docs.local")]
    #[case::joined_word_as_neighbor("aria-kit dot rs", "aria-kit.rs")]
    #[case::keeps_each_words_casing("User Underscore ID", "User_ID")]
    #[case::symbol_word_in_capitals("user UNDERSCORE id", "user_id")]
    #[case::brackets_around_the_join("(user underscore id)", "(user_id)")]
    #[case::extra_spaces_around_the_symbol("user   underscore  id", "user_id")]
    #[case::spacing_elsewhere_is_kept("say  user underscore id  now", "say  user_id  now")]
    #[case::non_ascii_word("café underscore menu", "café_menu")]
    #[case::slash_between_words("src forward slash main", "src/main")]
    #[case::backslash_between_words("c backslash windows", "c\\windows")]
    #[case::backslash_said_apart("c Back  Slash windows", "c\\windows")]
    #[case::repeated_symbol("x underscore underscore init", "x__init")]
    #[case::repeated_name_said_apart("c back slash back slash server", "c\\\\server")]
    #[case::repeated_name_said_both_ways("c backslash back slash server", "c\\\\server")]
    #[case::listed_with_commas("c forward slash, forward slash, server", "c//server")]
    #[case::joined_slash_as_neighbor("src/main forward slash lib", "src/main/lib")]
    #[case::small_word_neighbor("to underscore string", "to_string")]
    #[case::flag_after_a_word("git commit hyphen hyphen amend", "git commit --amend")]
    #[case::flag_after_a_comma("then, hyphen hyphen verbose", "then, --verbose")]
    #[case::flag_first("hyphen hyphen verbose", "--verbose")]
    #[case::path_from_here("run dot forward slash build", "run ./build")]
    #[case::path_from_the_parent(
        "cd dot dot forward slash src forward slash main",
        "cd ../src/main"
    )]
    #[case::join_before_a_flag("user underscore id hyphen hyphen all", "user_id --all")]
    // Accepted risk: a symbol word in ordinary prose still joins.
    #[case::symbol_word_in_prose("add a hyphen here", "add a-here")]
    #[case::listed_repeat_in_prose("connect the dot, dot the i", "connect the..the i")]
    fn joins(#[case] heard: &str, #[case] typed: &str) {
        assert_eq!(applied(heard), typed);
    }

    #[rstest]
    #[case::empty("")]
    #[case::symbol_word_first("dot com")]
    #[case::symbol_word_last("user underscore")]
    #[case::dash_is_a_word("aria dash kit dash test")]
    #[case::point_is_a_word("about 2 point 5 seconds")]
    #[case::comma_before_symbol("user, underscore id")]
    #[case::comma_after_symbol("user underscore, id")]
    #[case::bracket_before_right_word("user underscore (id)")]
    #[case::apostrophe_in_word("user's underscore id")]
    #[case::symbol_inside_longer_word("hyphenated underscored dotted")]
    #[case::line_break_between_words("user\nunderscore id")]
    #[case::line_break_inside_a_name("c back\nslash windows")]
    #[case::unlisted_symbols_in_a_row("x underscore dot y")]
    #[case::slash_between_words_of_prose("read and slash or write")]
    #[case::comma_inside_a_name("Come back, slash the price")]
    #[case::flag_said_last("go hyphen hyphen")]
    #[case::line_break_after_a_flag("hyphen hyphen\nverbose")]
    fn leaves_unchanged(#[case] heard: &str) {
        assert_eq!(applied(heard), heard);
    }

    #[rstest]
    #[case::name_said_last("src forward slash", "src/")]
    #[case::name_said_alone("forward slash", "/")]
    #[case::name_opening_later_text("forward slash conf", "/conf")]
    #[case::name_said_apart_last("cd back slash", "cd\\")]
    #[case::comma_after_the_name("c back slash, windows", "c\\windows")]
    #[case::comma_before_the_name("path, forward slash usr", "path, /usr")]
    #[case::line_break_before_the_name("src\nforward slash main", "src\n/main")]
    #[case::sentence_ending_on_the_name("src forward slash.", "src/.")]
    #[case::repeated_name("forward slash forward slash host", "//host")]
    #[case::after_another_symbols_name("x underscore forward slash y", "x underscore /y")]
    #[case::run_said_last("cd dot dot forward slash", "cd ../")]
    #[case::run_alone("dot forward slash", "./")]
    #[case::run_ending_a_sentence("type dot forward slash? Okay", "type ./? Okay")]
    #[case::run_ending_a_sentence_before_a_word(
        "type dot forward slash? run forward slash help",
        "type ./? run /help"
    )]
    #[case::run_before_a_line_break("cd dot dot forward slash\nls", "cd ../\nls")]
    fn a_named_slash_always_types(#[case] heard: &str, #[case] typed: &str) {
        assert_eq!(applied(heard), typed);
    }

    #[rstest]
    #[case::after_a_command("run forward slash review", "run /review")]
    #[case::said_last("go back slash", "go \\")]
    #[case::in_a_path_from_the_root("forward slash users forward slash x", "/users/x")]
    #[case::in_a_path_from_the_parent(
        "cd dot dot forward slash crates forward slash aria",
        "cd ../crates/aria"
    )]
    #[case::after_a_spoken_compound("build hyphen tools forward slash main", "build-tools /main")]
    #[case::after_a_spoken_compound_of_a_path("user underscore id forward slash x", "user_id/x")]
    #[case::after_a_number("24 forward slash 7", "24/7")]
    #[case::after_a_contraction("let's forward slash review", "let's /review")]
    #[case::after_a_hyphenated_word("re-run forward slash test", "re-run /test")]
    #[case::after_a_typographic_apostrophe("it’s forward slash x", "it’s /x")]
    fn a_named_slash_stands_apart_unless_the_word_before_reads_as_a_path(
        #[case] heard: &str,
        #[case] typed: &str,
    ) {
        assert_eq!(applied(heard), typed);
    }

    #[rstest]
    #[case::pair_of_words("read and forward slash or write", "read and/or write")]
    #[case::after_a_comma("done, forward slash review", "done, /review")]
    #[case::after_a_line_break("done\nforward slash review", "done\n/review")]
    #[case::opening_the_dictation("forward slash review", "/review")]
    fn a_named_slash_joins_a_plain_word_in_prose(#[case] heard: &str, #[case] typed: &str) {
        let corrected =
            Vocabulary::default().correct_spoken(heard, TypedBefore::Nothing, Writing::Prose);
        assert_eq!(
            corrected.map(|corrected| corrected.text).as_deref(),
            Some(typed)
        );
    }

    #[test]
    fn a_named_slash_glues_to_a_hyphenated_project_folder() {
        let vocabulary = Vocabulary::new(["aria-kit"])
            .unwrap()
            .with_path_names(["aria-kit"], usize::MAX)
            .unwrap();
        let corrected = vocabulary
            .correct_spoken(
                "aria dash kit forward slash src",
                TypedBefore::Nothing,
                Writing::Commands,
            )
            .unwrap();
        assert_eq!(corrected.text, "aria-kit/src");
    }

    #[rstest]
    #[case::listed_folder(&["crates"], "crates forward slash aria", "crates/aria")]
    #[case::listed_in_capitals(&["Crates"], "crates forward slash aria", "crates/aria")]
    #[case::unlisted_folder(&["src"], "crates forward slash aria", "crates /aria")]
    #[case::spoken_hyphenated_folder(
        &["build-tools"],
        "build hyphen tools forward slash main",
        "build-tools/main"
    )]
    fn a_named_slash_glues_to_a_project_folder(
        #[case] folders: &[&str],
        #[case] heard: &str,
        #[case] typed: &str,
    ) {
        let vocabulary = Vocabulary::default()
            .with_path_names(folders, usize::MAX)
            .unwrap();
        let corrected = vocabulary
            .correct_spoken(heard, TypedBefore::Nothing, Writing::Commands)
            .unwrap();
        assert_eq!(corrected.text, typed);
    }

    #[rstest]
    #[case::chain("forward slash src forward slash main", "/src/main")]
    #[case::listed_with_commas("Backslash, backslash, confidence.", "\\\\confidence.")]
    #[case::before_a_slash("dot local forward slash share", ".local/share")]
    fn a_symbol_opening_the_dictation_joins_the_word_after(
        #[case] heard: &str,
        #[case] typed: &str,
    ) {
        let corrected =
            Vocabulary::default().correct_spoken(heard, TypedBefore::Nothing, Writing::Commands);
        assert_eq!(
            corrected.map(|corrected| corrected.text).as_deref(),
            Some(typed)
        );
    }

    #[rstest]
    #[case::opening_symbol("dot env", TypedBefore::Nothing, ".env")]
    #[case::opening_run("dot dot forward slash src", TypedBefore::Nothing, "../src")]
    #[case::run_after_typed_text("dot forward slash build", TypedBefore::Text("run"), "./build")]
    #[case::run_after_a_sentence("hyphen hyphen verbose", TypedBefore::Apart, "--verbose")]
    fn text_opening_on_a_symbol_that_joins_only_the_word_after_stands_apart(
        #[case] heard: &str,
        #[case] before: TypedBefore<'_>,
        #[case] typed: &str,
    ) {
        let corrected = Vocabulary::default()
            .correct_spoken(heard, before, Writing::Commands)
            .unwrap();
        assert_eq!(
            (corrected.text.as_str(), corrected.attachment),
            (typed, Attachment::Apart)
        );
    }

    #[test]
    fn empty_typed_text_is_nothing_typed() {
        let corrected = Vocabulary::default().correct_spoken(
            "dot env",
            TypedBefore::Text(""),
            Writing::Commands,
        );
        assert_eq!(
            corrected.map(|corrected| corrected.text).as_deref(),
            Some(".env")
        );
    }

    #[test]
    fn a_symbol_after_a_sentence_end_stays_a_word() {
        let corrected =
            Vocabulary::default().correct_spoken("dot env", TypedBefore::Apart, Writing::Commands);
        assert_eq!(corrected, None);
    }

    #[test]
    fn a_named_slash_after_a_sentence_end_types_apart() {
        let corrected = Vocabulary::default()
            .correct_spoken("forward slash conf", TypedBefore::Apart, Writing::Commands)
            .unwrap();
        assert_eq!(
            (corrected.text.as_str(), corrected.attachment),
            ("/conf", Attachment::Apart)
        );
    }

    #[rstest]
    #[case::vocabulary_keeps_its_spelling("open cargo dot toml", "open Cargo.toml")]
    #[case::match_keeps_declared_casing("kit dash pro hyphen test", "Kit-Pro-test")]
    #[case::symbols_beside_a_match("aria-stt and user underscore id", "aria-stt and user_id")]
    #[case::match_spoken_with_a_slash("run ci slash cd", "run CI/CD")]
    #[case::match_spoken_with_a_forward_slash("run ci forward slash cd", "run CI/CD")]
    #[case::match_spoken_with_a_backslash("open tools backslash kit", "open Tools\\Kit")]
    #[case::match_spoken_with_a_name_said_apart("open tools back slash kit", "open Tools\\Kit")]
    fn joins_through_vocabulary(#[case] heard: &str, #[case] typed: &str) {
        let vocabulary =
            Vocabulary::new(["aria-stt", "Cargo.toml", "Kit-Pro", "CI/CD", "Tools\\Kit"]).unwrap();
        let corrected = vocabulary
            .correct_spoken(heard, TypedBefore::Nothing, Writing::Commands)
            .map(|corrected| corrected.text);
        assert_eq!(corrected.as_deref().unwrap_or(heard), typed);
    }

    #[rstest]
    #[case::underscore("call user", "underscore id", Some(("_id", true)))]
    #[case::chain("max", "underscore retry underscore count", Some(("_retry_count", true)))]
    #[case::dot_after_a_digit("2", "dot 5", Some((".5", true)))]
    #[case::hyphen_before_a_digit("3", "hyphen 5", Some(("-5", true)))]
    #[case::slash_before_a_digit("24", "forward slash 7", Some(("/7", true)))]
    #[case::numbers_around_a_slash("", "one forward slash two", Some(("1/2", false)))]
    #[case::hyphen_before_a_number_word("utf", "hyphen sixteen", Some(("-16", true)))]
    #[case::hyphen_before_a_long_number("version", "hyphen twenty five", Some(("-25", true)))]
    #[case::hyphen_after_a_symbol_word("the end, underscore", "hyphen twenty", Some(("hyphen 20", false)))]
    #[case::digits_after_digits_typed("30", "dot one zero zero", Some((".100", true)))]
    #[case::digits_after_an_opening_symbol("python", "underscore three dot twelve", Some(("_3.12", true)))]
    #[case::space_typed_after("src ", "forward slash main", Some(("/main", false)))]
    #[case::line_break_typed_after("src\n", "forward slash main", Some(("/main", false)))]
    #[case::slash_the_recognizer_typed_last("run it /", "compact", None)]
    #[case::path_typed_last("/Users", "forward slash x", Some(("/x", true)))]
    #[case::english_word_typed_last("run", "forward slash review", Some(("/review", false)))]
    #[case::punctuation_typed("user,", "underscore id", None)]
    fn opens_onto_the_word_typed_before(
        #[case] before: &str,
        #[case] heard: &str,
        #[case] typed: Option<(&str, bool)>,
    ) {
        let corrected = Vocabulary::default().correct_spoken(
            heard,
            if before.is_empty() {
                TypedBefore::Nothing
            } else {
                TypedBefore::Text(before)
            },
            Writing::Commands,
        );
        assert_eq!(
            corrected.as_ref().map(|corrected| (
                corrected.text.as_str(),
                corrected.attachment == Attachment::Joined
            )),
            typed
        );
    }

    #[test]
    fn a_term_never_reads_across_the_word_typed_before() {
        let vocabulary = Vocabulary::new(["New York"]).unwrap();
        assert_eq!(
            vocabulary.correct_spoken("york", TypedBefore::Text("new"), Writing::Commands),
            None
        );
    }

    #[test]
    fn personal_rules_get_what_no_join_claimed() {
        let personal = PersonalRules::new(vec![], vec![("teh".into(), "the".into())]);
        let vocabulary = Vocabulary::compile(&[], personal).unwrap();
        assert_eq!(
            vocabulary
                .correct_spoken(
                    "teh user underscore id",
                    TypedBefore::Nothing,
                    Writing::Commands
                )
                .map(|corrected| corrected.text)
                .as_deref(),
            Some("the user_id")
        );
    }

    #[test]
    fn term_correction_never_joins() {
        let vocabulary = Vocabulary::new(["aria-stt"]).unwrap();
        assert_eq!(vocabulary.correct("user underscore id"), None);
    }

    #[test]
    fn a_cancelled_layer_stops_with_the_text_unchanged() {
        let cancel = CancelToken::new();
        cancel.cancel();
        let mut text = "user underscore id".to_owned();
        assert!(JoinSymbols.apply(&mut text, &cancel).is_err());
        assert_eq!(text, "user underscore id");
    }
}
