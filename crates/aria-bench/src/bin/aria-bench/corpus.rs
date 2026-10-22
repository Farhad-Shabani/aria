//! Builds the corpora the speech benchmarks measure.
//!
//! ```text
//! aria-bench corpus build/corpus --utterances 100
//! aria-bench noise build/noise
//! aria-bench noisy --corpus build/corpus-long/manifest.tsv \
//!     --noise build/noise --snr 10 --output build/corpus-noisy-10db
//! ```

use std::num::NonZeroUsize;
use std::path::PathBuf;

use aria_bench::stt::noise::NoisyRequest;
use aria_bench::stt::{Built, demand, librispeech, noise};
use clap::Parser;

#[derive(Parser)]
pub(crate) struct CorpusCli {
    /// Where `wav/` and `manifest.tsv` go.
    output: PathBuf,
    #[arg(long, default_value = "100")]
    utterances: NonZeroUsize,
}

pub(crate) fn corpus(cli: &CorpusCli) -> anyhow::Result<()> {
    report(&librispeech::build(&cli.output, cli.utterances)?);
    Ok(())
}

#[derive(Parser)]
pub(crate) struct NoiseCli {
    /// Where the recordings go, one WAV per environment.
    output: PathBuf,
}

pub(crate) fn noise(cli: &NoiseCli) -> anyhow::Result<()> {
    let fetched = demand::fetch(&cli.output)?;
    println!(
        "{} noise recordings -> {}",
        fetched.recordings,
        fetched.directory.display()
    );
    Ok(())
}

#[derive(Parser)]
pub(crate) struct NoisyCli {
    /// Manifest of the clean corpus to copy.
    #[arg(long)]
    corpus: PathBuf,

    /// Directory of noise recordings, as `aria-bench noise` leaves them.
    #[arg(long)]
    noise: PathBuf,

    /// Speech power over noise power, in decibels; 20 is a quiet office, 0
    /// is noise as loud as the speech.
    #[arg(long, allow_negative_numbers = true)]
    snr: f64,

    /// Where the noisy WAVs and their manifest go.
    #[arg(long)]
    output: PathBuf,
}

pub(crate) fn noisy(cli: &NoisyCli) -> anyhow::Result<()> {
    report(&noise::build(&NoisyRequest {
        corpus: cli.corpus.clone(),
        noise: cli.noise.clone(),
        snr_db: cli.snr,
        output: cli.output.clone(),
    })?);
    Ok(())
}

fn report(built: &Built) {
    println!(
        "{} utterances, {} reference words -> {}",
        built.utterances,
        built.words,
        built.manifest.display()
    );
}
