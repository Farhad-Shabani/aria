//! Parakeet decode-window benchmark.
//!
//! Times one `transcribe_samples` call over increasing prefixes of a fixture,
//! which is what a cancelled press has to wait out: parakeet exposes no abort
//! hook, so cancellation is only noticed when the current window finishes.
//! Run with:
//!
//! ```text
//! ARIA_TEST_PARAKEET_MODEL=~/.aria/models/parakeet/parakeet-tdt-0.6b-v3 \
//!   ARIA_TEST_PARAKEET_WAV=~/recordings/parakeet-window.wav \
//!   cargo bench -p aria-stt --features parakeet --bench parakeet_decode
//! ```
//!
//! `ARIA_BENCH_RUNS` overrides the run count. Missing inputs skip the bench
//! so `cargo bench` stays green on machines without fixtures.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use aria_harness::{inputs, stats};
use aria_kit::audio::{CANONICAL_SAMPLE_RATE, CHANNELS, read_wav};
use parakeet_rs::{ParakeetTDT, Transcriber};

const BENCH: &str = "parakeet decode";
const PARAKEET_MODEL_ENV: &str = "ARIA_TEST_PARAKEET_MODEL";
const WAV_ENV: &str = "ARIA_TEST_PARAKEET_WAV";
const DEFAULT_RUNS: u32 = 3;

const WINDOW_SECONDS: &[usize] = &[15, 30, 60, 120];

fn main() -> anyhow::Result<()> {
    let Some(inputs) = BenchInputs::from_environment()? else {
        return Ok(());
    };

    let samples = read_wav(&inputs.wav)
        .with_context(|| format!("failed to read {}", inputs.wav.display()))?
        .into_canonical()?
        .into_samples();
    let available = samples.len() / CANONICAL_SAMPLE_RATE as usize;
    println!("model {}", inputs.model.display());
    println!("audio {} ({available}s)", inputs.wav.display());

    let load_started = Instant::now();
    let mut parakeet = ParakeetTDT::from_pretrained(&inputs.model, None)
        .map_err(|error| anyhow::anyhow!("failed to load the parakeet model: {error}"))?;
    println!("load  {:?}\n", load_started.elapsed());

    println!(
        "{:>7}  {:>10}  {:>10}  {:>9}",
        "window", "median", "max", "realtime"
    );
    for &seconds in WINDOW_SECONDS {
        if seconds > available {
            println!("{seconds:>6}s  skipped, the fixture is only {available}s");
            continue;
        }
        let window = &samples[..seconds * CANONICAL_SAMPLE_RATE as usize];
        let elapsed = measure(&mut parakeet, window, inputs.runs)?;
        let Some(summary) = stats::summarize(&elapsed) else {
            bail!("no measurements for the {seconds}s window");
        };
        let median = Duration::from_millis(summary.median);
        let max = Duration::from_millis(summary.max);
        let realtime = seconds as f64 / median.as_secs_f64();
        println!("{seconds:>6}s  {median:>10.2?}  {max:>10.2?}  {realtime:>8.1}x");
    }
    Ok(())
}

struct BenchInputs {
    model: PathBuf,
    wav: PathBuf,
    runs: u32,
}

impl BenchInputs {
    fn from_environment() -> anyhow::Result<Option<Self>> {
        let (Some(model), Some(wav)) = (
            inputs::fixture(BENCH, PARAKEET_MODEL_ENV)?,
            inputs::fixture(BENCH, WAV_ENV)?,
        ) else {
            return Ok(None);
        };
        Ok(Some(Self {
            model,
            wav,
            runs: inputs::run_count(DEFAULT_RUNS)?,
        }))
    }
}

fn measure(parakeet: &mut ParakeetTDT, window: &[f32], runs: u32) -> anyhow::Result<Vec<u64>> {
    (0..runs)
        .map(|_run| {
            let started = Instant::now();
            parakeet
                .transcribe_samples(window.to_vec(), CANONICAL_SAMPLE_RATE, CHANNELS, None)
                .map_err(|error| anyhow::anyhow!("parakeet inference failed: {error}"))?;
            Ok(started.elapsed().as_millis() as u64)
        })
        .collect()
}
