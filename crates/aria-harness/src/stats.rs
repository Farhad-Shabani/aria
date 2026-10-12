//! Order statistics and 95% intervals over measured samples.

pub struct Summary {
    pub count: usize,
    pub min: u64,
    pub median: u64,
    pub max: u64,
}

/// Summarizes a series of samples; `None` when the series is empty.
pub fn summarize(samples: &[u64]) -> Option<Summary> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    Some(Summary {
        count: sorted.len(),
        min: *sorted.first()?,
        median: sorted[sorted.len() / 2],
        max: *sorted.last()?,
    })
}

const RESAMPLES: usize = 1000;

pub fn paired_mean_ci95(mean: f64, independent_groups: usize) -> Option<[f64; 2]> {
    if independent_groups == 0 || !(-1.0..=1.0).contains(&mean) {
        return None;
    }
    let half_width = (2.0 * 40.0_f64.ln() / independent_groups as f64).sqrt();
    Some([(mean - half_width).max(-1.0), (mean + half_width).min(1.0)])
}

/// Where `statistic` over the population plausibly lies, at 95%, by
/// resampling `rows` with replacement. `None` below two rows.
///
/// The interval is deterministic: the same rows always produce the same
/// bounds, so two runs over one input stay comparable.
pub fn bootstrap<T: Copy>(rows: &[T], statistic: impl Fn(&[T]) -> f64) -> Option<(f64, f64)> {
    if rows.len() < 2 {
        return None;
    }
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut resample = vec![rows[0]; rows.len()];
    let mut estimates = Vec::with_capacity(RESAMPLES);
    for _ in 0..RESAMPLES {
        for slot in &mut resample {
            *slot = rows[splitmix64(&mut state) as usize % rows.len()];
        }
        estimates.push(statistic(&resample));
    }
    Some(central_95(estimates))
}

/// The middle 95% of `RESAMPLES` bootstrap estimates.
fn central_95(mut estimates: Vec<f64>) -> (f64, f64) {
    estimates.sort_by(f64::total_cmp);
    let tail = RESAMPLES / 40;
    (estimates[tail], estimates[RESAMPLES - 1 - tail])
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut mixed = *state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    mixed ^ (mixed >> 31)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accepted,
    Regressed,
    Inconclusive,
}

impl Decision {
    pub fn of(interval: Option<(f64, f64)>, margin: f64) -> Self {
        match interval {
            Some((lower, upper))
                if lower.is_finite()
                    && upper.is_finite()
                    && lower <= upper
                    && margin.is_finite()
                    && margin >= 0.0 =>
            {
                if upper <= margin {
                    Self::Accepted
                } else if lower > margin {
                    Self::Regressed
                } else {
                    Self::Inconclusive
                }
            }
            _ => Self::Inconclusive,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Regressed => "regressed",
            Self::Inconclusive => "inconclusive",
        }
    }
}

pub fn difference_interval(baseline: &[u64], candidate: &[u64]) -> Option<(f64, f64)> {
    if baseline.len() < 2 || candidate.len() < 2 {
        return None;
    }
    let mut state = 0x9E37_79B9_7F4A_7C15;
    let mut before = vec![0; baseline.len()];
    let mut after = vec![0; candidate.len()];
    let mut estimates = Vec::with_capacity(RESAMPLES);
    for _ in 0..RESAMPLES {
        for sample in &mut before {
            *sample = baseline[splitmix64(&mut state) as usize % baseline.len()];
        }
        for sample in &mut after {
            *sample = candidate[splitmix64(&mut state) as usize % candidate.len()];
        }
        before.sort_unstable();
        after.sort_unstable();
        estimates.push(after[after.len() / 2] as f64 - before[before.len() / 2] as f64);
    }
    Some(central_95(estimates))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_interval_uses_independent_groups_and_bounded_scores() {
        let interval = paired_mean_ci95(1.0, 100).unwrap();
        assert!((interval[0] - 0.7283796968518761).abs() < 1e-12);
        assert_eq!(interval[1], 1.0);
        assert_eq!(paired_mean_ci95(-1.0, 1), Some([-1.0, 1.0]));
        for (mean, groups) in [(0.0, 0), (f64::NAN, 1), (1.1, 1)] {
            assert_eq!(paired_mean_ci95(mean, groups), None);
        }
    }

    fn median(samples: &[u64]) -> f64 {
        summarize(samples).unwrap().median as f64
    }

    #[test]
    fn the_interval_brackets_the_statistic() {
        let samples = [90, 95, 100, 100, 105, 110, 300];
        let (lower, upper) = bootstrap(&samples, median).unwrap();
        assert!(lower <= 100.0 && 100.0 <= upper, "{lower}..{upper}");
        assert!(lower < upper);
    }

    #[test]
    fn identical_samples_give_identical_bounds_across_calls() {
        let samples = [7, 9, 11, 13, 15];
        assert_eq!(bootstrap(&samples, median), bootstrap(&samples, median));
    }

    #[test]
    fn a_single_sample_has_no_interval() {
        assert!(bootstrap(&[42], median).is_none());
    }

    #[test]
    fn unequal_counts_compare_the_change_without_pairing_unrelated_samples() {
        assert_eq!(
            difference_interval(&[100; 10], &[110; 5]),
            Some((10.0, 10.0))
        );
        assert_eq!(difference_interval(&[100], &[100; 5]), None);
        assert_eq!(Decision::of(Some((10.0, 10.0)), 0.0), Decision::Regressed);
        assert_eq!(Decision::of(Some((0.0, 0.0)), 0.0), Decision::Accepted);
        assert_eq!(Decision::of(Some((-5.0, 8.0)), 5.0), Decision::Inconclusive);
        assert_eq!(Decision::of(None, 5.0), Decision::Inconclusive);
        assert_eq!(
            Decision::of(Some((0.0, 0.0)), f64::NAN),
            Decision::Inconclusive
        );
    }
}
