use std::collections::{HashMap, HashSet};
use std::path::Component;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::layer::vocabulary::matching_forms;

use super::{MAX_SOURCE_BYTES, MAX_TERMS, SELECTED_TERMS, Source, SourceKind, Term};

const MAX_WORDS: usize = 6;
const MAX_NUMBER_DIGITS: usize = 2;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("vocabulary selection was cancelled")]
    Cancelled,
    #[error("vocabulary selection exceeds the evidence or scratch limit")]
    TooLarge,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Selection {
    pub terms: Vec<Term>,
    /// The plain folder names the sources' paths run through, shallow
    /// first, up to the cap. Dictation glues a slash to them.
    pub folders: Vec<String>,
    /// Names that pass the keep rules, before the cap.
    pub found: usize,
}

/// A name has a spoken form only through a symbol, and is sayable only when
/// short, with at most one short number: `vex16.rs`, not `icon-1024-dark.png`.
pub(crate) fn eligible(name: &str) -> bool {
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return false;
    }
    let mut parts = 0;
    let mut words = 0;
    let mut numbers = 0;
    for part in name.split(['.', '-', '_']).filter(|part| !part.is_empty()) {
        if !part.chars().all(char::is_alphanumeric) || part.chars().all(char::is_numeric) {
            return false;
        }
        parts += 1;
        for word in spoken_words(part) {
            words += 1;
            if word.chars().all(char::is_numeric) {
                numbers += 1;
                if word.chars().count() > MAX_NUMBER_DIGITS {
                    return false;
                }
            }
        }
    }
    parts >= 2 && words <= MAX_WORDS && numbers <= 1
}

/// `AppDelegate` → `App`, `Delegate`; `HTMLParser` → `HTML`, `Parser`;
/// `vex16` → `vex`, `16`.
fn spoken_words(part: &str) -> Vec<&str> {
    let characters: Vec<(usize, char)> = part.char_indices().collect();
    let mut words = Vec::new();
    let mut start = 0;
    for index in 1..characters.len() {
        let (offset, character) = characters[index];
        let previous = characters[index - 1].1;
        let lower_follows = characters
            .get(index + 1)
            .is_some_and(|(_, next)| next.is_lowercase());
        if previous.is_numeric() != character.is_numeric()
            || (previous.is_lowercase() && character.is_uppercase())
            || (previous.is_uppercase() && character.is_uppercase() && lower_follows)
        {
            words.push(&part[start..offset]);
            start = offset;
        }
    }
    words.push(&part[start..]);
    words
}

/// Keeps the names that pass `eligible` and share no spoken form, shallow
/// names first, up to the cap.
pub fn terms(sources: &[Source], cancel: &CancelToken) -> Result<Selection, Error> {
    let check = || {
        if cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    };
    check()?;
    let mut count = 0usize;
    let mut scratch = 64 * 1024;
    let mut depths: HashMap<&str, usize> = HashMap::new();
    let mut folders: HashMap<&str, usize> = HashMap::new();
    for source in sources {
        check()?;
        count = count
            .checked_add(source.candidates.len())
            .ok_or(Error::TooLarge)?;
        if count > MAX_TERMS {
            return Err(Error::TooLarge);
        }
        let depth = source
            .relative_path
            .components()
            .filter(|component| matches!(component, Component::Normal(_)))
            .count();
        for (depth, folder) in folders_of(source) {
            check()?;
            match folders.get_mut(folder) {
                Some(shallowest) => *shallowest = (*shallowest).min(depth),
                None => {
                    charge(&mut scratch, 256)?;
                    folders.insert(folder, depth);
                }
            }
        }
        for candidate in &source.candidates {
            check()?;
            let canonical = candidate.canonical.as_str();
            if !eligible(canonical) {
                continue;
            }
            match depths.get_mut(canonical) {
                Some(shallowest) => *shallowest = (*shallowest).min(depth),
                None => {
                    charge(&mut scratch, 256)?;
                    depths.insert(canonical, depth);
                }
            }
        }
    }
    let mut aliases: HashMap<String, &str> = HashMap::new();
    let mut conflicts = HashSet::new();
    for &canonical in depths.keys() {
        check()?;
        for alias in matching_forms(canonical) {
            check()?;
            if let Some(&other) = aliases.get(&alias) {
                if other != canonical {
                    conflicts.insert(other);
                    conflicts.insert(canonical);
                }
            } else {
                charge(&mut scratch, 128 + 2 * alias.capacity())?;
                aliases.insert(alias, canonical);
            }
        }
    }
    let mut kept: Vec<(usize, &str)> = depths
        .into_iter()
        .filter(|(canonical, _)| !conflicts.contains(canonical))
        .map(|(canonical, depth)| (depth, canonical))
        .collect();
    check()?;
    kept.sort_unstable();
    check()?;
    let found = kept.len();
    let terms = kept
        .into_iter()
        .take(SELECTED_TERMS)
        .map(|(_, canonical)| Term {
            canonical: canonical.to_owned(),
            priority: 1,
        })
        .collect();
    let mut folders: Vec<(usize, &str)> = folders
        .into_iter()
        .map(|(folder, depth)| (depth, folder))
        .collect();
    check()?;
    folders.sort_unstable();
    let folders = folders
        .into_iter()
        .take(SELECTED_TERMS)
        .map(|(_, folder)| folder.to_owned())
        .collect();
    Ok(Selection {
        terms,
        folders,
        found,
    })
}

/// The plain names of the folders a source's path runs through, each with
/// its depth: a folder's own name too, but not a file's. A name with a
/// symbol other than a hyphen inside is a term already.
fn folders_of(source: &Source) -> impl Iterator<Item = (usize, &str)> {
    let mut names: Vec<&str> = source
        .relative_path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect();
    if source.kind == SourceKind::FileName {
        names.pop();
    }
    names
        .into_iter()
        .enumerate()
        .map(|(index, name)| (index + 1, name))
        // A hyphen reads as a letter before a slash, so `aria-kit` must be
        // listed to glue as `aria-kit/src`.
        .filter(|(_, name)| {
            name.chars().all(|c| c.is_alphanumeric() || c == '-')
                && name.chars().any(char::is_alphabetic)
        })
}

fn charge(scratch: &mut usize, bytes: usize) -> Result<(), Error> {
    *scratch = scratch.checked_add(bytes).ok_or(Error::TooLarge)?;
    if *scratch > MAX_SOURCE_BYTES {
        return Err(Error::TooLarge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocabulary::{Candidate, PersonalRules, SourceKind, Vocabulary};

    fn source(path: &str, names: &[&str]) -> Source {
        Source {
            relative_path: path.into(),
            kind: SourceKind::FileName,
            fingerprint: [0; 32],
            input_bytes: 256,
            candidates: names
                .iter()
                .map(|name| Candidate {
                    canonical: (*name).into(),
                    span: 0..name.len(),
                })
                .collect(),
        }
    }

    /// Distinct sayable names: `name-aaaa`, `name-aaab`, …
    fn generated(count: usize) -> Vec<String> {
        (0..count)
            .map(|mut index| {
                let mut letters = [b'a'; 4];
                for letter in letters.iter_mut().rev() {
                    *letter += (index % 26) as u8;
                    index /= 26;
                }
                format!("name-{}", std::str::from_utf8(&letters).unwrap())
            })
            .collect()
    }

    fn canonicals(selection: &Selection) -> Vec<&str> {
        selection
            .terms
            .iter()
            .map(|term| term.canonical.as_str())
            .collect()
    }

    #[test]
    fn a_name_is_kept_only_with_a_symbol_and_a_sayable_shape() {
        for (name, kept) in [
            ("Cargo.toml", true),
            ("aria-kit", true),
            ("batch_job.py", true),
            ("vex16.rs", true),
            ("AppDelegate.swift", true),
            ("README", false),
            ("Makefile", false),
            ("261018-dependency-audit.md", false),
            ("icon-1024-dark.png", false),
            ("utf-8", false),
            ("model.v123.bin", false),
            ("one-two-three-four-five-six.rs", false),
            ("one-two-three-four-five.rs", true),
            ("my notes.txt", false),
            ("name.", false),
        ] {
            assert_eq!(eligible(name), kept, "{name}");
        }
    }

    #[test]
    fn shallow_names_win_the_cap() {
        let names = generated(SELECTED_TERMS);
        let mut sources: Vec<Source> = names
            .iter()
            .map(|name| source(&format!("a/b/{name}"), &[name]))
            .collect();
        sources.push(source("zeta-top.rs", &["zeta-top.rs"]));
        sources.push(source("a/b/zeta-top.rs", &["zeta-top.rs"]));
        let selected = terms(&sources, &CancelToken::never()).unwrap();
        assert_eq!(selected.found, SELECTED_TERMS + 1);
        assert_eq!(selected.terms.len(), SELECTED_TERMS);
        assert_eq!(canonicals(&selected)[0], "zeta-top.rs");
    }

    #[test]
    fn folders_are_the_word_names_a_path_runs_through() {
        let mut folder = source("tools/aria-cli", &["aria-cli"]);
        folder.kind = SourceKind::DirectoryName;
        let sources = [
            source("crates/aria-kit/src/lib.rs", &["lib.rs"]),
            source("docs/2024/notes.md", &["notes.md"]),
            folder,
        ];
        let selected = terms(&sources, &CancelToken::never()).unwrap();
        assert_eq!(
            selected.folders,
            ["crates", "docs", "tools", "aria-cli", "aria-kit", "src"]
        );
    }

    #[test]
    fn selection_ignores_source_order() {
        let mut sources = vec![
            source("one/two", &["zeta9.rs", "alpha.rs"]),
            source("one", &["beta.rs", "README"]),
            source("three", &["alpha.rs"]),
        ];
        let expected = terms(&sources, &CancelToken::never()).unwrap();
        assert_eq!(canonicals(&expected), ["alpha.rs", "beta.rs", "zeta9.rs"]);
        assert_eq!(expected.found, 3);
        sources.reverse();
        assert_eq!(terms(&sources, &CancelToken::never()).unwrap(), expected);
    }

    #[test]
    fn collisions_below_the_cap_remove_both_names() {
        let mut names = generated(SELECTED_TERMS);
        names.push("aria-stt".to_owned());
        names.push("Aria-STT".to_owned());
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let selected = terms(&[source("one", &names)], &CancelToken::never()).unwrap();
        assert_eq!(selected.found, SELECTED_TERMS);
        assert!(
            !selected
                .terms
                .iter()
                .any(|term| term.canonical.starts_with("aria"))
        );
    }

    #[test]
    fn a_selected_non_ascii_name_is_typed() {
        let selected = terms(
            &[source("one", &["écho-stt", "Cargo.toml"])],
            &CancelToken::never(),
        )
        .unwrap();
        let matcher = Vocabulary::compile(&selected.terms, PersonalRules::default()).unwrap();
        assert_eq!(
            matcher.correct("écho dash stt cargo dot toml"),
            Some("écho-stt Cargo.toml".into())
        );
    }

    #[test]
    fn a_full_entry_budget_of_names_fits_the_scratch_limit() {
        let names = generated(20_000);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let selected = terms(&[source("one", &names)], &CancelToken::never()).unwrap();
        assert_eq!(selected.found, 20_000);
    }

    #[test]
    fn a_cancelled_selection_fails() {
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(terms(&[], &cancel), Err(Error::Cancelled)));
        let names = generated(64);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let cancel = CancelToken::from_fn(move || {
            checks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 100
        });
        assert!(matches!(
            terms(&[source("one", &names)], &cancel),
            Err(Error::Cancelled)
        ));
    }

    #[test]
    fn evidence_past_the_limits_is_too_large() {
        let mut input = source("one", &["name.rs"]);
        input
            .candidates
            .resize(MAX_TERMS + 1, input.candidates[0].clone());
        assert!(matches!(
            terms(&[input], &CancelToken::never()),
            Err(Error::TooLarge)
        ));
        let names = generated(MAX_TERMS);
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert!(matches!(
            terms(&[source("one", &names)], &CancelToken::never()),
            Err(Error::TooLarge)
        ));
    }
}
