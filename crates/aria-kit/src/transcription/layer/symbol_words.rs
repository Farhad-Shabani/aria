//! The spoken names of symbols: "underscore" → `_`, "dot dot dot" → `...`.

// A recognizer splits "backslash" as often as not, so "back slash" is listed
// too. Bare "slash" is plain talk and names nothing: "they slash prices".
const SYMBOLS: [(&str, &[&str], Joins); 5] = [
    ("_", &["underscore"], Joins::Both),
    ("-", &["hyphen"], Joins::Both),
    (".", &["dot"], Joins::Both),
    ("/", &["forward slash"], Joins::Always),
    ("\\", &["backslash", "back slash"], Joins::Always),
];

// Only listed runs type as one: "dot dot dot" → `...`. Other symbols in a
// row stay words, since symbol words meet in prose too.
const RUNS: [(&str, Joins); 8] = [
    ("__", Joins::Both),
    ("--", Joins::After),
    ("..", Joins::Both),
    ("...", Joins::Both),
    ("//", Joins::Always),
    ("\\\\", Joins::Always),
    ("./", Joins::After),
    ("../", Joins::After),
];

/// The most words any name takes: "dot dot forward slash" → `../`.
pub(super) const MAX_NAME_WORDS: usize = 4;

/// Which words beside it a symbol joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Joins {
    /// "user underscore id" → `user_id`, only when both words can join.
    Both,
    /// Types even with no word to join: "src forward slash" → `src/`.
    Always,
    /// "hyphen hyphen verbose" → `--verbose`.
    After,
}

/// A symbol's or a run's name at the start of some words: "back slash" → `\`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SymbolName {
    pub word_count: usize,
    pub typed: &'static str,
    pub joins: Joins,
    /// "forward slash." → `/.`, and no word after it joins.
    pub sentence_mark: Option<char>,
}

/// The longest name `words` start with: "dot dot forward slash" → `../`.
///
/// Commas inside a run drop: "dot, dot, dot" → `...`. A comma inside a
/// name, as "back, slash", or after a lone symbol leaves plain words,
/// unless the symbol [joins always](Joins::Always): "back slash," → `\`.
pub(super) fn leading_name<'a>(
    mut words: impl Iterator<Item = &'a str> + Clone,
) -> Option<SymbolName> {
    let mut longest = None;
    let mut runs = RUNS.map(|(run, _)| Some(run));
    let (mut count, mut listed) = (0, false);
    while let Some(said) = leading_symbol(words.clone()) {
        runs = runs.map(|run| run?.strip_prefix(said.symbol));
        let named = match count {
            0 => Some((said.symbol, said.joins)),
            _ => completed_run(&runs),
        };
        count += said.word_count;
        let named = named.filter(|(_, joins)| {
            !said.ends_in_comma
                || listed
                || (said.joins == Joins::Always && *joins == Joins::Always)
        });
        if let Some((typed, joins)) = named {
            longest = Some(SymbolName {
                word_count: count,
                typed,
                joins,
                sentence_mark: said.sentence_mark,
            });
        }
        if said.sentence_mark.is_some() || !run_can_grow(&runs) {
            break;
        }
        listed |= said.ends_in_comma;
        words.nth(said.word_count - 1);
    }
    longest
}

/// Whether `words` start a longer name: "back" of "back slash".
pub(super) fn opens_a_name<'a>(words: impl Iterator<Item = &'a str> + Clone) -> bool {
    opens(words, RUNS.map(|(run, _)| Some(run)), Opens::ALongerName)
}

/// Whether `words` say or start a run that joins only the next word:
/// "hyphen" of "hyphen hyphen".
pub(super) fn opens_a_join_after<'a>(words: impl Iterator<Item = &'a str> + Clone) -> bool {
    let runs = RUNS.map(|(run, joins)| (joins == Joins::After).then_some(run));
    opens(words, runs, Opens::ARun)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Opens {
    /// A name the words don't finish yet: "back" of "back slash".
    ALongerName,
    /// A run, said whole or in part: "hyphen" or "hyphen hyphen".
    ARun,
}

/// Whether `words` say the start of one of `runs`, maybe partway through
/// a name: "dot dot forward" of `../`.
fn opens<'a>(
    mut words: impl Iterator<Item = &'a str> + Clone,
    mut runs: [Option<&'static str>; RUNS.len()],
    what: Opens,
) -> bool {
    let mut symbols = 0;
    loop {
        if words.clone().next().is_none() {
            return symbols > 0;
        }
        let Some(said) = leading_symbol(words.clone()) else {
            return partly_said_symbol(words).is_some_and(|symbol| {
                (symbols == 0 && what == Opens::ALongerName)
                    || runs.iter().flatten().any(|rest| rest.starts_with(symbol))
            });
        };
        runs = runs.map(|run| run?.strip_prefix(said.symbol));
        let open = match what {
            Opens::ALongerName => run_can_grow(&runs),
            Opens::ARun => runs.iter().any(Option::is_some),
        };
        if !open {
            return false;
        }
        symbols += 1;
        words.nth(said.word_count - 1);
    }
}

pub(super) fn is_one_word_name(word: &str) -> bool {
    leading_name(std::iter::once(word)).is_some()
}

/// The names of `character`: `\` → "backslash", "back slash". Empty when
/// no spoken symbol types it.
fn names(character: char) -> &'static [&'static str] {
    SYMBOLS
        .iter()
        .find(|(symbol, ..)| symbol.chars().eq([character]))
        .map_or(&[], |(_, names, _)| names)
}

// Plain talk needs "forward slash" for `/`, but a listed name is said on
// purpose, so "slash" names its `/` too: "src slash main" → `src/main`.
pub(super) fn listed_names(character: char) -> Vec<&'static str> {
    let names = names(character).iter().copied();
    match character {
        '/' => names.chain(["slash"]).collect(),
        _ => names.collect(),
    }
}

/// The first name of `character`: `_` → "underscore".
pub(super) fn word(character: char) -> Option<&'static str> {
    names(character).first().copied()
}

pub(super) fn joins_always(character: char) -> bool {
    SYMBOLS
        .iter()
        .any(|(symbol, _, joins)| *joins == Joins::Always && symbol.chars().eq([character]))
}

/// Whether a spoken symbol types `character`.
pub(super) fn is_symbol(character: char) -> bool {
    word(character).is_some()
}

/// A symbol's name at the start of some words: "dot," → `.`.
struct SpokenSymbol {
    symbol: &'static str,
    joins: Joins,
    word_count: usize,
    /// "dot," ends in a comma.
    ends_in_comma: bool,
    /// "forward slash." ends a sentence. Only a symbol that joins always
    /// takes a mark.
    sentence_mark: Option<char>,
}

fn leading_symbol<'a>(words: impl Iterator<Item = &'a str> + Clone) -> Option<SpokenSymbol> {
    each_name().find_map(|(symbol, name, joins)| {
        let mut words = words.clone();
        let mut parts = name.split(' ').peekable();
        let (mut comma, mut mark) = (false, None);
        while let Some(part) = parts.next() {
            let mut word = words.next()?;
            if parts.peek().is_none() {
                if let Some(bare) = word.strip_suffix(',') {
                    (word, comma) = (bare, true);
                } else if let Some(ending) = word
                    .strip_suffix(['.', '?', '!'])
                    .filter(|_| joins == Joins::Always)
                {
                    mark = word.chars().next_back();
                    word = ending;
                }
            }
            if !word.eq_ignore_ascii_case(part) {
                return None;
            }
        }
        Some(SpokenSymbol {
            symbol,
            joins,
            word_count: name.split(' ').count(),
            ends_in_comma: comma,
            sentence_mark: mark,
        })
    })
}

/// The symbol whose name `words` start but don't finish: "back" → `\`.
fn partly_said_symbol<'a>(words: impl Iterator<Item = &'a str> + Clone) -> Option<&'static str> {
    each_name().find_map(|(symbol, name, _)| {
        let mut parts = name.split(' ');
        let begun = words.clone().all(|word| {
            parts
                .next()
                .is_some_and(|part| word.eq_ignore_ascii_case(part))
        });
        (begun && parts.next().is_some()).then_some(symbol)
    })
}

/// Each name as `(symbol, name, joins)`: `("/", "forward slash", Always)`.
fn each_name() -> impl Iterator<Item = (&'static str, &'static str, Joins)> {
    SYMBOLS
        .iter()
        .flat_map(|(symbol, names, joins)| names.iter().map(move |name| (*symbol, *name, *joins)))
}

/// The run the symbols said so far complete, given what is left of each.
fn completed_run(runs: &[Option<&'static str>; RUNS.len()]) -> Option<(&'static str, Joins)> {
    let run = runs.iter().position(|rest| *rest == Some(""))?;
    Some(RUNS[run])
}

/// Whether one more symbol could make a longer run: "dot dot" → `...`.
fn run_can_grow(runs: &[Option<&'static str>; RUNS.len()]) -> bool {
    runs.iter().flatten().any(|rest| !rest.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_name_takes_longest_name_words() {
        let longest_said = |symbol: &str| {
            symbol
                .chars()
                .map(|character| {
                    names(character)
                        .iter()
                        .max_by_key(|name| name.split(' ').count())
                        .copied()
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ")
        };
        let longest = SYMBOLS
            .iter()
            .map(|(symbol, ..)| *symbol)
            .chain(RUNS.iter().map(|(run, _)| *run))
            .map(|symbol| {
                let said = longest_said(symbol);
                let name =
                    leading_name(said.split(' ')).expect("each listed name names its symbol");
                assert_eq!(name.typed, symbol, "{said}");
                name.word_count
            })
            .max();
        assert_eq!(longest, Some(MAX_NAME_WORDS));
    }
}
