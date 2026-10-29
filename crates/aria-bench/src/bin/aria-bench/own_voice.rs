//! The `own-voice-corpus` subcommand: dictation-shaped conversations read by
//! LibriSpeech speakers, one enrolled owner each, for replaying through the
//! own-voice filter.

use std::num::NonZeroUsize;

use anyhow::Context;
use aria_bench::own_voice::scores::{self, measure};
use aria_bench::own_voice::{Noisy, Request, write_corpus};
use aria_bench::stt::librispeech::Subset;
use clap::Parser;

use crate::flag_path::expand;

const EXAMPLES: &str = "\
Examples:
  aria-bench own-voice-corpus --out build/own-voice
  aria-bench own-voice-corpus --out build/own-voice-10db --noise build/noise --snr 10
";

#[derive(Parser)]
#[command(
    about = "Conversations for the own-voice filter, read by LibriSpeech speakers",
    after_help = EXAMPLES
)]
pub(crate) struct Cli {
    /// LibriSpeech cache; test-clean is downloaded here when missing
    #[arg(long, default_value = "build/corpus/librispeech")]
    librispeech: String,
    /// wespeaker speaker-embedding ONNX model
    #[arg(long, default_value = "~/.aria/models/wespeaker/en/model.onnx")]
    speaker_model: String,
    /// Empty directory to write one folder per owner and `voices.json` into
    #[arg(long)]
    out: String,
    /// How many speakers take a turn as the owner, alternating female and male
    #[arg(long, default_value = "4")]
    owners: NonZeroUsize,
    /// Directory of noise recordings to mix into every scenario
    #[arg(long, requires = "snr")]
    noise: Option<String>,
    /// Speech power over noise power, in decibels
    #[arg(long, requires = "noise")]
    snr: Option<f64>,
}

pub(crate) fn run(cli: &Cli) -> anyhow::Result<()> {
    let out = expand(&cli.out)?;
    std::fs::create_dir_all(&out).context("creating the corpus directory")?;
    let noise = match (&cli.noise, cli.snr) {
        (Some(recordings), Some(snr_db)) => Some(Noisy {
            recordings: expand(recordings)?,
            snr_db,
        }),
        _ => None,
    };
    let request = Request {
        librispeech: expand(&cli.librispeech)?,
        speaker_model: expand(&cli.speaker_model)?,
        owners: cli.owners.get(),
        noise,
    };
    let owners = write_corpus(&request, &out)?;
    let owners: Vec<String> = owners.iter().map(u32::to_string).collect();
    println!("wrote owners {} to {}", owners.join(", "), out.display());
    Ok(())
}

#[derive(Parser)]
#[command(
    about = "How far apart an owner's voice and others score, by window length",
    after_help = "Examples:\n  aria-bench own-voice-scores --out build/own-voice-scores-dev.json\n  aria-bench own-voice-scores --subset test-clean --out build/own-voice-scores-test.json\n"
)]
pub(crate) struct ScoresCli {
    /// Subsets whose speakers are enrolled and heard
    #[arg(long = "subset", value_enum, default_values_t = [Subset::DevClean, Subset::DevOther])]
    subsets: Vec<Subset>,
    /// LibriSpeech cache; each subset is downloaded here when missing
    #[arg(long, default_value = "build/corpus/librispeech")]
    librispeech: String,
    /// wespeaker speaker-embedding ONNX model
    #[arg(long, default_value = "~/.aria/models/wespeaker/en/model.onnx")]
    speaker_model: String,
    /// The summary JSON
    #[arg(long)]
    out: String,
}

pub(crate) fn run_scores(cli: &ScoresCli) -> anyhow::Result<()> {
    let librispeech = expand(&cli.librispeech)?;
    std::fs::create_dir_all(&librispeech).context("creating the LibriSpeech cache")?;
    measure(&scores::Request {
        librispeech,
        subsets: cli.subsets.clone(),
        speaker_model: expand(&cli.speaker_model)?,
        out: expand(&cli.out)?,
    })?;
    Ok(())
}
