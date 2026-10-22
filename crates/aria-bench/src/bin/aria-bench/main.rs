//! One benchmark CLI: each model family is a subcommand.
//!
//! ```text
//! aria-bench stt --corpus build/corpus/manifest.tsv
//! aria-bench tts --kokoro ~/.aria/models/kokoro/kokoro.onnx --kokoro-voices ~/.aria/models/kokoro/voices.bin
//! ```

mod corpus;
mod diarize;
mod flag_path;
mod stt;
mod tts;

use clap::Parser;

#[derive(Parser)]
#[command(name = "aria-bench", about = "Benchmarks per model family")]
enum Cli {
    /// Word error rate per transcription backend.
    Stt(stt::Cli),
    /// Analyze recorded STT results without loading a model.
    CompareStt(stt::CompareCli),
    /// Check an STT run and save it as the committed reference.
    ReferenceStt(stt::ReferenceCli),
    /// Build the LibriSpeech corpus the accuracy recipes measure.
    Corpus(corpus::CorpusCli),
    /// Fetch the DEMAND room noise `noisy` mixes in.
    Noise(corpus::NoiseCli),
    /// A copy of a corpus under room noise at a fixed signal-to-noise ratio.
    Noisy(corpus::NoisyCli),
    /// Latency and round-trip word error rate per TTS backend.
    Tts(tts::Cli),
    /// Speaker-label accuracy on synthesized conversations.
    Diarize(diarize::Cli),
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,aria_bench=info")),
        )
        .init();
    aria_stt::whisper::install_logging_hooks();
    match Cli::parse() {
        Cli::Stt(cli) => stt::run(&cli),
        Cli::CompareStt(cli) => stt::compare(&cli),
        Cli::ReferenceStt(cli) => stt::reference(&cli),
        Cli::Corpus(cli) => corpus::corpus(&cli),
        Cli::Noise(cli) => corpus::noise(&cli),
        Cli::Noisy(cli) => corpus::noisy(&cli),
        Cli::Tts(cli) => tts::run(&cli),
        Cli::Diarize(cli) => diarize::run(&cli),
    }
}
