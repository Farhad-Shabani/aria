use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::Path;

use aria_kit::cancel::CancelToken;
use aria_kit::transcription::layer::vocabulary::matching_forms;

use super::{MAX_SOURCE_BYTES, MAX_TERMS, SELECTED_TERMS, Source, SourceKind, Term};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("vocabulary selection was cancelled")]
    Cancelled,
    #[error("vocabulary selection exceeds the evidence or scratch limit")]
    TooLarge,
}

pub(crate) fn eligible(kind: SourceKind, text: &str) -> bool {
    if text.is_empty()
        || text.len() > 256
        || text.trim() != text
        || text.chars().any(char::is_control)
    {
        return false;
    }
    if kind != SourceKind::Text {
        return true;
    }
    let mut letters = false;
    let mut digits = false;
    let mut structured = false;
    let mut previous_alphanumeric = false;
    let mut characters = text.char_indices().peekable();
    while let Some((offset, character)) = characters.next() {
        letters |= character.is_alphabetic();
        digits |= character.is_numeric();
        structured |= offset > 0 && character.is_uppercase();
        structured |= previous_alphanumeric
            && matches!(character, '.' | '-' | '_')
            && characters
                .peek()
                .is_some_and(|(_, next)| next.is_alphanumeric());
        previous_alphanumeric = character.is_alphanumeric();
    }
    structured || (letters && digits) || text.ends_with(['+', '#'])
}

pub(crate) fn terms(sources: &[Source], cancel: &CancelToken) -> Result<Vec<Term>, Error> {
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
    let mut inventory: HashMap<&str, (usize, usize)> = HashMap::new();
    let mut support: HashSet<(&str, &Path)> = HashSet::new();
    for source in sources {
        check()?;
        count = count
            .checked_add(source.candidates.len())
            .ok_or(Error::TooLarge)?;
        if count > MAX_TERMS {
            return Err(Error::TooLarge);
        }
        for (rank, candidate) in source.candidates.iter().enumerate() {
            check()?;
            let canonical = candidate.canonical.as_str();
            if !eligible(source.kind, canonical) {
                continue;
            }
            if !inventory.contains_key(canonical) {
                // Includes hash-table growth, collision flags, and ranking storage.
                charge(&mut scratch, 1024)?;
                inventory.insert(canonical, (0, rank));
            }
            let evidence = inventory.get_mut(canonical).expect("inserted candidate");
            evidence.1 = evidence.1.min(rank);
            if !support.contains(&(canonical, source.relative_path.as_path())) {
                charge(&mut scratch, 128)?;
                support.insert((canonical, source.relative_path.as_path()));
                evidence.0 += 1;
            }
        }
    }
    let mut aliases: HashMap<String, &str> = HashMap::new();
    let mut conflicts = HashSet::new();
    for &canonical in inventory.keys() {
        check()?;
        for alias in matching_forms(canonical) {
            check()?;
            if let Some(&other) = aliases.get(&alias) {
                if other != canonical {
                    conflicts.insert(other);
                    conflicts.insert(canonical);
                }
            } else {
                charge(&mut scratch, 512 + 4 * alias.capacity())?;
                aliases.insert(alias, canonical);
            }
        }
    }
    let mut ranked = BinaryHeap::new();
    for (&canonical, &(support, rank)) in &inventory {
        check()?;
        if !conflicts.contains(canonical) {
            ranked.push(Reverse((Reverse(support), rank, canonical)));
        }
    }
    let mut selected = Vec::new();
    while selected.len() < SELECTED_TERMS {
        check()?;
        let Some(Reverse((_, _, canonical))) = ranked.pop() else {
            break;
        };
        charge(&mut scratch, 128 + 2 * canonical.len())?;
        selected.push(Term {
            canonical: canonical.to_owned(),
            priority: 1,
        });
    }
    check()?;
    Ok(selected)
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
    use crate::vocabulary::{Candidate, PersonalRules, Vocabulary};

    fn source(path: &str, kind: SourceKind, names: &[&str]) -> Source {
        Source {
            relative_path: path.into(),
            kind,
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

    #[test]
    fn support_counts_paths_once_and_rank_ties_use_canonical_bytes() {
        let mut sources = vec![
            source("one", SourceKind::FileName, &["zeta9", "alpha", "beta"]),
            source("one", SourceKind::Text, &["zeta9"]),
            source("two", SourceKind::FileName, &["alpha", "beta"]),
            source("three", SourceKind::FileName, &["beta", "alpha"]),
        ];
        let expected = terms(&sources, &CancelToken::never()).unwrap();
        assert_eq!(
            expected
                .iter()
                .map(|term| term.canonical.as_str())
                .collect::<Vec<_>>(),
            ["alpha", "beta", "zeta9"]
        );
        assert!(expected.iter().all(|term| term.priority == 1));
        sources.reverse();
        assert_eq!(terms(&sources, &CancelToken::never()).unwrap(), expected);
    }

    #[test]
    fn below_cutoff_collisions_remove_whole_terms_and_preserve_alias_parity() {
        let names: Vec<String> = std::iter::once("aria-stt".to_owned())
            .chain((0..2048).map(|index| format!("word{index:04}")))
            .chain(std::iter::once("aria_stt".to_owned()))
            .collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let selected = terms(
            &[source("one", SourceKind::FileName, &names)],
            &CancelToken::never(),
        )
        .unwrap();
        assert_eq!(selected.len(), SELECTED_TERMS);
        assert!(
            !selected
                .iter()
                .any(|term| term.canonical.starts_with("aria"))
        );
        let selected = terms(
            &[source(
                "one",
                SourceKind::FileName,
                &["écho-stt", "Cargo.toml"],
            )],
            &CancelToken::never(),
        )
        .unwrap();
        let matcher = Vocabulary::compile(&selected, PersonalRules::default()).unwrap();
        assert_eq!(
            matcher.correct("écho dash stt cargo dot toml"),
            Some("écho-stt Cargo.toml".into())
        );
        assert!(
            terms(
                &[source(
                    "one",
                    SourceKind::FileName,
                    &["aria-stt", "aria dash stt"]
                )],
                &CancelToken::never()
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn ranking_observes_cancellation_and_scratch_is_bounded() {
        let names: Vec<String> = (0..64).map(|index| format!("word{index:04}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        let checks = std::sync::atomic::AtomicUsize::new(0);
        let cancel = CancelToken::from_fn(move || {
            checks.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 280
        });
        assert!(matches!(
            terms(&[source("one", SourceKind::FileName, &names)], &cancel),
            Err(Error::Cancelled)
        ));
        let names: Vec<String> = (0..20_000).map(|index| format!("word{index:05}")).collect();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        assert!(matches!(
            terms(
                &[source("one", SourceKind::FileName, &names)],
                &CancelToken::never()
            ),
            Err(Error::TooLarge)
        ));
    }

    #[test]
    fn cancellation_and_inventory_limits_fail_explicitly() {
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(terms(&[], &cancel), Err(Error::Cancelled)));
        let mut input = source("one", SourceKind::FileName, &["name"]);
        input
            .candidates
            .resize(MAX_TERMS + 1, input.candidates[0].clone());
        assert!(matches!(
            terms(&[input], &CancelToken::never()),
            Err(Error::TooLarge)
        ));
    }
}
