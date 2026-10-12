//! Word-level alignment of a hypothesis against its reference.
//!
//! Word error rate is the sum of substitutions, deletions, and insertions over
//! the reference length. Reporting the three separately says *how* a backend
//! failed: a model that hallucinates and a model that goes quiet can share a
//! rate while behaving nothing alike.

use std::collections::HashMap;
use std::ops::Range;

/// Alignment weights taken from NIST `sclite`.
///
/// They never enter the reported counts. Several alignments can share the
/// minimum number of edits while splitting them differently, and these weights
/// decide which of those wins, so the split follows a stated policy instead of
/// whichever path a tie-break happened to reach.
const SUBSTITUTION_COST: u32 = 4;
const DELETION_COST: u32 = 3;
const INSERTION_COST: u32 = 3;

/// How a hypothesis differs from its reference.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Alignment {
    pub substitutions: usize,
    pub deletions: usize,
    pub insertions: usize,
}

/// Aligns two word sequences, both already normalized.
pub(super) fn align(reference: &[String], hypothesis: &[String]) -> Alignment {
    Table::fill(reference, hypothesis).trace()
}

/// Counts how a hypothesis differs from its reference, both normalized with
/// the corpus scorer's own rules. This is the entry point for scorers outside
/// this benchmark — a round-trip TTS check, say — so every WER this repo
/// reports shares one normalization and one alignment policy.
pub fn word_errors(reference: &str, hypothesis: &str) -> Alignment {
    align(
        &crate::stt::normalize::words(reference),
        &crate::stt::normalize::words(hypothesis),
    )
}

pub fn reference_matches(reference: &str, hypothesis: &str) -> Vec<bool> {
    Table::fill(
        &crate::stt::normalize::words(reference),
        &crate::stt::normalize::words(hypothesis),
    )
    .reference_matches()
}

pub(crate) struct ExactAlignment {
    reference: Vec<Option<usize>>,
    hypothesis: Vec<Option<usize>>,
    anchors: Vec<usize>,
    errors: HashMap<(Edit, usize, Option<String>), usize>,
}

impl ExactAlignment {
    pub(crate) fn reference_span_matches(&self, span: Range<usize>) -> bool {
        contiguous(&self.reference[span])
    }

    pub(crate) fn hypothesis_span_matches(&self, span: Range<usize>) -> bool {
        contiguous(&self.hypothesis[span])
    }

    pub(crate) fn anchor(&self, position: usize) -> usize {
        self.anchors[position]
    }

    pub(crate) fn introduced_errors(&self, baseline: &Self) -> usize {
        self.errors
            .iter()
            .map(|(edit, count)| count.saturating_sub(*baseline.errors.get(edit).unwrap_or(&0)))
            .sum()
    }
}

fn contiguous(positions: &[Option<usize>]) -> bool {
    let Some(Some(first)) = positions.first() else {
        return false;
    };
    positions
        .iter()
        .enumerate()
        .all(|(offset, position)| *position == Some(first + offset))
}

pub(crate) fn exact_alignment(reference: &str, hypothesis: &str) -> ExactAlignment {
    let reference: Vec<_> = reference.split_whitespace().map(str::to_owned).collect();
    let hypothesis: Vec<_> = hypothesis.split_whitespace().map(str::to_owned).collect();
    let table = Table::fill(&reference, &hypothesis);
    let mut alignment = ExactAlignment {
        reference: vec![None; reference.len()],
        hypothesis: vec![None; hypothesis.len()],
        anchors: vec![0; hypothesis.len()],
        errors: HashMap::new(),
    };
    let (mut row, mut column) = (reference.len(), hypothesis.len());
    while row > 0 || column > 0 {
        let edit = table.edit(row, column);
        let anchor = if edit == Edit::Insertion {
            row
        } else {
            row - 1
        };
        if edit != Edit::Deletion {
            alignment.anchors[column - 1] = anchor;
        }
        if edit != Edit::Correct {
            let actual = (edit != Edit::Deletion).then(|| hypothesis[column - 1].clone());
            *alignment.errors.entry((edit, anchor, actual)).or_default() += 1;
        }
        match edit {
            Edit::Correct => {
                alignment.reference[row - 1] = Some(column - 1);
                alignment.hypothesis[column - 1] = Some(row - 1);
                row -= 1;
                column -= 1;
            }
            Edit::Substitution => {
                row -= 1;
                column -= 1;
            }
            Edit::Deletion => row -= 1,
            Edit::Insertion => column -= 1,
        }
    }
    alignment
}

pub(crate) fn partitioned_errors(
    reference: &str,
    hypothesis: &str,
    targets: &[bool],
) -> (Alignment, Alignment) {
    let reference = crate::stt::normalize::words(reference);
    assert_eq!(reference.len(), targets.len());
    let table = Table::fill(&reference, &crate::stt::normalize::words(hypothesis));
    let (mut row, mut column) = (table.reference_words, table.hypothesis_words);
    let mut counts = [Alignment::default(), Alignment::default()];
    while row > 0 || column > 0 {
        let edit = table.edit(row, column);
        // Insertions belong to the following reference word, or the last word at EOF.
        let anchor = if matches!(edit, Edit::Insertion) {
            row.min(reference.len().saturating_sub(1))
        } else {
            row - 1
        };
        let target = targets.get(anchor).copied().unwrap_or(false);
        let count = &mut counts[usize::from(target)];
        match edit {
            Edit::Correct => {
                row -= 1;
                column -= 1;
            }
            Edit::Substitution => {
                count.substitutions += 1;
                row -= 1;
                column -= 1;
            }
            Edit::Deletion => {
                count.deletions += 1;
                row -= 1;
            }
            Edit::Insertion => {
                count.insertions += 1;
                column -= 1;
            }
        }
    }
    (counts[1], counts[0])
}

/// One edit on the path from the reference to the hypothesis.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Edit {
    /// The words agree.
    Correct,
    /// A reference word was heard as a different word.
    Substitution,
    /// A reference word was not heard at all.
    Deletion,
    /// A word was heard that the reference does not contain.
    Insertion,
}

/// The edit-distance table, row per reference word, column per hypothesis word.
struct Table {
    reference_words: usize,
    hypothesis_words: usize,
    costs: Vec<u32>,
    edits: Vec<Edit>,
}

impl Table {
    fn fill(reference: &[String], hypothesis: &[String]) -> Self {
        let mut table = Self::empty(reference.len(), hypothesis.len());
        table.fill_borders();

        for (row, expected) in reference.iter().enumerate() {
            for (column, actual) in hypothesis.iter().enumerate() {
                let (row, column) = (row + 1, column + 1);
                let diagonal = if expected == actual {
                    (table.cost(row - 1, column - 1), Edit::Correct)
                } else {
                    (
                        table.cost(row - 1, column - 1) + SUBSTITUTION_COST,
                        Edit::Substitution,
                    )
                };
                let deletion = (table.cost(row - 1, column) + DELETION_COST, Edit::Deletion);
                let insertion = (
                    table.cost(row, column - 1) + INSERTION_COST,
                    Edit::Insertion,
                );

                // Ties keep the first candidate: diagonal, then deletion, then
                // insertion.
                let cheapest = [diagonal, deletion, insertion]
                    .into_iter()
                    .min_by_key(|(cost, _edit)| *cost)
                    .expect("the candidate edits are never empty");
                table.set(row, column, cheapest);
            }
        }
        table
    }

    fn empty(reference_words: usize, hypothesis_words: usize) -> Self {
        let cells = (reference_words + 1) * (hypothesis_words + 1);
        Self {
            reference_words,
            hypothesis_words,
            costs: vec![0; cells],
            edits: vec![Edit::Correct; cells],
        }
    }

    /// An empty hypothesis deletes every reference word; an empty reference
    /// makes every hypothesis word an insertion.
    fn fill_borders(&mut self) {
        for row in 1..=self.reference_words {
            let cost = self.cost(row - 1, 0) + DELETION_COST;
            self.set(row, 0, (cost, Edit::Deletion));
        }
        for column in 1..=self.hypothesis_words {
            let cost = self.cost(0, column - 1) + INSERTION_COST;
            self.set(0, column, (cost, Edit::Insertion));
        }
    }

    /// Walks the cheapest path back to the origin, counting what it crosses.
    fn trace(&self) -> Alignment {
        let mut alignment = Alignment::default();
        let (mut row, mut column) = (self.reference_words, self.hypothesis_words);

        while row > 0 || column > 0 {
            match self.edit(row, column) {
                Edit::Correct => {
                    row -= 1;
                    column -= 1;
                }
                Edit::Substitution => {
                    alignment.substitutions += 1;
                    row -= 1;
                    column -= 1;
                }
                Edit::Deletion => {
                    alignment.deletions += 1;
                    row -= 1;
                }
                Edit::Insertion => {
                    alignment.insertions += 1;
                    column -= 1;
                }
            }
        }
        alignment
    }

    fn reference_matches(&self) -> Vec<bool> {
        let mut matches = vec![false; self.reference_words];
        let (mut row, mut column) = (self.reference_words, self.hypothesis_words);

        while row > 0 || column > 0 {
            match self.edit(row, column) {
                Edit::Correct | Edit::Substitution => {
                    matches[row - 1] = matches!(self.edit(row, column), Edit::Correct);
                    row -= 1;
                    column -= 1;
                }
                Edit::Deletion => row -= 1,
                Edit::Insertion => column -= 1,
            }
        }
        matches
    }

    fn cell(&self, row: usize, column: usize) -> usize {
        row * (self.hypothesis_words + 1) + column
    }

    fn cost(&self, row: usize, column: usize) -> u32 {
        self.costs[self.cell(row, column)]
    }

    fn edit(&self, row: usize, column: usize) -> Edit {
        self.edits[self.cell(row, column)]
    }

    fn set(&mut self, row: usize, column: usize, (cost, edit): (u32, Edit)) {
        let cell = self.cell(row, column);
        self.costs[cell] = cost;
        self.edits[cell] = edit;
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    /// Cases and their expected counts, produced by an independent word-error
    /// scorer over the same pairs.
    const SCORER_CASES: &str = include_str!("scorer-parity.jsonl");

    #[derive(Deserialize)]
    struct ScorerCase {
        reference: String,
        hypothesis: String,
        substitutions: usize,
        deletions: usize,
        insertions: usize,
    }

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    fn aligned(reference: &str, hypothesis: &str) -> Alignment {
        align(&words(reference), &words(hypothesis))
    }

    #[test]
    fn an_exact_transcript_has_no_errors() {
        assert_eq!(
            aligned("the socket closes", "the socket closes"),
            Alignment::default()
        );
    }

    #[test]
    fn each_kind_of_error_is_counted_separately() {
        assert_eq!(
            aligned("a b c", "a x c"),
            Alignment {
                substitutions: 1,
                ..Alignment::default()
            }
        );
        assert_eq!(
            aligned("a b c", "a c"),
            Alignment {
                deletions: 1,
                ..Alignment::default()
            }
        );
        assert_eq!(
            aligned("a b c", "a b z c"),
            Alignment {
                insertions: 1,
                ..Alignment::default()
            }
        );
    }

    #[test]
    fn an_empty_transcript_loses_every_reference_word() {
        assert_eq!(
            aligned("a b c", ""),
            Alignment {
                deletions: 3,
                ..Alignment::default()
            }
        );
    }

    #[test]
    fn a_transcript_of_nothing_is_all_insertions() {
        assert_eq!(
            aligned("", "a b c"),
            Alignment {
                insertions: 3,
                ..Alignment::default()
            }
        );
    }

    #[test]
    fn reference_matches_preserve_reference_positions() {
        for (reference, hypothesis, expected) in [
            ("a a", "a", vec![false, true]),
            ("a b c", "a extra b c", vec![true, true, true]),
            ("a b c", "a c", vec![true, false, true]),
            ("a b c d", "a x c", vec![true, false, true, false]),
            ("Hello, WORLD!", "hello world", vec![true, true]),
            ("a b", "", vec![false, false]),
            ("", "extra words", vec![]),
        ] {
            assert_eq!(
                reference_matches(reference, hypothesis),
                expected,
                "{reference:?} -> {hypothesis:?}"
            );
        }
    }

    #[test]
    fn every_case_matches_an_independent_scorer() {
        let mut differences = Vec::new();
        for case in SCORER_CASES.lines() {
            let case: ScorerCase = serde_json::from_str(case).expect("the case should parse");
            let alignment = aligned(&case.reference, &case.hypothesis);
            let expected = Alignment {
                substitutions: case.substitutions,
                deletions: case.deletions,
                insertions: case.insertions,
            };
            if alignment != expected {
                differences.push(format!(
                    "  {:?} -> {:?}\n    expected {expected:?}\n    ours     {alignment:?}",
                    case.reference, case.hypothesis
                ));
            }
        }
        assert!(
            differences.is_empty(),
            "{} of {} cases differ:\n{}",
            differences.len(),
            SCORER_CASES.lines().count(),
            differences.join("\n")
        );
    }
}
