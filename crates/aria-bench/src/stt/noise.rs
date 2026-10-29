//! A corpus under room noise at a fixed signal-to-noise ratio.
//!
//! Each utterance gets one recording of real noise mixed in. The references
//! are unchanged, so every extra error is the engine's. Which noise an
//! utterance gets depends only on its position in the manifest, so a corpus
//! built twice is the same.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use aria_kit::audio::{Clip, WavError, canonical_samples, read_wav, write_wav};

use super::corpus::{Built, Corpus, CorpusError, write_manifest};

const OPENING_STEP: u64 = canonical_samples(Duration::from_secs(30));

pub struct NoisyRequest {
    /// The clean corpus manifest.
    pub corpus: PathBuf,
    /// A directory of noise recordings, any WAV format.
    pub noise: PathBuf,
    /// Speech power over noise power, in decibels.
    pub snr_db: f64,
    /// Where `wav/` and `manifest.tsv` are written.
    pub output: PathBuf,
}

pub fn build(request: &NoisyRequest) -> Result<Built, NoiseError> {
    let corpus = Corpus::load(&request.corpus)?;
    let noises = load_noises(&request.noise)?;
    let output = std::path::absolute(&request.output).map_err(create_error(&request.output))?;
    if output.exists() {
        return Err(NoiseError::Exists { path: output });
    }
    // Built beside `output` and moved into place whole, so a failed build
    // leaves nothing a rerun would refuse to overwrite.
    let parent = output.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent).map_err(create_error(parent))?;
    let staging = tempfile::tempdir_in(parent).map_err(create_error(parent))?;
    let audio_dir = staging.path().join("wav");
    fs::create_dir_all(&audio_dir).map_err(create_error(&audio_dir))?;

    let mut utterances = Vec::with_capacity(corpus.utterances.len());
    for (index, utterance) in corpus.utterances.iter().enumerate() {
        let name = format!("{}.wav", utterance.id);
        let speech = read_wav(&utterance.audio)?.into_canonical()?;
        let window = noises[index % noises.len()].window(index, speech.samples().len())?;
        let mixed = mix(speech.samples(), &window, request.snr_db);
        write_wav(audio_dir.join(&name), &Clip::canonical(mixed))?;
        utterances.push((output.join("wav").join(name), utterance.reference.as_str()));
    }
    let mut built = write_manifest(
        &staging.path().join("manifest.tsv"),
        utterances
            .iter()
            .map(|(wav, reference)| (wav.as_path(), *reference)),
    )?;
    fs::rename(staging.keep(), &output).map_err(create_error(&output))?;
    built.manifest = output.join("manifest.tsv");
    Ok(built)
}

fn create_error(path: &Path) -> impl FnOnce(std::io::Error) -> NoiseError + '_ {
    move |source| NoiseError::Create {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn load_noises(directory: &Path) -> Result<Vec<Noise>, NoiseError> {
    let mut paths: Vec<PathBuf> = fs::read_dir(directory)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect()
        })
        .map_err(|source| NoiseError::ListNoise {
            path: directory.to_path_buf(),
            source,
        })?;
    paths.retain(|path| path.extension().is_some_and(|extension| extension == "wav"));
    paths.sort();
    if paths.is_empty() {
        return Err(NoiseError::NoNoise {
            path: directory.to_path_buf(),
        });
    }
    paths
        .into_iter()
        .map(|path| {
            let samples = read_wav(&path)?.into_canonical()?.into_samples();
            if power(&samples) == 0.0 {
                return Err(NoiseError::SilentNoise { path });
            }
            Ok(Noise { path, samples })
        })
        .collect()
}

pub(crate) struct Noise {
    path: PathBuf,
    samples: Vec<f32>,
}

impl Noise {
    /// `length` samples of the noise for the `turn`th utterance on it.
    /// Silent noise has no level to scale to a ratio.
    pub(crate) fn window(&self, turn: usize, length: usize) -> Result<Vec<f32>, NoiseError> {
        let window: Vec<f32> = self.cycle_from(turn).take(length).collect();
        if power(&window) == 0.0 {
            return Err(NoiseError::SilentNoise {
                path: self.path.clone(),
            });
        }
        Ok(window)
    }

    /// Each later utterance on a noise opens it a step further in, wrapping
    /// past the end, so neighbours on it do not share an opening.
    fn cycle_from(&self, turn: usize) -> impl Iterator<Item = f32> + '_ {
        let start = (turn * OPENING_STEP as usize) % self.samples.len();
        self.samples.iter().copied().cycle().skip(start)
    }
}

/// `noise` must not be silent. A sum that would clip is scaled down whole,
/// which keeps the ratio.
fn mix(speech: &[f32], noise: &[f32], snr_db: f64) -> Vec<f32> {
    let mut mixed = speech[..speech.len().min(noise.len())].to_vec();
    mix_into(&mut [&mut mixed], noise, snr_db);
    mixed
}

/// Adds `noise` to every track at `snr_db` against the first. A sum that
/// would clip scales every track down by the same factor, so tracks that
/// share a clock stay comparable.
pub(crate) fn mix_into(tracks: &mut [&mut Vec<f32>], noise: &[f32], snr_db: f64) {
    let gain = gain_for_snr(power(tracks[0]), power(noise), snr_db);
    let mut peak = 0f32;
    for track in tracks.iter_mut() {
        for (sample, noise) in track.iter_mut().zip(noise) {
            *sample += gain * noise;
            peak = peak.max(sample.abs());
        }
    }
    if peak > 1.0 {
        for track in tracks.iter_mut() {
            track.iter_mut().for_each(|sample| *sample /= peak);
        }
    }
}

/// torchaudio's `add_noise`: the noise gain that puts the mix at `snr_db`.
fn gain_for_snr(speech_power: f64, noise_power: f64, snr_db: f64) -> f32 {
    (speech_power / noise_power * 10f64.powf(-snr_db / 10.0)).sqrt() as f32
}

fn power(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples
        .iter()
        .map(|&sample| f64::from(sample) * f64::from(sample))
        .sum::<f64>()
        / samples.len() as f64
}

#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    #[error(transparent)]
    Corpus(#[from] CorpusError),

    #[error(transparent)]
    Wav(#[from] WavError),

    #[error(transparent)]
    Resample(#[from] aria_kit::audio::ResampleError),

    #[error("failed to list the noise recordings in {}", path.display())]
    ListNoise {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{} holds no WAV recordings", path.display())]
    NoNoise { path: PathBuf },

    #[error("{} is empty or silent where it was drawn; it cannot be scaled to any SNR", path.display())]
    SilentNoise { path: PathBuf },

    #[error("{} already exists; the noisy corpus is not overwritten", path.display())]
    Exists { path: PathBuf },

    #[error("failed to create {}", path.display())]
    Create {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::f32::consts::TAU;
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    fn tone(amplitude: f32, samples: usize) -> Vec<f32> {
        (0..samples)
            .map(|index| amplitude * (TAU * index as f32 / 40.0).sin())
            .collect()
    }

    fn measured_snr_db(speech: &[f32], mixed: &[f32]) -> f64 {
        let added: Vec<f32> = mixed
            .iter()
            .zip(speech)
            .map(|(mixed, speech)| mixed - speech)
            .collect();
        10.0 * (power(speech) / power(&added)).log10()
    }

    #[test]
    fn the_mix_lands_on_the_requested_snr() {
        let speech = tone(0.3, 4_000);
        let noise = tone(0.01, 4_000);
        for snr_db in [20.0, 10.0, 0.0, -5.0] {
            let mixed = mix(&speech, &noise, snr_db);
            assert!(
                (measured_snr_db(&speech, &mixed) - snr_db).abs() < 0.01,
                "{snr_db} dB"
            );
        }
    }

    #[test]
    fn later_turns_open_the_noise_further_in() {
        let step = OPENING_STEP as usize;
        let noise = Noise {
            path: PathBuf::new(),
            samples: (0..3 * step).map(|index| index as f32).collect(),
        };
        assert_eq!(noise.cycle_from(0).next(), Some(0.0));
        assert_eq!(noise.cycle_from(1).next(), Some(step as f32));
        assert_eq!(noise.cycle_from(3).next(), Some(0.0));
    }

    #[test]
    fn a_built_corpus_pairs_each_utterance_with_a_noise_in_turn() {
        let root = TempDir::new().expect("a temp dir");
        let clean = root.path().join("clean");
        let noise = root.path().join("noise");
        fs::create_dir_all(&clean).unwrap();
        fs::create_dir_all(&noise).unwrap();
        let mut manifest = fs::File::create(clean.join("manifest.tsv")).unwrap();
        for name in ["one", "two", "three"] {
            let path = clean.join(format!("{name}.wav"));
            write_wav(&path, &Clip::canonical(tone(0.5, 16_000))).unwrap();
            writeln!(manifest, "{}\tsaid {name}", path.display()).unwrap();
        }
        // A sine and a square wave, told apart by shape once scaled.
        write_wav(noise.join("a.wav"), &Clip::canonical(tone(0.2, 8_000))).unwrap();
        let square: Vec<f32> = (0..8_000)
            .map(|index| if index % 2 == 0 { 0.3 } else { -0.3 })
            .collect();
        write_wav(noise.join("b.wav"), &Clip::canonical(square)).unwrap();

        let built = build(&NoisyRequest {
            corpus: clean.join("manifest.tsv"),
            noise,
            snr_db: 10.0,
            output: root.path().join("noisy"),
        })
        .expect("the noisy corpus should build");

        assert_eq!(built.utterances, 3);
        let manifest = fs::read_to_string(&built.manifest).unwrap();
        let lines: Vec<&str> = manifest.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].ends_with("/two.wav\tsaid two"), "{}", lines[1]);
        let speech = tone(0.5, 16_000);
        let added: Vec<Vec<f32>> = lines
            .iter()
            .map(|line| {
                let (audio, _reference) = line.split_once('\t').unwrap();
                let mixed = read_wav(audio).unwrap().into_samples();
                let snr = measured_snr_db(&speech, &mixed);
                assert!((snr - 10.0).abs() < 0.1, "{audio}: {snr} dB");
                mixed.iter().zip(&speech).map(|(m, s)| m - s).collect()
            })
            .collect();
        let is_sine = |noise: &[f32]| noise[0].abs() < 1e-3 && noise[10] > 0.01;
        let is_square = |noise: &[f32]| noise[0] > 0.01 && (noise[0] + noise[1]).abs() < 1e-3;
        assert!(is_sine(&added[0]), "one takes a.wav");
        assert!(is_square(&added[1]), "two takes b.wav");
        assert!(is_sine(&added[2]), "three wraps back to a.wav");

        let again = build(&NoisyRequest {
            corpus: clean.join("manifest.tsv"),
            noise: root.path().join("noise"),
            snr_db: 10.0,
            output: root.path().join("noisy"),
        });
        assert!(
            matches!(again, Err(NoiseError::Exists { .. })),
            "a second build must not overwrite"
        );
    }

    #[test]
    fn a_failed_build_leaves_no_output_for_a_rerun_to_refuse() {
        let root = TempDir::new().expect("a temp dir");
        let clean = root.path().join("clean");
        let noise = root.path().join("noise");
        fs::create_dir_all(&clean).unwrap();
        fs::create_dir_all(&noise).unwrap();
        let good = clean.join("one.wav");
        write_wav(&good, &Clip::canonical(tone(0.5, 16_000))).unwrap();
        write_wav(noise.join("a.wav"), &Clip::canonical(tone(0.2, 8_000))).unwrap();
        let manifest = format!(
            "{}\tsaid one\n{}\tsaid two\n",
            good.display(),
            clean.join("missing.wav").display()
        );
        fs::write(clean.join("manifest.tsv"), manifest).unwrap();
        let output = root.path().join("noisy");

        let built = build(&NoisyRequest {
            corpus: clean.join("manifest.tsv"),
            noise,
            snr_db: 10.0,
            output: output.clone(),
        });

        assert!(built.is_err());
        assert!(!output.exists(), "a failed build leaves nothing behind");
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            2,
            "no staging dir"
        );
    }

    #[test]
    fn a_mix_that_would_clip_is_brought_down_to_full_scale() {
        let speech = tone(0.9, 16_000);
        let noise: Vec<f32> = (0..16_000)
            .map(|index| if index % 2 == 0 { 0.9 } else { -0.9 })
            .collect();

        let mixed = mix(&speech, &noise, 0.0);

        let peak = mixed
            .iter()
            .fold(0f32, |peak, sample| peak.max(sample.abs()));
        assert!((peak - 1.0).abs() < 1e-6, "peak {peak}");
        assert!(power(&mixed) > 0.3, "still full scale, not merely quiet");
    }

    #[test]
    fn silent_noise_is_refused() {
        let root = TempDir::new().expect("a temp dir");
        write_wav(
            root.path().join("flat.wav"),
            &Clip::canonical(vec![0.0; 100]),
        )
        .unwrap();
        let error = load_noises(root.path()).err().expect("silence is an error");
        assert!(matches!(error, NoiseError::SilentNoise { .. }), "{error}");
    }
}
