//! The log-mel spectrogram Whisper reads instead of raw audio.
//!
//! candle ships one, but it splits the work across threads, and a browser
//! refuses to spawn them. This is the same algorithm on one thread.

use candle_transformers::models::whisper;

/// Amplitudes this far below the loudest are floor noise, and clamping them
/// keeps a quiet recording from being amplified into hiss.
const DYNAMIC_RANGE: f32 = 8.0;

const TAU: f32 = std::f32::consts::TAU;

pub(super) fn spoken_frames(samples: usize) -> usize {
    samples.div_ceil(whisper::HOP_LENGTH)
}

/// Band-major: every frame of band 0, then every frame of band 1.
pub(super) fn spectrogram(samples: &[f32], filters: &[f32], bands: usize) -> Vec<f32> {
    let frames = padded_frames(spoken_frames(samples.len()));
    let mut mel = vec![0.0; frames * bands];
    if frames == 0 {
        return mel;
    }

    let window = hann(whisper::N_FFT);
    let transform = Transform::plan(whisper::N_FFT);
    let mut windowed = vec![0.0; whisper::N_FFT];
    let mut spectrum = vec![0.0; 2 * whisper::N_FFT];

    for frame in 0..frames {
        let offset = frame * whisper::HOP_LENGTH;
        let heard = whisper::N_FFT.min(samples.len().saturating_sub(offset));
        // Past the audio every frame is the same silence, and the transform of
        // silence is zero, so the tail costs nothing.
        if heard == 0 {
            fill_silent_bands(&mut mel, bands, frames, frame);
            continue;
        }
        for (i, slot) in windowed.iter_mut().enumerate() {
            *slot = if i < heard {
                window[i] * samples[offset + i]
            } else {
                0.0
            };
        }
        transform.run(&windowed, &mut spectrum);
        fold_to_power(&mut spectrum);
        write_bands(&mut mel, &spectrum, filters, bands, frames, frame);
    }

    normalize(&mut mel);
    mel
}

/// Whisper reads fixed thirty-second windows, so the spectrogram runs to the
/// end of the last window the audio reaches.
fn padded_frames(spoken: usize) -> usize {
    spoken.div_ceil(whisper::N_FRAMES) * whisper::N_FRAMES
}

fn hann(size: usize) -> Vec<f32> {
    (0..size)
        .map(|i| 0.5 * (1.0 - (TAU * i as f32 / size as f32).cos()))
        .collect()
}

fn write_bands(
    mel: &mut [f32],
    power: &[f32],
    filters: &[f32],
    bands: usize,
    frames: usize,
    frame: usize,
) {
    let bins = 1 + whisper::N_FFT / 2;
    for band in 0..bands {
        let filter = &filters[band * bins..(band + 1) * bins];
        let energy: f32 = power[..bins]
            .iter()
            .zip(filter)
            .map(|(power, weight)| power * weight)
            .sum();
        mel[band * frames + frame] = energy.max(1e-10).log10();
    }
}

fn fill_silent_bands(mel: &mut [f32], bands: usize, frames: usize, frame: usize) {
    for band in 0..bands {
        mel[band * frames + frame] = 1e-10f32.log10();
    }
}

fn fold_to_power(spectrum: &mut [f32]) {
    let size = spectrum.len() / 2;
    for bin in 0..size {
        let (re, im) = (spectrum[2 * bin], spectrum[2 * bin + 1]);
        spectrum[bin] = re * re + im * im;
    }
    for bin in 1..size / 2 {
        spectrum[bin] += spectrum[size - bin];
    }
}

fn normalize(mel: &mut [f32]) {
    let loudest = mel.iter().copied().fold(f32::MIN, f32::max);
    let floor = loudest - DYNAMIC_RANGE;
    for band in mel.iter_mut() {
        *band = band.max(floor) / 4.0 + 1.0;
    }
}

/// Whisper's 400-sample frame halves down to an odd 25, so the halving is
/// Cooley-Tukey and the leaf is a direct transform.
struct Transform {
    size: usize,
    /// Roots of unity for the whole transform. A halving step of length `n`
    /// reads index `k * size / n`.
    twiddles: Vec<(f32, f32)>,
    /// Roots of unity for the odd transform at the leaves, indexed by the
    /// product of its two loop counters modulo the leaf size.
    leaf_twiddles: Vec<(f32, f32)>,
    leaf: usize,
}

impl Transform {
    fn plan(size: usize) -> Self {
        let leaf = odd_factor(size);
        Self {
            size,
            twiddles: roots(size),
            leaf_twiddles: roots(leaf),
            leaf,
        }
    }

    /// `out` holds two values per input sample: a real part and an imaginary
    /// one.
    fn run(&self, input: &[f32], out: &mut [f32]) {
        self.step(input, 0, 1, self.size, out);
    }

    fn step(&self, input: &[f32], offset: usize, stride: usize, count: usize, out: &mut [f32]) {
        if count == self.leaf {
            self.leaf_transform(input, offset, stride, out);
            return;
        }

        let half = count / 2;
        let (even, odd) = out.split_at_mut(count);
        self.step(input, offset, stride * 2, half, even);
        self.step(input, offset + stride, stride * 2, half, odd);

        let span = self.size / count;
        for k in 0..half {
            let (re, im) = self.twiddles[k * span];
            let (even_re, even_im) = (out[2 * k], out[2 * k + 1]);
            let (odd_re, odd_im) = (out[count + 2 * k], out[count + 2 * k + 1]);
            let (turned_re, turned_im) = (re * odd_re - im * odd_im, re * odd_im + im * odd_re);

            out[2 * k] = even_re + turned_re;
            out[2 * k + 1] = even_im + turned_im;
            out[count + 2 * k] = even_re - turned_re;
            out[count + 2 * k + 1] = even_im - turned_im;
        }
    }

    fn leaf_transform(&self, input: &[f32], offset: usize, stride: usize, out: &mut [f32]) {
        for k in 0..self.leaf {
            let mut re = 0.0;
            let mut im = 0.0;
            for j in 0..self.leaf {
                let (turn_re, turn_im) = self.leaf_twiddles[k * j % self.leaf];
                let sample = input[offset + j * stride];
                re += sample * turn_re;
                im += sample * turn_im;
            }
            out[2 * k] = re;
            out[2 * k + 1] = im;
        }
    }
}

/// `e^(-2*pi*i*k/size)` for every `k`.
fn roots(size: usize) -> Vec<(f32, f32)> {
    (0..size)
        .map(|k| {
            let angle = TAU * k as f32 / size as f32;
            (angle.cos(), -angle.sin())
        })
        .collect()
}

fn odd_factor(mut size: usize) -> usize {
    while size.is_multiple_of(2) {
        size /= 2;
    }
    size
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording that ends mid-window leaves the spectrogram running past
    /// the audio. Only the frames the audio reached may be transcribed.
    #[test]
    fn padding_never_counts_as_something_spoken() {
        for seconds in [1, 15, 20, 30, 45] {
            let samples = seconds * whisper::SAMPLE_RATE;
            let spoken = spoken_frames(samples);
            let padded = padded_frames(spoken);

            assert!(
                spoken <= padded,
                "{seconds}s: {spoken} spoken frames exceeds {padded} padded"
            );
            assert_eq!(
                padded % whisper::N_FRAMES,
                0,
                "{seconds}s does not pad to whole windows"
            );
        }
    }

    #[test]
    fn an_empty_recording_has_nothing_to_decode() {
        assert_eq!(spoken_frames(0), 0);
        assert!(spectrogram(&[], &[], 80).is_empty());
    }

    #[test]
    fn it_matches_candle_frame_for_frame() {
        let filters = super::super::decode_mel_filters();
        let samples: Vec<f32> = (0..16_000).map(|i| (i as f32 * 0.05).sin() * 0.4).collect();
        let config = whisper::Config {
            num_mel_bins: 80,
            max_source_positions: 1500,
            d_model: 384,
            encoder_attention_heads: 6,
            encoder_layers: 4,
            vocab_size: 51864,
            max_target_positions: 448,
            decoder_attention_heads: 6,
            decoder_layers: 4,
            suppress_tokens: Vec::new(),
        };

        let ours = spectrogram(&samples, &filters, 80);
        let theirs = whisper::audio::pcm_to_mel(&config, &samples, &filters);

        assert_eq!(ours.len(), theirs.len());
        // The tolerance is rounding, not agreement to the bit: candle takes the
        // cosine of an angle that grows with its loop counters, which an `f32`
        // resolves poorly.
        for (ours, theirs) in ours.iter().zip(&theirs) {
            assert!(
                (ours - theirs).abs() < 1e-3,
                "{ours} and {theirs} disagree beyond rounding"
            );
        }
    }
}
