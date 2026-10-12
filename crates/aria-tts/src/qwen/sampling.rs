//! Logit sampling matching the reference generation defaults: repetition
//! penalty on the raw logits, suppressed ranges, temperature scaling, top-k,
//! then top-p truncation before softmax sampling.

use std::collections::HashSet;

use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

#[derive(Debug, thiserror::Error)]
#[error("qwen: sampling failed: {0}")]
pub struct SamplingError(String);

/// Repeatable with the same seed and inputs. The random stream differs from PyTorch's.
pub struct Sampler {
    temperature: f32,
    top_k: usize,
    top_p: f32,
    repetition_penalty: f32,
    rng: StdRng,
}

impl Sampler {
    pub fn new(
        temperature: f32,
        top_k: usize,
        top_p: f32,
        repetition_penalty: f32,
        seed: u64,
    ) -> Self {
        Self {
            temperature,
            top_k,
            top_p,
            repetition_penalty,
            rng: StdRng::seed_from_u64(seed),
        }
    }

    /// Deterministic argmax pick with identical logit preprocessing.
    #[allow(clippy::too_many_arguments)]
    pub fn argmax(
        &self,
        logits: &mut [f32],
        penalized: &HashSet<u32>,
        banned: &[u32],
        suppressed: Option<std::ops::Range<u32>>,
        keep: &[u32],
    ) -> Result<usize, SamplingError> {
        self.process(logits, penalized, banned, suppressed, keep);
        let best = logits
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .ok_or_else(|| SamplingError("empty distribution".to_owned()))?;
        Ok(best)
    }

    /// Picks an index from `logits`. `penalized` marks ids already generated
    /// (repetition penalty applies), `banned` are forced off, and `suppressed`
    /// removes a contiguous id range except for ids listed in `keep`.
    #[allow(clippy::too_many_arguments)]
    pub fn sample(
        &mut self,
        logits: &mut [f32],
        penalized: &HashSet<u32>,
        banned: &[u32],
        suppressed: Option<std::ops::Range<u32>>,
        keep: &[u32],
    ) -> Result<usize, SamplingError> {
        self.process(logits, penalized, banned, suppressed, keep);
        // Float rounding can leave a draw past the accumulated mass; the most
        // probable index catches it instead of a suppressed one.
        let probs = softmax_values(logits, &(0..logits.len()).collect::<Vec<_>>());
        let draw: f32 = self.rng.random();
        let mut cumulative = 0.0f32;
        let mut best = (0usize, f32::NEG_INFINITY);
        for (i, prob) in probs.iter().enumerate() {
            cumulative += prob;
            if *prob > best.1 {
                best = (i, *prob);
            }
            if draw < cumulative {
                return Ok(i);
            }
        }
        Ok(best.0)
    }

    fn process(
        &self,
        logits: &mut [f32],
        penalized: &HashSet<u32>,
        banned: &[u32],
        suppressed: Option<std::ops::Range<u32>>,
        keep: &[u32],
    ) {
        let keep_set: HashSet<u32> = keep.iter().copied().collect();

        // Repetition penalty: shrink positives, grow negatives.
        for (id, logit) in logits.iter_mut().enumerate() {
            let id = id as u32;
            if penalized.contains(&id) {
                *logit = if *logit > 0.0 {
                    *logit / self.repetition_penalty
                } else {
                    *logit * self.repetition_penalty
                };
            }
            if banned.contains(&id)
                || suppressed
                    .as_ref()
                    .is_some_and(|range| range.contains(&id) && !keep_set.contains(&id))
            {
                *logit = f32::NEG_INFINITY;
            }
        }

        if self.temperature != 1.0 && self.temperature > 0.0 {
            for logit in logits.iter_mut() {
                *logit /= self.temperature;
            }
        }

        // Top-k: suppress logits below the k-th largest finite value.
        if self.top_k > 0 && self.top_k < logits.len() {
            let mut order: Vec<usize> = (0..logits.len())
                .filter(|&i| logits[i] > f32::NEG_INFINITY)
                .collect();
            if order.len() > self.top_k {
                order.sort_by(|a, b| logits[*b].total_cmp(&logits[*a]));
                let threshold = order[self.top_k - 1];
                for i in order.iter().skip(self.top_k) {
                    if logits[*i] < logits[threshold] {
                        logits[*i] = f32::NEG_INFINITY;
                    }
                }
            }
        }

        // Top-p: smallest prefix of sorted probabilities covering top_p mass.
        if self.top_p < 1.0 {
            let mut order: Vec<usize> = (0..logits.len())
                .filter(|&i| logits[i] > f32::NEG_INFINITY)
                .collect();
            order.sort_by(|a, b| logits[*b].total_cmp(&logits[*a]));
            let probs: Vec<f32> = softmax_values(logits, &order);
            let mut cumulative = 0.0f32;
            let mut cutoff = order.len();
            for (rank, prob) in probs.iter().enumerate() {
                cumulative += prob;
                if cumulative > self.top_p {
                    cutoff = rank + 1;
                    break;
                }
            }
            for &i in order.iter().skip(cutoff) {
                logits[i] = f32::NEG_INFINITY;
            }
        }
    }
}

fn softmax_values(logits: &[f32], indices: &[usize]) -> Vec<f32> {
    let max = indices
        .iter()
        .map(|&i| logits[i])
        .fold(f32::NEG_INFINITY, f32::max);
    let exps: Vec<f32> = indices.iter().map(|&i| (logits[i] - max).exp()).collect();
    let sum: f32 = exps.iter().sum();
    exps.into_iter().map(|e| e / sum).collect()
}
