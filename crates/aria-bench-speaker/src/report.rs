//! Turning scored trials into the two numbers a threshold is chosen from.
//!
//! A gate admits a voice when its cosine reaches the threshold. Raise it and
//! strangers stay out, but the enrolled speaker is turned away more often.
//! Lower it and the trade runs the other way. There is no single right
//! setting — which error costs more is the caller's call — so this reports
//! the curve's crossing point and the rates at whatever gates the code ships
//! with.

use std::fmt;

use super::{Consistency, Measured, Scored};

/// What one threshold costs, as fractions of the trials it judged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub threshold: f32,
    /// Two people the gate called one.
    pub false_accept: f64,
    /// One person the gate called two.
    pub false_reject: f64,
}

impl Rates {
    /// The midpoint of the two errors. At the equal-error threshold it is the
    /// equal error rate, the one number comparable across systems.
    pub fn equal_error(&self) -> f64 {
        f64::midpoint(self.false_accept, self.false_reject)
    }
}

/// The rates a gate at `threshold` would have had on these trials.
pub fn rates_at(scored: &[Scored], threshold: f32) -> Rates {
    let (same, different): (Vec<_>, Vec<_>) = scored.iter().partition(|trial| trial.same_speaker);
    Rates {
        threshold,
        false_accept: share(&different, |trial| trial.score >= threshold),
        false_reject: share(&same, |trial| trial.score < threshold),
    }
}

/// The threshold where the two errors come closest to meeting.
///
/// Every observed score is a candidate: between two of them no trial changes
/// side, so nothing in the gaps can score better.
pub fn equal_error_rate(scored: &[Scored]) -> Option<Rates> {
    scored
        .iter()
        .map(|trial| rates_at(scored, trial.score))
        .min_by(|a, b| {
            let gap = |rates: &Rates| (rates.false_accept - rates.false_reject).abs();
            gap(a).total_cmp(&gap(b))
        })
}

/// A fraction of `trials`, or zero when there are none to judge.
fn share(trials: &[&Scored], wrong: impl Fn(&Scored) -> bool) -> f64 {
    if trials.is_empty() {
        return 0.0;
    }
    trials.iter().filter(|trial| wrong(trial)).count() as f64 / trials.len() as f64
}

/// Everything one run measured, ready to print.
pub struct Report {
    pub same: usize,
    pub different: usize,
    pub unusable: usize,
    pub equal_error: Option<Rates>,
    /// The gates the tree ships, and what they would have cost here.
    pub gates: Vec<(&'static str, Rates)>,
    pub consistency: Vec<Consistency>,
    /// The floors worth weighing against each other.
    pub consistency_floors: Vec<f32>,
}

impl Report {
    /// `gates` names the thresholds worth reporting: the constant's name, and
    /// its value. The caller passes them because this crate must not become a
    /// second place those numbers live.
    pub fn new(measured: &Measured, gates: &[(&'static str, f32)], floors: &[f32]) -> Self {
        let mut consistency: Vec<Consistency> = measured.consistency.clone();
        consistency.sort_by(|a, b| a.score.total_cmp(&b.score));
        Self {
            same: measured
                .scored
                .iter()
                .filter(|trial| trial.same_speaker)
                .count(),
            different: measured
                .scored
                .iter()
                .filter(|trial| !trial.same_speaker)
                .count(),
            unusable: measured.unusable,
            equal_error: equal_error_rate(&measured.scored),
            gates: gates
                .iter()
                .map(|&(name, threshold)| (name, rates_at(&measured.scored, threshold)))
                .collect(),
            consistency,
            consistency_floors: floors.to_vec(),
        }
    }

    /// The consistency score at `share` of the way through the sorted
    /// scores. `None` when nothing multi-sample was measured.
    fn percentile(&self, share: f64) -> Option<f32> {
        if self.consistency.is_empty() {
            return None;
        }
        let last = self.consistency.len() - 1;
        let at = (share * last as f64).round() as usize;
        Some(self.consistency[at].score)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let percent = |rate: f64| rate * 100.0;
        writeln!(
            formatter,
            "trials    {} one voice, {} two",
            self.same, self.different
        )?;
        if self.unusable > 0 {
            writeln!(
                formatter,
                "unusable  {} trials held no voice to embed",
                self.unusable
            )?;
        }
        match self.equal_error {
            Some(rates) => writeln!(
                formatter,
                "\nequal error  {:.2}%  at threshold {:.3}",
                percent(rates.equal_error()),
                rates.threshold
            )?,
            None => writeln!(formatter, "\nequal error  no trials scored")?,
        }

        writeln!(formatter, "\nshipped gates")?;
        for (name, rates) in &self.gates {
            writeln!(
                formatter,
                "  {:.2}  {name:<24} strangers in {:>6.2}%   turned away {:>6.2}%",
                rates.threshold,
                percent(rates.false_accept),
                percent(rates.false_reject)
            )?;
        }

        if self.consistency.is_empty() {
            return Ok(());
        }
        writeln!(formatter, "\nenrollment consistency")?;
        writeln!(
            formatter,
            "  lowest {:.3}   tenth {:.3}   median {:.3}   ninetieth {:.3}   highest {:.3}",
            self.percentile(0.0).unwrap_or_default(),
            self.percentile(0.1).unwrap_or_default(),
            self.percentile(0.5).unwrap_or_default(),
            self.percentile(0.9).unwrap_or_default(),
            self.percentile(1.0).unwrap_or_default(),
        )?;
        for &floor in &self.consistency_floors {
            let below = self
                .consistency
                .iter()
                .filter(|enrolled| enrolled.score < floor)
                .count();
            writeln!(
                formatter,
                "  a floor of {floor:.2} refuses {below} of {} ({:.1}%)",
                self.consistency.len(),
                percent(below as f64 / self.consistency.len() as f64),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scored(trials: &[(bool, f32)]) -> Vec<Scored> {
        trials
            .iter()
            .map(|&(same_speaker, score)| Scored {
                same_speaker,
                score,
            })
            .collect()
    }

    #[test]
    fn a_gate_below_every_score_lets_everyone_in() {
        let trials = scored(&[(true, 0.9), (false, 0.2)]);
        let rates = rates_at(&trials, 0.0);

        assert_eq!(rates.false_accept, 1.0);
        assert_eq!(rates.false_reject, 0.0);
    }

    #[test]
    fn a_gate_above_every_score_turns_everyone_away() {
        let trials = scored(&[(true, 0.9), (false, 0.2)]);
        let rates = rates_at(&trials, 1.0);

        assert_eq!(rates.false_accept, 0.0);
        assert_eq!(rates.false_reject, 1.0);
    }

    #[test]
    fn voices_that_separate_cleanly_have_no_error() {
        let trials = scored(&[(true, 0.9), (true, 0.8), (false, 0.1), (false, 0.2)]);
        let rates = equal_error_rate(&trials).expect("the trials scored");

        assert_eq!(rates.equal_error(), 0.0);
    }

    #[test]
    fn voices_that_do_not_separate_meet_in_the_middle() {
        // Each side straddles the other, so no gate does better than half.
        let trials = scored(&[(true, 0.2), (true, 0.8), (false, 0.2), (false, 0.8)]);
        let rates = equal_error_rate(&trials).expect("the trials scored");

        assert_eq!(rates.equal_error(), 0.5);
    }

    #[test]
    fn nothing_scored_has_no_crossing_point() {
        assert!(equal_error_rate(&[]).is_none());
    }

    #[test]
    fn a_side_with_no_trials_reports_no_error_of_its_own() {
        let rates = rates_at(&scored(&[(true, 0.9)]), 0.5);

        assert_eq!(rates.false_accept, 0.0, "nobody was a stranger");
        assert_eq!(rates.false_reject, 0.0);
    }

    #[test]
    fn the_report_counts_both_sides_and_the_unusable() {
        let measured = Measured {
            scored: scored(&[(true, 0.9), (false, 0.1), (false, 0.2)]),
            consistency: vec![Consistency {
                samples: 3,
                score: 0.42,
            }],
            unusable: 2,
        };

        let report = Report::new(&measured, &[("OWN_VOICE", 0.4)], &[0.5]);

        assert_eq!((report.same, report.different, report.unusable), (1, 2, 2));
        assert_eq!(report.gates[0].1.threshold, 0.4);
        assert!(
            report
                .to_string()
                .contains("a floor of 0.50 refuses 1 of 1")
        );
    }
}
