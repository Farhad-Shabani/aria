//! The words that name a symbol.

// Each symbol with its names, its own name first, and what it joins. A
// recognizer writes "backslash" apart as often as whole. Bare "slash" is
// no name, since it is a word in plain talk, so "we slash prices" stays
// prose.
const SYMBOLS: [(&str, &[&str], Joins); 5] = [
    ("_", &["underscore"], Joins::Both),
    ("-", &["hyphen"], Joins::Both),
    (".", &["dot"], Joins::Both),
    ("/", &["forward slash"], Joins::Always),
    ("\\", &["backslash", "back slash"], Joins::Always),
];

// Symbols said in a row are listed, as dictation tools list "dot dot dot":
// an open rule would join any two symbol words that meet in prose.
// A run that opens what follows it, as `--` opens a flag and `./` a path,
// joins only the word after it.
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

/// The most words any name takes, as "dot dot forward slash" names `../`.
pub(super) const LONGEST_NAME: usize = 4;

/// Which of the words beside it a symbol joins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Joins {
    /// The word before it and the word after, as `_` in `user_id`, when
    /// both can sit in a join.
    Both,
    /// Whatever words are beside it, so saying its name always types it:
    /// alone, at either end, or beside any word, as `/` in `src/main`.
    Always,
    /// Only the word after it, as `--` in `--verbose`.
    After,
}

/// A name some words open with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Name {
    /// How many words say it.
    pub words: usize,
    pub typed: &'static str,
    pub joins: Joins,
    /// The mark that ends a sentence on it, as "." in "forward slash.":
    /// typed after it, and nothing joins it from after.
    pub mark: Option<char>,
}

/// The longest name `words` open with: one symbol's, as "back slash" names
/// `\`, or a listed run's, as "dot dot forward slash" names `../`.
///
/// A recognizer lists repeated words with commas, as "dot, dot,", so a
/// comma after a symbol inside a run drops, and one after the run drops
/// with them. A comma inside one symbol's name, as in "back, slash", keeps
/// its words apart. A name with only a comma after it names nothing, unless
/// it [joins always](Joins::Always): then the comma drops. A mark ending a
/// sentence on such a name stays.
pub(super) fn name_at<'a>(mut words: impl Iterator<Item = &'a str> + Clone) -> Option<Name> {
    let mut longest = None;
    let mut runs = RUNS.map(|(run, _)| Some(run));
    let (mut count, mut listed) = (0, false);
    while let Some(said) = symbol_at(words.clone()) {
        runs = runs.map(|run| run?.strip_prefix(said.symbol));
        let named = match count {
            0 => Some((said.symbol, said.joins)),
            _ => completed_run(&runs),
        };
        count += said.words;
        let named = named.filter(|(_, joins)| {
            !said.comma || listed || (said.joins == Joins::Always && *joins == Joins::Always)
        });
        if let Some((typed, joins)) = named {
            longest = Some(Name {
                words: count,
                typed,
                joins,
                mark: said.mark,
            });
        }
        if said.mark.is_some() || !goes_on(&runs) {
            break;
        }
        listed |= said.comma;
        words.nth(said.words - 1);
    }
    longest
}

/// Whether `words`, all of them, open a name that more words would finish,
/// as "back" opens "back slash" and "dot," opens "dot, dot".
pub(super) fn opens_a_name<'a>(words: impl Iterator<Item = &'a str> + Clone) -> bool {
    opens(words, RUNS.map(|(run, _)| Some(run)), Opens::ALongerName)
}

/// Whether `words`, all of them, say a run that joins only the word after
/// it, or open one, as "hyphen" opens "hyphen hyphen".
pub(super) fn opens_a_join_after<'a>(words: impl Iterator<Item = &'a str> + Clone) -> bool {
    let runs = RUNS.map(|(run, joins)| (joins == Joins::After).then_some(run));
    opens(words, runs, Opens::ARun)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Opens {
    /// Any name the words don't finish: one symbol's or a run's.
    ALongerName,
    /// A run, finished or not.
    ARun,
}

/// Whether `words`, all of them, say the first symbols of one of `runs`,
/// the last of them maybe partway.
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
        let Some(said) = symbol_at(words.clone()) else {
            return begun_symbol(words).is_some_and(|symbol| {
                (symbols == 0 && what == Opens::ALongerName)
                    || runs.iter().flatten().any(|rest| rest.starts_with(symbol))
            });
        };
        runs = runs.map(|run| run?.strip_prefix(said.symbol));
        let open = match what {
            Opens::ALongerName => goes_on(&runs),
            Opens::ARun => runs.iter().any(Option::is_some),
        };
        if !open {
            return false;
        }
        symbols += 1;
        words.nth(said.words - 1);
    }
}

/// Whether `word` alone names a symbol.
pub(super) fn is_name(word: &str) -> bool {
    name_at(std::iter::once(word)).is_some()
}

/// The names of `character`, its own name first, as "backslash" and "back
/// slash" name `\`. None when no spoken symbol types it.
pub(super) fn names(character: char) -> &'static [&'static str] {
    SYMBOLS
        .iter()
        .find(|(symbol, ..)| symbol.chars().eq([character]))
        .map_or(&[], |(_, names, _)| names)
}

/// The word that names `character`, as "underscore" names `_`: its own
/// name.
pub(super) fn word(character: char) -> Option<&'static str> {
    names(character).first().copied()
}

/// Whether `character` is a symbol that [joins always](Joins::Always), as
/// `/` is.
pub(super) fn joins_always(character: char) -> bool {
    SYMBOLS
        .iter()
        .any(|(symbol, _, joins)| *joins == Joins::Always && symbol.chars().eq([character]))
}

/// Whether a spoken symbol types `character`.
pub(super) fn is_symbol(character: char) -> bool {
    word(character).is_some()
}

/// One symbol as some words say it.
struct Said {
    symbol: &'static str,
    joins: Joins,
    words: usize,
    /// A comma follows its last word.
    comma: bool,
    /// A mark ending a sentence follows its last word, which only a symbol
    /// that joins always takes.
    mark: Option<char>,
}

/// The symbol `words` open with.
fn symbol_at<'a>(words: impl Iterator<Item = &'a str> + Clone) -> Option<Said> {
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
        Some(Said {
            symbol,
            joins,
            words: name.split(' ').count(),
            comma,
            mark,
        })
    })
}

/// The symbol whose name `words`, all of them, open and don't finish.
fn begun_symbol<'a>(words: impl Iterator<Item = &'a str> + Clone) -> Option<&'static str> {
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

/// Each name beside its symbol and what the symbol joins.
fn each_name() -> impl Iterator<Item = (&'static str, &'static str, Joins)> {
    SYMBOLS
        .iter()
        .flat_map(|(symbol, names, joins)| names.iter().map(move |name| (*symbol, *name, *joins)))
}

/// The run the symbols said so far make, given what is left of each run.
fn completed_run(runs: &[Option<&'static str>; RUNS.len()]) -> Option<(&'static str, Joins)> {
    let run = runs.iter().position(|rest| *rest == Some(""))?;
    Some(RUNS[run])
}

/// Whether another symbol may make a longer run.
fn goes_on(runs: &[Option<&'static str>; RUNS.len()]) -> bool {
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
                let name = name_at(said.split(' ')).expect("each listed name names its symbol");
                assert_eq!(name.typed, symbol, "{said}");
                name.words
            })
            .max();
        assert_eq!(longest, Some(LONGEST_NAME));
    }
}
