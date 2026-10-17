//! Measures every configured TTS backend over one corpus.
//!
//! ```text
//! aria-bench tts \
//!     --qwen ~/.aria/models/qwen/qwen3-tts-0.6b-customvoice \
//!     --qwen-tokenizer ~/.aria/models/qwen/qwen3-tts-tokenizer \
//!     --asr-parakeet ~/.aria/models/parakeet/parakeet-tdt-0.6b-v3
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use aria_bench::tts::{AsrRequest, BackendSummary, EngineRequest, Report};
use aria_kit::primitives::error_chain;
use aria_kit::synthesis::{SpeakingRate, Voice};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Latency and round-trip word error rate per TTS backend")]
pub(crate) struct Cli {
    /// Text to speak, one utterance per line; omitted speaks the built-in
    /// corpus.
    #[arg(long)]
    corpus: Option<PathBuf>,

    /// Where to write one result row per attempt, as newline-delimited JSON.
    #[arg(long, default_value = "bench-results.jsonl")]
    results: PathBuf,

    /// Path to the qwen talker model directory.
    #[arg(long, requires = "qwen_tokenizer")]
    qwen: Option<PathBuf>,

    /// Path to the qwen speech-tokenizer model directory.
    #[arg(long, requires = "qwen")]
    qwen_tokenizer: Option<PathBuf>,

    /// Path to the kokoro ONNX model file.
    #[arg(long, requires = "kokoro_voices")]
    kokoro: Option<PathBuf>,

    /// Path to the kokoro voice styles file.
    #[arg(long, requires = "kokoro")]
    kokoro_voices: Option<PathBuf>,

    /// The voice every backend is asked for; engines without it refuse.
    #[arg(long)]
    voice: Option<String>,

    /// Speaking-rate multiplier; 1.0 is an engine's natural pace.
    #[arg(long, default_value = "1.0")]
    rate: f32,

    /// How many times every utterance is spoken for latency.
    #[arg(long, default_value = "3")]
    runs: usize,

    /// Transcribes what was synthesized back into text for scoring.
    #[arg(long)]
    asr_parakeet: Option<PathBuf>,

    /// Runs the judging parakeet on the GPU rather than the CPU.
    #[arg(long, requires = "asr_parakeet")]
    asr_parakeet_gpu: bool,

    /// Judges with a whisper.candle safetensors directory instead.
    #[arg(long, conflicts_with = "asr_parakeet")]
    asr_whisper_candle: Option<PathBuf>,

    /// Fail the run when any backend's median real-time factor exceeds this.
    #[arg(long)]
    max_rtf: Option<f64>,

    /// Fail the run when any backend's round-trip WER exceeds this fraction.
    #[arg(long)]
    max_wer: Option<f64>,
}

impl Cli {
    fn backends(&self) -> Vec<EngineRequest> {
        let mut backends = Vec::new();
        if let (Some(model_dir), Some(tokenizer_dir)) = (&self.qwen, &self.qwen_tokenizer) {
            backends.push(EngineRequest::Qwen {
                model_dir: model_dir.clone(),
                tokenizer_dir: tokenizer_dir.clone(),
            });
        }
        if let (Some(model), Some(voices)) = (&self.kokoro, &self.kokoro_voices) {
            backends.push(EngineRequest::Kokoro {
                model: model.clone(),
                voices: voices.clone(),
            });
        }
        backends
    }

    fn judge(&self) -> Option<AsrRequest> {
        if let Some(model_directory) = &self.asr_parakeet {
            return Some(AsrRequest::Parakeet {
                model_directory: model_directory.clone(),
                gpu: self.asr_parakeet_gpu,
            });
        }
        self.asr_whisper_candle
            .clone()
            .map(|model_directory| AsrRequest::WhisperCandle { model_directory })
    }
}

pub(crate) fn run(cli: &Cli) -> anyhow::Result<()> {
    let judge = cli.judge();
    if cli.max_wer.is_some() && judge.is_none() {
        anyhow::bail!("--max-wer needs a judge; name --asr-parakeet or --asr-whisper-candle");
    }
    finite(cli.max_rtf, "--max-rtf")?;
    finite(cli.max_wer, "--max-wer")?;
    let request = aria_bench::tts::Request {
        corpus: cli.corpus.clone(),
        results: cli.results.clone(),
        backends: cli.backends(),
        // An empty or malformed name is refused here, before any load.
        voice: match &cli.voice {
            Some(name) => Some(Voice::new(name).context("the named voice is unusable")?),
            None => None,
        },
        rate: SpeakingRate::new(cli.rate).context("the speaking rate is out of range")?,
        runs: cli.runs,
        judge,
    };

    let report = match aria_bench::tts::run(&request) {
        Ok(report) => report,
        Err(error) => {
            // The chain names model and result paths; it belongs in the
            // log. The terminal learns the outermost cause only.
            tracing::warn!(detail = %error_chain(&error), "the benchmark failed");
            eprintln!("error: {error}");
            std::process::exit(1);
        }
    };
    print(&report);
    mostly_worked(&report)?;
    within(&report, cli)?;
    Ok(())
}

/// A backend that fails more attempts than it finishes is broken, not slow.
/// Its median hides behind the few attempts that worked, so the percentile
/// gates below would pass a machine that barely speaks.
fn mostly_worked(report: &Report) -> anyhow::Result<()> {
    let broken = report
        .summaries
        .iter()
        .find(|summary| summary.failures * 2 > summary.attempts);
    match broken {
        Some(summary) => anyhow::bail!(
            "{} failed {} of {} attempts",
            summary.backend,
            summary.failures,
            summary.attempts
        ),
        None => Ok(()),
    }
}

/// Every backend reports before this runs, so one regression cannot hide
/// behind another backend's numbers.
fn within(report: &Report, cli: &Cli) -> anyhow::Result<()> {
    let max_rtf = cli.max_rtf;
    let max_wer = cli.max_wer;
    if let Some(ceiling) = max_rtf {
        let exceeded: Vec<&BackendSummary> = report
            .summaries
            .iter()
            .filter(|summary| summary.median_rtf.is_some_and(|rtf| rtf > ceiling))
            .collect();
        anyhow::ensure!(
            exceeded.is_empty(),
            "{} exceeded the {:.2} rate-factor ceiling",
            names(&exceeded),
            ceiling
        );
    }
    if let Some(ceiling) = max_wer {
        // An unmeasured backend has not passed the gate.
        let exceeded: Vec<&BackendSummary> = report
            .summaries
            .iter()
            .filter(|summary| summary.wer.is_none_or(|wer| wer > ceiling))
            .collect();
        anyhow::ensure!(
            exceeded.is_empty(),
            "{} exceeded the {:.0}% ceiling",
            names(&exceeded),
            ceiling * 100.0
        );
    }
    Ok(())
}

/// A gate that is not a number compares false against everything and would
/// silently guard nothing.
fn finite(value: Option<f64>, flag: &str) -> anyhow::Result<()> {
    match value {
        Some(value) if !value.is_finite() => anyhow::bail!("{flag} must be a finite number"),
        _ => Ok(()),
    }
}

fn names(summaries: &[&BackendSummary]) -> String {
    summaries
        .iter()
        .map(|summary| summary.backend)
        .collect::<Vec<_>>()
        .join(", ")
}

fn print(report: &Report) {
    println!("\n{} utterances\n", report.corpus_utterances);
    println!(
        "{:<10} {:>7} {:>7}  {:>8} {:>8} {:>7}  {:>7} {:>7}",
        "backend", "load_s", "failed", "med ttfa", "p95 ttfa", "med rtf", "wer", "attempts"
    );
    for summary in &report.summaries {
        println!(
            "{:<10} {:>7.1} {:>7}  {:>8} {:>8} {:>7}  {:>7} {:>7}",
            summary.backend,
            summary.load_seconds,
            summary.failures,
            optional_millis(summary.median_ttfa_ms),
            optional_millis(summary.p95_ttfa_ms),
            rate_factor(summary.median_rtf),
            percent(summary.wer),
            summary.attempts,
        );
    }
    for skipped in &report.skipped {
        println!("skipped {}: {}", skipped.backend, skipped.reason);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    println!(
        "\nresults: {}",
        home_relative(&report.results, home.as_deref())
    );
}

/// Keeps the user's name out of a printed path: a path under home prints as
/// `~/...`, and any other absolute path as its file name alone.
fn home_relative(path: &Path, home: Option<&Path>) -> String {
    if let Some(relative) = home.and_then(|home| path.strip_prefix(home).ok()) {
        return format!("~/{}", relative.display());
    }
    if path.is_absolute()
        && let Some(name) = path.file_name()
    {
        return name.to_string_lossy().into_owned();
    }
    path.display().to_string()
}

fn optional_millis(value: Option<u128>) -> String {
    match value {
        Some(millis) => format!("{millis}"),
        None => "-".to_owned(),
    }
}

fn rate_factor(rtf: Option<f64>) -> String {
    match rtf {
        Some(rtf) => format!("{rtf:.2}"),
        None => "-".to_owned(),
    }
}

fn percent(wer: Option<f64>) -> String {
    match wer {
        Some(wer) => format!("{:.1}%", wer * 100.0),
        None => "-".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Option<&'static Path> {
        Some(Path::new("/Users/tester"))
    }

    #[test]
    fn a_path_under_home_is_home_relative() {
        assert_eq!(
            home_relative(Path::new("/Users/tester/runs/bench.jsonl"), home()),
            "~/runs/bench.jsonl"
        );
    }

    #[test]
    fn another_users_path_keeps_only_its_file_name() {
        assert_eq!(
            home_relative(Path::new("/Users/alice/runs/bench.jsonl"), home()),
            "bench.jsonl"
        );
    }

    #[test]
    fn a_relative_path_is_shown_whole() {
        assert_eq!(
            home_relative(Path::new("runs/bench.jsonl"), home()),
            "runs/bench.jsonl"
        );
    }

    #[test]
    fn an_unknown_home_still_hides_the_user() {
        assert_eq!(
            home_relative(Path::new("/Users/tester/bench.jsonl"), None),
            "bench.jsonl"
        );
    }
}
