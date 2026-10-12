//! Kaldi log mel filterbank features, as WeSpeaker's models were trained on.
//!
//! WeSpeaker computes features with `torchaudio.compliance.kaldi.fbank`,
//! overriding only the mel count, the frame geometry, the window, and the
//! dither. Everything else is a Kaldi default, and the defaults are not
//! incidental — the model learned on them:
//!
//! - 16 kHz, 80 mel bins from 20 Hz to Nyquist, 25 ms frames, 10 ms shift
//! - the DC offset removed per frame, then preemphasis at 0.97
//! - a Hamming window, no dither
//! - the 400-sample frame zero-padded to 512 before the transform
//! - triangles placed on exact mel positions, not on rounded FFT bins
//! - the log floored at `f32::EPSILON`, then per-bin mean subtraction
//!
//! A probe test pins all of it against torchaudio's own output.

use std::sync::OnceLock;

use realfft::RealFftPlanner;

pub(super) const FRAME_LENGTH: usize = 400;
const FRAME_SHIFT: usize = 160;
/// Kaldi rounds the transform up to a power of two, so a 25 ms frame is
/// analyzed at 512 points rather than 400.
const PADDED_LENGTH: usize = 512;
const N_FREQS: usize = PADDED_LENGTH / 2 + 1;
/// Kaldi's mel banks span the transform's lower half and leave Nyquist out.
const N_FFT_BINS: usize = PADDED_LENGTH / 2;
pub(super) const N_MELS: usize = 80;
const SAMPLE_RATE: f64 = 16_000.0;
const LOW_FREQ: f64 = 20.0;
const PREEMPHASIS: f64 = 0.97;
/// What Kaldi floors the log at: `std::numeric_limits<float>::epsilon()`.
const LOG_FLOOR: f64 = f32::EPSILON as f64;

/// Features for one audio window, `[T, 80]`; empty when the window is too
/// short for a single frame.
pub(super) fn fbank(samples: &[f32]) -> Vec<[f32; N_MELS]> {
    if samples.len() < FRAME_LENGTH {
        return Vec::new();
    }
    let n_frames = 1 + (samples.len() - FRAME_LENGTH) / FRAME_SHIFT;
    let window = hamming();
    let mel = mel_filterbank();

    let fft = RealFftPlanner::<f64>::new().plan_fft_forward(PADDED_LENGTH);
    let mut input = fft.make_input_vec();
    let mut spectrum = fft.make_output_vec();
    let mut power = [0.0_f64; N_FREQS];

    let mut features = vec![[0.0_f64; N_MELS]; n_frames];
    for (index, feature) in features.iter_mut().enumerate() {
        let frame = &samples[index * FRAME_SHIFT..index * FRAME_SHIFT + FRAME_LENGTH];
        // Kaldi reads waveforms at int16 scale, whatever the file held.
        for (into, &sample) in input.iter_mut().zip(frame) {
            *into = f64::from(sample) * 32_768.0;
        }
        input[FRAME_LENGTH..].fill(0.0);

        let offset = input[..FRAME_LENGTH].iter().sum::<f64>() / FRAME_LENGTH as f64;
        for value in &mut input[..FRAME_LENGTH] {
            *value -= offset;
        }
        // Backwards, so each sample still sees its untouched predecessor.
        // The first has none and stands in for its own.
        for at in (1..FRAME_LENGTH).rev() {
            input[at] -= PREEMPHASIS * input[at - 1];
        }
        input[0] -= PREEMPHASIS * input[0];
        for (value, &weight) in input[..FRAME_LENGTH].iter_mut().zip(window) {
            *value *= f64::from(weight);
        }

        fft.process(&mut input, &mut spectrum)
            .expect("buffer lengths come from the plan");
        for (bin, value) in power.iter_mut().zip(&spectrum) {
            *bin = value.norm_sqr();
        }
        for (out, filter) in feature.iter_mut().zip(mel) {
            let energy: f64 = power[..N_FFT_BINS]
                .iter()
                .zip(filter)
                .map(|(p, w)| p * w)
                .sum();
            *out = energy.max(LOG_FLOOR).ln();
        }
    }

    // Subtract each bin's mean over time.
    let mut mean = [0.0_f64; N_MELS];
    for feature in &features {
        for (sum, value) in mean.iter_mut().zip(feature) {
            *sum += value;
        }
    }
    features
        .iter()
        .map(|feature| {
            std::array::from_fn(|bin| (feature[bin] - mean[bin] / n_frames as f64) as f32)
        })
        .collect()
}

fn hamming() -> &'static [f32; FRAME_LENGTH] {
    static WINDOW: OnceLock<[f32; FRAME_LENGTH]> = OnceLock::new();
    WINDOW.get_or_init(|| {
        std::array::from_fn(|n| {
            let phase = std::f64::consts::TAU * n as f64 / (FRAME_LENGTH - 1) as f64;
            (0.54 - 0.46 * phase.cos()) as f32
        })
    })
}

/// Triangular mel filters over the transform's lower half.
///
/// Each triangle is placed at exact mel positions and every FFT bin is
/// weighted by where its own frequency falls inside one. Rounding the
/// triangle's corners to whole bins instead — the textbook shortcut —
/// collapses the narrow low-frequency filters onto each other.
fn mel_filterbank() -> &'static Vec<[f64; N_FFT_BINS]> {
    static FILTERS: OnceLock<Vec<[f64; N_FFT_BINS]>> = OnceLock::new();
    FILTERS.get_or_init(|| {
        let hz_to_mel = |hz: f64| 1127.0 * (1.0 + hz / 700.0).ln();
        let bin_width = SAMPLE_RATE / PADDED_LENGTH as f64;
        let mel_low = hz_to_mel(LOW_FREQ);
        let mel_high = hz_to_mel(SAMPLE_RATE / 2.0);
        let mel_delta = (mel_high - mel_low) / (N_MELS + 1) as f64;

        (0..N_MELS)
            .map(|filter| {
                let left = mel_low + filter as f64 * mel_delta;
                let center = left + mel_delta;
                let right = center + mel_delta;
                std::array::from_fn(|bin| {
                    let mel = hz_to_mel(bin_width * bin as f64);
                    if mel <= left || mel >= right {
                        0.0
                    } else if mel <= center {
                        (mel - left) / (center - left)
                    } else {
                        (right - mel) / (right - center)
                    }
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_signal() -> Vec<f32> {
        (0..8_000)
            .map(|n| {
                let n = f64::from(n);
                (0.5 * (std::f64::consts::TAU * 440.0 * n / 16_000.0).sin()
                    + 0.25 * (std::f64::consts::TAU * 1_000.0 * n / 16_000.0).sin())
                    as f32
            })
            .collect()
    }

    #[test]
    fn a_window_shorter_than_one_frame_yields_no_features() {
        assert!(fbank(&[0.1; FRAME_LENGTH - 1]).is_empty());
    }

    /// Probe values taken from `torchaudio.compliance.kaldi.fbank` over this
    /// signal, with the arguments WeSpeaker passes and its mean subtraction
    /// applied. Regenerate them the same way if the recipe ever moves.
    #[test]
    fn fbank_matches_torchaudio_kaldi() {
        let features = fbank(&probe_signal());
        assert_eq!(features.len(), 48);
        let probes = [
            (0, 0, -0.871752),
            (0, 1, -0.890355),
            (0, 2, -0.569513),
            (0, 79, 1.732725),
            (1, 0, -0.003779),
            (1, 79, -1.738643),
            (24, 0, -0.490465),
            (24, 79, -2.032721),
            (47, 0, 1.141729),
            (47, 79, 0.874900),
        ];
        for (frame, bin, expected) in probes {
            let actual = features[frame][bin];
            assert!(
                (f64::from(actual) - expected).abs() < 1e-3,
                "frame {frame} bin {bin}: {actual} vs {expected}"
            );
        }
    }

    /// The low filters are the narrow ones, and the shortcut that rounds a
    /// triangle to whole FFT bins leaves several of them empty.
    #[test]
    fn every_mel_filter_covers_some_of_the_spectrum() {
        for (bin, filter) in mel_filterbank().iter().enumerate() {
            let weight: f64 = filter.iter().sum();
            assert!(weight > 0.0, "mel bin {bin} weighs nothing");
        }
    }
}
