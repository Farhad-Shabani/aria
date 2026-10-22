//! Measures every configured transcription backend over one corpus.
//!
//! ```text
//! aria-bench corpus build/corpus
//! aria-bench stt --corpus build/corpus/manifest.tsv \
//!     --model parakeet=~/.aria/models/parakeet/parakeet-tdt-0.6b-v3
//! ```

use std::path::{Path, PathBuf};

use anyhow::Context;
use aria::config::backend;
use aria_bench::stt::analysis::Artifact;
use aria_bench::stt::{BackendRequest, Cadence, Mode, Report, Request, Summary};
use aria_harness::stats::Decision;
use clap::Parser;

use crate::flag_path::expand;

const EXAMPLES: &str = "\
Examples:
  # One engine, named explicitly
  aria-bench stt --corpus build/corpus/manifest.tsv \\
      --model parakeet=~/.aria/models/parakeet/parakeet-tdt-0.6b-v3

  # Several engines; each --model adds one
  aria-bench stt --corpus build/corpus/manifest.tsv \\
      --model whisper=~/.aria/models/whisper/ggml-large-v3-turbo-q8_0.bin \\
      --model moonshine=~/.aria/models/moonshine/base

  # No --model: the ARIA_TEST_*_MODEL variables name the engines
  aria-bench stt --corpus build/corpus/manifest.tsv";

#[derive(Parser)]
#[command(about = "Word error rate per transcription backend", after_help = EXAMPLES)]
pub(crate) struct Cli {
    /// Manifest of audio paths and reference text, one utterance per line.
    #[arg(long)]
    corpus: PathBuf,

    /// Where to write one result row per utterance, as newline-delimited JSON.
    #[arg(long)]
    results: Option<PathBuf>,

    /// An engine and its model, as `<backend>=<path>` (parakeet-mlx takes a
    /// Hugging Face repository id). Repeatable. Omitted entirely, the
    /// ARIA_TEST_*_MODEL environment variables name the engines instead.
    #[arg(long, value_name = "BACKEND=PATH")]
    model: Vec<String>,

    /// Run parakeet on the GPU rather than the CPU.
    #[arg(long)]
    gpu: bool,

    /// Interpreter the parakeet-mlx sidecar runs under; falls back to
    /// ARIA_TEST_PYTHON.
    #[arg(long)]
    python: Option<PathBuf>,

    /// A completed results artifact to compare this run against.
    #[arg(long)]
    baseline: Option<PathBuf>,

    #[arg(long, default_value_t = 0.0, value_parser = nonnegative)]
    max_wer_increase: f64,

    #[arg(long, value_parser = ["whole", "streaming"], help = "Decode whole recordings (default) or stream them; nemotron always streams natively")]
    mode: Option<String>,

    /// Fail the run when any backend's word error rate exceeds this fraction.
    #[arg(long, value_parser = nonnegative)]
    max_wer: Option<f64>,

    /// Decode cadence for a streaming backend, as `<stride_ms>/<max_ms>`, in
    /// place of the one it ships with. Whisper, parakeet, and moonshine stream
    /// by agreement; parakeet-mlx streams natively in its sidecar, where only
    /// the stride applies.
    #[arg(long, value_name = "STRIDE_MS/MAX_MS")]
    cadence: Option<String>,

    /// How many times each backend hears the whole corpus. Under --realtime,
    /// streaming engines commit words on live timing, so more passes expose
    /// their run-to-run variance.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
    passes: u32,

    /// Replay streamed audio at wall-clock pace instead of one stride per
    /// decode. Slower, but reproduces a live microphone's backlog dynamics;
    /// results are not comparable across the two paces.
    #[arg(long)]
    realtime: bool,

    /// Record provisional caption updates from the existing streaming decodes.
    #[arg(long)]
    provisional: bool,
}

/// The engine names a `--model` value may open with.
const BACKENDS: [&str; 6] = [
    backend::WHISPER,
    backend::WHISPER_CANDLE,
    backend::PARAKEET,
    backend::MOONSHINE,
    backend::NEMOTRON,
    backend::PARAKEET_MLX,
];

/// Splits `<backend>=<value>`, refusing names no engine answers to.
fn split_model(value: &str) -> anyhow::Result<(&str, &str)> {
    let (name, model) = value
        .split_once('=')
        .with_context(|| format!("{value:?} is not <backend>=<path>"))?;
    anyhow::ensure!(
        BACKENDS.contains(&name),
        "{name:?} is not a backend; expected one of {}",
        BACKENDS.join(", ")
    );
    anyhow::ensure!(!model.is_empty(), "{value:?} names no model");
    Ok((name, model))
}

impl Cli {
    /// Each named engine, in the order the flags declare them — or, with no
    /// `--model` at all, the fixed order the environment names them in.
    fn backends(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<Vec<BackendRequest>> {
        let named: Vec<(String, String)> = match self.model.is_empty() {
            false => self
                .model
                .iter()
                .map(|value| split_model(value).map(|(n, m)| (n.to_owned(), m.to_owned())))
                .collect::<anyhow::Result<_>>()?,
            true => env_models(&env),
        };
        anyhow::ensure!(
            !named.is_empty(),
            "no model was named; pass --model <backend>=<path> or set ARIA_TEST_*_MODEL"
        );

        let mut backends = Vec::new();
        for (name, model) in &named {
            backends.push(self.request(name, model, &env)?);
        }
        anyhow::ensure!(
            !self.gpu || named.iter().any(|(name, _)| name == backend::PARAKEET),
            "--gpu applies to parakeet; name --model parakeet=<path>"
        );
        Ok(backends)
    }

    fn request(
        &self,
        name: &str,
        model: &str,
        env: &impl Fn(&str) -> Option<String>,
    ) -> anyhow::Result<BackendRequest> {
        Ok(match name {
            backend::WHISPER => BackendRequest::Whisper {
                model: expand(model)?,
            },
            backend::WHISPER_CANDLE => BackendRequest::WhisperCandle {
                model: expand(model)?,
            },
            backend::PARAKEET => BackendRequest::Parakeet {
                model: expand(model)?,
                gpu: self.gpu,
            },
            backend::MOONSHINE => BackendRequest::Moonshine {
                model: expand(model)?,
            },
            backend::NEMOTRON => BackendRequest::Nemotron {
                model: expand(model)?,
            },
            backend::PARAKEET_MLX => {
                let interpreter = match &self.python {
                    Some(path) => path.clone(),
                    None => expand(&env("ARIA_TEST_PYTHON").context(
                        "parakeet-mlx runs under an interpreter; pass --python or set ARIA_TEST_PYTHON",
                    )?)?,
                };
                BackendRequest::ParakeetMlx {
                    // A repository id, not a path; nothing to expand.
                    model: model.to_owned(),
                    interpreter,
                }
            }
            _ => unreachable!("split_model admits only the names above"),
        })
    }

    fn cadence(&self) -> anyhow::Result<Option<Cadence>> {
        let Some(policy) = self.cadence.as_deref() else {
            return Ok(None);
        };
        let (stride, max) = policy
            .split_once('/')
            .context("a decode cadence reads <stride_ms>/<max_ms>")?;
        let millis = |value: &str| -> anyhow::Result<usize> {
            value
                .trim()
                .parse()
                .with_context(|| format!("{value:?} is not a count of milliseconds"))
        };
        let (stride, max) = (millis(stride)?, millis(max)?);
        anyhow::ensure!(
            stride > 0 && stride < max && max <= usize::MAX / 16_000,
            "cadence requires 0 < stride < max without sample-count overflow"
        );
        Ok(Some(Cadence::from_millis(stride, max)))
    }
}

/// What the environment names, in the fixed order the benches share.
fn env_models(env: &impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    [
        (backend::WHISPER, "ARIA_TEST_WHISPER_MODEL"),
        (backend::WHISPER_CANDLE, "ARIA_TEST_WHISPER_CANDLE_MODEL"),
        (backend::PARAKEET, "ARIA_TEST_PARAKEET_MODEL"),
        (backend::MOONSHINE, "ARIA_TEST_MOONSHINE_MODEL"),
        (backend::NEMOTRON, "ARIA_TEST_NEMOTRON_MODEL"),
        (backend::PARAKEET_MLX, "ARIA_TEST_PARAKEET_MLX_MODEL"),
    ]
    .into_iter()
    .filter_map(|(name, variable)| {
        env(variable)
            .filter(|value| !value.trim().is_empty())
            .map(|value| (name.to_owned(), value))
    })
    .collect()
}

pub(crate) fn run(cli: &Cli) -> anyhow::Result<()> {
    let request = Request {
        corpus: cli.corpus.clone(),
        results: cli.results.clone(),
        baseline: cli.baseline.clone(),
        backends: cli.backends(|variable| std::env::var(variable).ok())?,
        cadence: cli.cadence()?,
        mode: cli.mode.as_deref().map(|mode| {
            if mode == "streaming" {
                Mode::Streaming
            } else {
                Mode::Whole
            }
        }),
        passes: cli.passes,
        realtime: cli.realtime,
        provisional: cli.provisional,
    };

    let report = aria_bench::stt::run(&request).context("the benchmark failed")?;
    print(&report);
    completed_without_errors(&report)?;
    if let Some(path) = &cli.baseline {
        compare_paths(&report.results, path, cli.max_wer_increase)?;
    }
    match cli.max_wer {
        Some(ceiling) => within(&report, ceiling),
        None => Ok(()),
    }
}

#[derive(Parser)]
#[command(about = "Report or compare completed STT artifacts without inference")]
pub(crate) struct CompareCli {
    results: PathBuf,
    #[arg(long)]
    baseline: Option<PathBuf>,
    #[arg(long, default_value_t = 0.0, value_parser = nonnegative)]
    max_wer_increase: f64,
}

fn nonnegative(value: &str) -> Result<f64, String> {
    let number: f64 = value.parse().map_err(|_| "expected a number".to_owned())?;
    if !number.is_finite() || number < 0.0 {
        return Err("expected a finite, nonnegative fraction".to_owned());
    }
    Ok(number)
}

pub(crate) fn compare(cli: &CompareCli) -> anyhow::Result<()> {
    let artifact = Artifact::load(&cli.results)?;
    for measurement in artifact.measurements() {
        println!(
            "{} WER {:.2}% 95% CI {:?}%; {} utterances × {} passes; {} failures",
            measurement.backend,
            measurement.wer * 100.0,
            measurement
                .interval
                .map(|(lower, upper)| (lower * 100.0, upper * 100.0)),
            measurement.utterances,
            measurement.passes,
            measurement.failures
        );
        println!(
            "  decode {} ms / {} calls; total {} ms; first finalized median {:?} ms (n={}), first preview median {:?} ms (n={}); {}; timer origin: {}",
            measurement.decode_ms,
            measurement.decode_calls,
            measurement.transcribe_ms,
            measurement.first_finalized_median_ms,
            measurement.finalized_attempts,
            measurement.first_preview_median_ms,
            measurement.preview_attempts,
            if measurement.realtime {
                "real-time replay latency"
            } else {
                "decode-paced event timing, not live latency"
            },
            measurement.timing_origin
        );
    }
    if let Some(baseline) = &cli.baseline {
        compare_paths(&cli.results, baseline, cli.max_wer_increase)?;
    }
    Ok(())
}

#[derive(Parser)]
#[command(about = "Check an STT run and save it as the committed reference, naming no host")]
pub(crate) struct ReferenceCli {
    results: PathBuf,
    destination: PathBuf,
}

pub(crate) fn reference(cli: &ReferenceCli) -> anyhow::Result<()> {
    aria_bench::stt::analysis::save_reference(&cli.results, &cli.destination)?;
    for measurement in Artifact::load(&cli.destination)?.measurements() {
        println!(
            "{} WER {:.2}%; {} utterances × {} passes -> {}",
            measurement.backend,
            measurement.wer * 100.0,
            measurement.utterances,
            measurement.passes,
            cli.destination.display()
        );
    }
    Ok(())
}

fn compare_paths(results: &Path, baseline: &Path, margin: f64) -> anyhow::Result<()> {
    let comparisons = Artifact::load(results)?.compare(&Artifact::load(baseline)?, margin)?;
    for comparison in &comparisons {
        println!(
            "{} {:.2}% -> {:.2}%; paired Δ WER 95% CI {:?} percentage points; {}",
            comparison.backend,
            comparison.baseline_wer * 100.0,
            comparison.candidate_wer * 100.0,
            comparison
                .difference_interval
                .map(|(lower, upper)| (lower * 100.0, upper * 100.0)),
            comparison.decision.label()
        );
        for change in &comparison.changes {
            println!("  {} changed", change.utterance);
            println!("    reference:   {}", change.reference);
            for (pass, hypothesis) in &change.baseline {
                println!("    baseline {pass}:  {hypothesis}");
            }
            for (pass, hypothesis) in &change.candidate {
                println!("    candidate {pass}: {hypothesis}");
            }
        }
    }
    anyhow::ensure!(
        comparisons
            .iter()
            .all(|comparison| comparison.decision == Decision::Accepted),
        "WER non-regression was not established; comparison regressed or is inconclusive"
    );
    Ok(())
}

fn completed_without_errors(report: &Report) -> anyhow::Result<()> {
    let silent = report.summaries.iter().find(|summary| summary.failures > 0);
    match silent {
        Some(summary) => anyhow::bail!(
            "{} failed {} utterance attempts",
            summary.backend,
            summary.failures
        ),
        None => Ok(()),
    }
}

/// Every backend reports before this runs, so one regression cannot hide the
/// rest of the comparison.
fn within(report: &Report, ceiling: f64) -> anyhow::Result<()> {
    let exceeded: Vec<&Summary> = report
        .summaries
        .iter()
        .filter(|summary| summary.word_error_rate() > ceiling)
        .collect();
    anyhow::ensure!(
        exceeded.is_empty(),
        "{} exceeded the {:.0}% ceiling",
        exceeded
            .iter()
            .map(|summary| summary.backend)
            .collect::<Vec<_>>()
            .join(", "),
        ceiling * 100.0
    );
    Ok(())
}

fn print(report: &Report) {
    println!(
        "\n{} utterances, {} reference words\n",
        report.corpus_utterances, report.corpus_words
    );
    println!(
        "{:<14} {:>7}  {:>14}  {:>6} {:>6} {:>6}  {:>8}",
        "backend", "wer", "95% ci", "sub", "del", "ins", "failed"
    );
    for summary in &report.summaries {
        println!("{}", line(summary));
    }
    for skipped in &report.skipped {
        println!("skipped {}: {}", skipped.backend, skipped.reason);
    }
    println!("\nresults: {}", report.results.display());
}

fn line(summary: &Summary) -> String {
    let interval = summary
        .wer_interval()
        .map(|(lower, upper)| format!("{:>5.2}–{:<5.2}%", lower * 100.0, upper * 100.0))
        .unwrap_or_else(|| "—".to_owned());
    format!(
        "{:<14} {:>6.2}%  {:>14}  {:>6} {:>6} {:>6}  {:>8}",
        summary.backend,
        summary.word_error_rate() * 100.0,
        interval,
        summary.substitutions,
        summary.deletions,
        summary.insertions,
        summary.failures,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Cli {
        Cli::try_parse_from(
            ["aria-bench", "--corpus", "manifest.tsv"]
                .iter()
                .chain(arguments),
        )
        .expect("arguments parse")
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn invalid_cadence_and_thresholds_are_rejected() {
        assert!(parse(&["--cadence", "0/20000"]).cadence().is_err());
        assert!(parse(&["--cadence", "1000/500"]).cadence().is_err());
        assert!(nonnegative("NaN").is_err());
        assert!(nonnegative("-0.1").is_err());
        assert!(nonnegative("0").is_ok());
    }

    #[test]
    fn a_model_value_splits_into_backend_and_path() {
        assert_eq!(
            split_model("parakeet=/models/tdt").expect("valid"),
            ("parakeet", "/models/tdt")
        );
    }

    #[test]
    fn an_unknown_backend_is_named_in_the_error() {
        let error = split_model("foo=/models/x").expect_err("rejected");
        assert!(error.to_string().contains("parakeet"), "{error}");
    }

    #[test]
    fn a_value_without_an_equals_sign_is_rejected() {
        assert!(split_model("parakeet").is_err());
    }

    #[test]
    fn models_keep_their_flag_order() {
        let cli = parse(&["--model", "whisper=/w.bin", "--model", "moonshine=/m"]);
        let backends = cli.backends(no_env).expect("resolves");
        assert!(matches!(backends[0], BackendRequest::Whisper { .. }));
        assert!(matches!(backends[1], BackendRequest::Moonshine { .. }));
    }

    #[test]
    fn gpu_without_a_parakeet_model_is_refused() {
        let cli = parse(&["--model", "whisper=/w.bin", "--gpu"]);
        assert!(cli.backends(no_env).is_err());
    }

    #[test]
    fn gpu_reaches_the_parakeet_request() {
        let cli = parse(&["--model", "parakeet=/p", "--gpu"]);
        let backends = cli.backends(no_env).expect("resolves");
        assert!(matches!(
            backends[0],
            BackendRequest::Parakeet { gpu: true, .. }
        ));
    }

    #[test]
    fn parakeet_mlx_without_an_interpreter_is_refused() {
        let cli = parse(&["--model", "parakeet-mlx=repo/id"]);
        assert!(cli.backends(no_env).is_err());
    }

    #[test]
    fn parakeet_mlx_takes_the_python_flag() {
        let cli = parse(&[
            "--model",
            "parakeet-mlx=repo/id",
            "--python",
            "/usr/bin/python3",
        ]);
        let backends = cli.backends(no_env).expect("resolves");
        assert!(matches!(backends[0], BackendRequest::ParakeetMlx { .. }));
    }

    #[test]
    fn the_environment_names_engines_when_no_model_flag_appears() {
        let cli = parse(&[]);
        let backends = cli
            .backends(|variable| match variable {
                "ARIA_TEST_WHISPER_MODEL" => Some("/w.bin".to_owned()),
                "ARIA_TEST_MOONSHINE_MODEL" => Some("/m".to_owned()),
                _ => None,
            })
            .expect("resolves");
        assert!(matches!(backends[0], BackendRequest::Whisper { .. }));
        assert!(matches!(backends[1], BackendRequest::Moonshine { .. }));
    }

    #[test]
    fn nothing_named_anywhere_is_refused() {
        let cli = parse(&[]);
        assert!(cli.backends(no_env).is_err());
    }
}
