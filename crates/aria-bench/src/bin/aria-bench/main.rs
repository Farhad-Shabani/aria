//! One benchmark CLI: each model family is a subcommand.
//!
//! ```text
//! aria-bench stt --corpus build/corpus/manifest.tsv
//! aria-bench tts --kokoro ~/.aria/models/kokoro/kokoro.onnx --kokoro-voices ~/.aria/models/kokoro/voices.bin
//! ```

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
    /// Latency and round-trip word error rate per TTS backend.
    Tts(tts::Cli),
    /// Speaker-label accuracy on synthesized conversations.
    Diarize(diarize::Cli),
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    aria_stt::whisper::install_logging_hooks();
    match Cli::parse() {
        Cli::Stt(cli) => stt::run(&cli),
        Cli::CompareStt(cli) => stt::compare(&cli),
        Cli::Tts(cli) => tts::run(&cli),
        Cli::Diarize(cli) => diarize::run(&cli),
    }
}
