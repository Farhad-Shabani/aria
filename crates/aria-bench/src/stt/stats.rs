//! Percentile-bootstrap confidence interval over per-utterance error counts.

use aria_harness::stats::bootstrap;

/// Where the corpus-level word error rate plausibly lies, at 95%, from one
/// `(reference_words, errors)` pair per utterance.
pub(super) fn wer_interval(rows: &[(usize, usize)]) -> Option<(f64, f64)> {
    bootstrap(rows, word_error_rate)
}

fn word_error_rate(rows: &[(usize, usize)]) -> f64 {
    let words: usize = rows
        .iter()
        .map(|(reference_words, _)| reference_words)
        .sum();
    let errors: usize = rows.iter().map(|(_, errors)| errors).sum();
    if words == 0 {
        0.0
    } else {
        errors as f64 / words as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_weighs_each_utterance_by_its_words() {
        assert_eq!(word_error_rate(&[(10, 1), (40, 2)]), 0.06);
    }

    #[test]
    fn a_corpus_without_reference_words_has_no_errors() {
        assert_eq!(word_error_rate(&[(0, 0)]), 0.0);
    }
}
