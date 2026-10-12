//! Mel-spectrogram front end for the speaker encoder: a radix-2 FFT STFT
//! with a Hann window and the librosa-compatible Slaney mel filterbank
//! (128 bands, 24 kHz, fmax 12 kHz), then log compression.

use candle_core::{Device, Tensor};

const N_FFT: usize = 1024;
const HOP: usize = 256;
const WINDOW: usize = 1024;
const PAD: usize = (N_FFT - HOP) / 2;
const N_MELS: usize = 128;
const SAMPLE_RATE: f64 = 24_000.0;
const F_MAX: f64 = 12_000.0;
/// Magnitude floor before the square root.
const MAG_FLOOR: f32 = 1e-9;
/// Log compression floor.
const LOG_CLAMP: f32 = 1e-5;
/// ln(6.4) / 27 — the librosa Slaney log-step.
const LOGSTEP_VALUE: f64 = 0.06875177742057906;

#[derive(Debug, thiserror::Error)]
#[error("qwen: mel spectrogram failed: {0}")]
pub struct MelError(pub String);

pub type Result<T> = std::result::Result<T, MelError>;

/// The Slaney-scale Hz↔Mel pair librosa uses when `htk=False`.
fn hz_to_mel(frequency: f64) -> f64 {
    const F_SP: f64 = 200.0 / 3.0;
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = MIN_LOG_HZ / F_SP;
    const LOGSTEP: f64 = LOGSTEP_VALUE;
    if frequency < MIN_LOG_HZ {
        frequency / F_SP
    } else {
        MIN_LOG_MEL + (frequency / MIN_LOG_HZ).ln() / LOGSTEP
    }
}

fn mel_to_hz(mel: f64) -> f64 {
    const F_SP: f64 = 200.0 / 3.0;
    const MIN_LOG_HZ: f64 = 1000.0;
    const MIN_LOG_MEL: f64 = MIN_LOG_HZ / F_SP;
    const LOGSTEP: f64 = LOGSTEP_VALUE;
    if mel < MIN_LOG_MEL {
        F_SP * mel
    } else {
        MIN_LOG_HZ * (LOGSTEP * (mel - MIN_LOG_MEL)).exp()
    }
}

fn mel_points(n: usize) -> Vec<f64> {
    let low = hz_to_mel(0.0);
    let high = hz_to_mel(F_MAX);
    (0..n)
        .map(|i| low + (high - low) * i as f64 / (n - 1) as f64)
        .map(mel_to_hz)
        .collect()
}

/// The Slaney-normalized triangular filterbank, `[N_MELS, N_FFT/2 + 1]`.
fn filterbank() -> Vec<Vec<f32>> {
    let bins = N_FFT / 2 + 1;
    let fft_freqs: Vec<f64> = (0..bins)
        .map(|b| SAMPLE_RATE * b as f64 / N_FFT as f64)
        .collect();
    let points = mel_points(N_MELS + 2);

    let mut bank = vec![vec![0f32; bins]; N_MELS];
    for i in 0..N_MELS {
        let fdiff_low = points[i + 1] - points[i];
        let fdiff_high = points[i + 2] - points[i + 1];
        // Slaney normalization divides each triangle by its bandwidth.
        let enorm = 2.0 / (points[i + 2] - points[i]);
        for (b, &freq) in fft_freqs.iter().enumerate() {
            let lower = (freq - points[i]) / fdiff_low;
            let upper = (points[i + 2] - freq) / fdiff_high;
            bank[i][b] = lower.min(upper).max(0.0) as f32 * enorm as f32;
        }
    }
    bank
}

fn hann_window() -> Vec<f32> {
    // torch.hann_window defaults to the periodic form.
    (0..WINDOW)
        .map(|n| 0.5 * (1.0 - (std::f64::consts::TAU * n as f64 / WINDOW as f64).cos()) as f32)
        .collect()
}

/// Reflect-pads by `pad` samples on both ends without repeating the edge
/// samples, matching `F.pad(..., mode="reflect")`.
fn reflect_pad(samples: &[f32], pad: usize) -> Result<Vec<f32>> {
    let n = samples.len();
    if n < 2 {
        return Err(MelError("input too short to reflect-pad".to_owned()));
    }
    if pad == 0 {
        return Ok(samples.to_vec());
    }
    let mut out = Vec::with_capacity(n + 2 * pad);
    for i in (1..=pad).rev() {
        out.push(samples[i.min(n - 1)]);
    }
    out.extend_from_slice(samples);
    for i in 0..pad {
        out.push(samples[n - 2 - i]);
    }
    Ok(out)
}

/// In-place iterative radix-2 complex FFT; length must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());
    let bits = n.trailing_zeros();
    for i in 0..n {
        let j = (i.reverse_bits()) >> (usize::BITS - bits);
        if j > i {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut length = 2;
    while length <= n {
        let half = length / 2;
        let step = -std::f64::consts::TAU / length as f64;
        for start in (0..n).step_by(length) {
            for offset in 0..half {
                let angle = step * offset as f64;
                let (cos, sin) = (angle.cos() as f32, angle.sin() as f32);
                let even = start + offset;
                let odd = even + half;
                let tr = re[odd] * cos - im[odd] * sin;
                let ti = re[odd] * sin + im[odd] * cos;
                re[odd] = re[even] - tr;
                im[odd] = im[even] - ti;
                re[even] += tr;
                im[even] += ti;
            }
        }
        length *= 2;
    }
}

/// The log-mel spectrogram `[1, N_MELS, frames]` of one utterance.
pub fn log_mel(samples: &[f32], device: &Device) -> Result<Tensor> {
    let padded = reflect_pad(samples, PAD)?;
    let frames = (padded.len() - N_FFT) / HOP + 1;
    let window = hann_window();

    let bins = N_FFT / 2 + 1;
    let bank = filterbank();
    let mut magnitudes = vec![0f32; N_MELS * frames];

    let mut re = vec![0f32; N_FFT];
    let mut im = vec![0f32; N_FFT];
    for frame in 0..frames {
        let start = frame * HOP;
        for (i, value) in re.iter_mut().enumerate().take(N_FFT) {
            *value = padded[start + i] * window[i];
        }
        im.iter_mut().for_each(|v| *v = 0.0);
        fft(&mut re, &mut im);

        for b in 0..bins {
            let power = re[b] * re[b] + im[b] * im[b];
            let magnitude = (power as f32 + MAG_FLOOR).sqrt();
            for m in 0..N_MELS {
                magnitudes[m * frames + frame] += bank[m][b] * magnitude;
            }
        }
    }

    // Log compression over the filtered energies.
    for value in &mut magnitudes {
        *value = (*value).max(LOG_CLAMP).ln();
    }

    Tensor::from_vec(magnitudes, (1, N_MELS, frames), device).map_err(|e| MelError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_peaks_at_the_sine_bin() {
        const N: usize = 1024;
        const K: usize = 100;
        let mut re: Vec<f32> = (0..N)
            .map(|n| (std::f64::consts::TAU * K as f64 * n as f64 / N as f64).cos() as f32)
            .collect();
        let mut im = vec![0f32; N];
        fft(&mut re, &mut im);
        let mags: Vec<f32> = (0..N / 2 + 1)
            .map(|b| (re[b] * re[b] + im[b] * im[b]).sqrt())
            .collect();
        let best = (0..N / 2 + 1)
            .max_by(|a, b| mags[*a].total_cmp(&mags[*b]))
            .unwrap();
        assert_eq!(best, K);
        assert!(mags[K] > N as f32 * 0.4);
    }

    #[test]
    fn dc_bin_is_the_sum() {
        let mut re = vec![1f32; 64];
        let mut im = vec![0f32; 64];
        fft(&mut re, &mut im);
        assert!((im[0]).abs() < 1e-4);
    }
}

#[cfg(test)]
mod filterbank_tests {
    use super::*;

    #[test]
    fn filterbank_covers_the_spectrum() {
        let bank = filterbank();
        // Every band must have mass somewhere in the spectrum.
        for (i, row) in bank.iter().enumerate() {
            let mass: f32 = row.iter().sum();
            assert!(mass > 0.0, "band {i} empty");
        }
        // First band peaks near DC.
        let peak = bank[0]
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert!(peak < 8, "first band peaks at bin {peak}");
    }
}
