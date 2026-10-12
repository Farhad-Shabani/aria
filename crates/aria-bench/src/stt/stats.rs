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

    fn rows(count: usize, words: usize, errors: usize) -> Vec<(usize, usize)> {
        vec![(words, errors); count]
    }

    #[test]
    fn interval_brackets_the_point_estimate() {
        let mut uneven = rows(50, 10, 1);
        uneven.extend(rows(50, 10, 3));
        let (lower, upper) = wer_interval(&uneven).unwrap();

        let point = 200.0 / 1000.0;
        assert!(lower <= point && point <= upper);
        assert!(lower < upper);
    }
}
