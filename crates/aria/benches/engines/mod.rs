use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use aria::config::{TranscriptionConfig, TranscriptionDevice, backend};
use aria::transcription::{Cadence, Stream};
use aria_harness::{
    artifact::{self, Provenance},
    inputs, stats,
};
use serde::{Deserialize, Serialize};

pub(crate) const MOONSHINE_MODEL_ENV: &str = "ARIA_TEST_MOONSHINE_MODEL";
pub(crate) const NEMOTRON_MODEL_ENV: &str = "ARIA_TEST_NEMOTRON_MODEL";
pub(crate) const PARAKEET_MODEL_ENV: &str = "ARIA_TEST_PARAKEET_MODEL";
pub(crate) const WHISPER_MODEL_ENV: &str = "ARIA_TEST_WHISPER_MODEL";
pub(crate) const WHISPER_CANDLE_MODEL_ENV: &str = "ARIA_TEST_WHISPER_CANDLE_MODEL";
pub(crate) const PARAKEET_MLX_MODEL_ENV: &str = "ARIA_TEST_PARAKEET_MLX_MODEL";
pub(crate) const PYTHON_ENV: &str = "ARIA_TEST_PYTHON";
pub(crate) const HUB_CACHE_ENV: &str = "HF_HOME";
pub(crate) const WAV_ENV: &str = "ARIA_TEST_DICTATION_WAV";
pub(crate) const DEFAULT_RUNS: u32 = 10;

pub(crate) struct BenchEngine {
    pub(crate) backend: &'static str,
    pub(crate) config: TranscriptionConfig,
}

pub(crate) fn discover() -> anyhow::Result<Vec<BenchEngine>> {
    let selected = std::env::var("ARIA_BENCH_BACKENDS").ok().map(|value| {
        value
            .split(',')
            .map(|name| name.trim().to_owned())
            .collect::<Vec<_>>()
    });
    let wanted = |backend: &str| {
        selected
            .as_ref()
            .is_none_or(|names| names.iter().any(|name| name == backend))
    };
    let path = |backend, variable| {
        if wanted(backend) {
            inputs::path_input(variable)
        } else {
            Ok(None)
        }
    };
    let mut engines = Vec::new();
    if let Some(model) = path(backend::WHISPER, WHISPER_MODEL_ENV)? {
        engines.push(BenchEngine {
            backend: backend::WHISPER,
            config: TranscriptionConfig::Whisper { model },
        });
    }
    if let Some(model_directory) = path(backend::WHISPER_CANDLE, WHISPER_CANDLE_MODEL_ENV)? {
        engines.push(BenchEngine {
            backend: backend::WHISPER_CANDLE,
            config: TranscriptionConfig::WhisperCandle { model_directory },
        });
    }
    if let Some(model_directory) = path(backend::PARAKEET, PARAKEET_MODEL_ENV)? {
        engines.push(BenchEngine {
            backend: backend::PARAKEET,
            config: TranscriptionConfig::Parakeet {
                model_directory,
                device: TranscriptionDevice::Cpu,
            },
        });
    }
    if let Some(model_directory) = path(backend::MOONSHINE, MOONSHINE_MODEL_ENV)? {
        engines.push(BenchEngine {
            backend: backend::MOONSHINE,
            config: TranscriptionConfig::Moonshine { model_directory },
        });
    }
    if let Some(model_directory) = path(backend::NEMOTRON, NEMOTRON_MODEL_ENV)? {
        engines.push(BenchEngine {
            backend: backend::NEMOTRON,
            config: TranscriptionConfig::Nemotron { model_directory },
        });
    }
    if wanted(backend::PARAKEET_MLX)
        && let Some(engine) = parakeet_mlx()?
    {
        engines.push(engine);
    }
    if let Some(names) = selected {
        for name in names {
            ensure!(
                engines.iter().any(|engine| engine.backend == name),
                "requested backend {name:?} is unknown or has no configured model"
            );
        }
    }
    Ok(engines)
}

fn parakeet_mlx() -> anyhow::Result<Option<BenchEngine>> {
    let Some(repository) = repository(PARAKEET_MLX_MODEL_ENV) else {
        return Ok(None);
    };
    let Some(interpreter) = inputs::path_input(PYTHON_ENV)? else {
        anyhow::bail!("{PARAKEET_MLX_MODEL_ENV} is set but {PYTHON_ENV} is not");
    };
    // The sandbox gives the daemon a fake home, so the sidecar looks for its
    // model in an empty cache and downloads gigabytes on every run.
    if std::env::var_os(HUB_CACHE_ENV).is_none() {
        anyhow::bail!(
            "{PARAKEET_MLX_MODEL_ENV} needs {HUB_CACHE_ENV} pointing at the real hub cache"
        );
    }
    Ok(Some(BenchEngine {
        backend: backend::PARAKEET_MLX,
        config: TranscriptionConfig::ParakeetMlx {
            model: repository,
            interpreter,
        },
    }))
}

fn repository(variable: &str) -> Option<String> {
    std::env::var(variable)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub(crate) struct Options {
    pub(crate) runs: u32,
    pub(crate) warmups: u32,
    baseline: Option<Run>,
    margin_ms: f64,
}

impl Options {
    pub(crate) fn from_environment() -> anyhow::Result<Self> {
        ensure!(
            std::env::var_os("ARIA_BENCH_SAVE_BASELINE").is_none(),
            "use the saved artifact path as ARIA_BENCH_BASELINE; baseline overwrites are unsupported"
        );
        let warmups = std::env::var("ARIA_BENCH_WARMUPS")
            .unwrap_or_else(|_| "1".into())
            .parse::<u32>()
            .context("ARIA_BENCH_WARMUPS must be a nonnegative integer")?;
        let runs = inputs::run_count(DEFAULT_RUNS)?;
        ensure!(
            runs.checked_add(warmups).is_some(),
            "run and warmup counts overflow"
        );
        let margin_ms = std::env::var("ARIA_BENCH_MAX_REGRESSION_MS")
            .unwrap_or_else(|_| "0".into())
            .parse::<f64>()
            .context("ARIA_BENCH_MAX_REGRESSION_MS must be nonnegative")?;
        ensure!(
            margin_ms.is_finite() && margin_ms >= 0.0,
            "ARIA_BENCH_MAX_REGRESSION_MS must be finite and nonnegative"
        );
        let baseline_path = inputs::path_input("ARIA_BENCH_BASELINE")?;
        ensure!(
            std::env::var_os("ARIA_BENCH_BASELINE").is_none() || baseline_path.is_some(),
            "ARIA_BENCH_BASELINE cannot be blank"
        );
        let baseline = baseline_path
            .map(|path| -> anyhow::Result<Run> {
                let run = serde_json::from_reader(
                    std::fs::File::open(&path)
                        .with_context(|| format!("cannot read baseline {}", path.display()))?,
                )?;
                validate(&run)?;
                Ok(run)
            })
            .transpose()?;
        Ok(Self {
            runs,
            warmups,
            baseline,
            margin_ms,
        })
    }

    pub(crate) fn missing_inputs(&self, bench: &str) -> anyhow::Result<()> {
        ensure!(
            self.baseline.is_none(),
            "{bench}: required baseline comparison has no benchmark inputs"
        );
        println!("skipping {bench}: no configured models or fixture");
        Ok(())
    }

    pub(crate) fn offline(&self, kind: &str, metric: &str) -> anyhow::Result<bool> {
        let path = inputs::path_input("ARIA_BENCH_COMPARE")?;
        ensure!(
            std::env::var_os("ARIA_BENCH_COMPARE").is_none() || path.is_some(),
            "ARIA_BENCH_COMPARE cannot be blank"
        );
        let Some(path) = path else {
            return Ok(false);
        };
        ensure!(
            self.baseline.is_some(),
            "ARIA_BENCH_COMPARE requires ARIA_BENCH_BASELINE"
        );
        let current: Run = serde_json::from_reader(std::fs::File::open(&path)?)?;
        ensure!(
            current.kind == kind,
            "candidate has the wrong benchmark kind"
        );
        self.compare(&current, metric)?;
        if current
            .entries
            .iter()
            .any(|entry| entry.configuration.contains("rpc-delivery-v2"))
        {
            self.compare(&current, "request->final-receipt")?;
        }
        if current.kind == "dictation-latency"
            && current.entries.iter().any(|entry| {
                entry.configuration.contains("rpc-delivery-v1")
                    || entry.configuration.contains("rpc-delivery-v2")
            })
        {
            self.compare(&current, "capture->delivered")?;
        }
        Ok(true)
    }

    pub(crate) fn preflight(&self, current: &Run) -> anyhow::Result<()> {
        if let Some(recorded) = &self.baseline {
            compatible_conditions(current, recorded)?;
        }
        Ok(())
    }

    pub(crate) fn preflight_engine(&self, entry: &Observation) -> anyhow::Result<()> {
        if let Some(recorded) = &self.baseline {
            compatible_entry(entry, recorded)?;
        }
        Ok(())
    }

    pub(crate) fn compare(&self, current: &Run, metric: &str) -> anyhow::Result<()> {
        let Some(recorded) = &self.baseline else {
            return Ok(());
        };
        compare(current, recorded, metric, self.margin_ms)
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Run {
    #[serde(skip)]
    output: Option<PathBuf>,
    schema: u32,
    kind: String,
    provenance: Provenance,
    pub(crate) daemon_sha256: Option<String>,
    fixture_sha256: String,
    runs: u32,
    warmups: u32,
    expected_backends: Vec<String>,
    pub(crate) entries: Vec<Observation>,
    pub(crate) failure: Option<String>,
    #[serde(default)]
    require_same_text: bool,
}

impl Run {
    pub(crate) fn new(
        kind: &str,
        wav: &Path,
        backends: Vec<String>,
        options: &Options,
    ) -> anyhow::Result<Self> {
        let provenance = Provenance::capture()?;
        let directory = inputs::path_input("ARIA_BENCH_RESULTS")?.unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../bench-results")
                .join(&provenance.run_id)
        });
        let output = prepare_output(&directory, kind)?;
        Ok(Self {
            output: Some(output),
            schema: 1,
            kind: kind.into(),
            provenance,
            daemon_sha256: None,
            fixture_sha256: artifact::fingerprint(wav)?,
            runs: options.runs,
            warmups: options.warmups,
            expected_backends: backends,
            entries: Vec::new(),
            failure: None,
            require_same_text: match std::env::var("ARIA_BENCH_REQUIRE_SAME_TEXT").as_deref() {
                Ok("1") => true,
                Err(std::env::VarError::NotPresent) | Ok("0") => false,
                _ => anyhow::bail!("ARIA_BENCH_REQUIRE_SAME_TEXT must be 0 or 1"),
            },
        })
    }

    pub(crate) fn save(&self) -> anyhow::Result<()> {
        let path = self.output.as_ref().context("no collection output path")?;
        artifact::write_json(path, self)?;
        println!("raw results: {}", path.display());
        Ok(())
    }
}

fn prepare_output(directory: &Path, kind: &str) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(directory)?;
    let path = directory.join(format!("{kind}.json"));
    match std::fs::symlink_metadata(&path) {
        Ok(_) => anyhow::bail!("result artifact already exists: {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
        Err(error) => Err(error.into()),
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Observation {
    backend: String,
    configuration: String,
    model_sha256: Option<String>,
    pub(crate) startup_ms: Option<u64>,
    pub(crate) samples_ms: BTreeMap<String, Vec<u64>>,
    pub(crate) warmup_ms: BTreeMap<String, Vec<u64>>,
    pub(crate) daemon_log: Option<String>,
    #[serde(default)]
    pub(crate) delivery_traces: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) vocabulary: Option<serde_json::Value>,
}

impl Observation {
    pub(crate) fn new(engine: &BenchEngine, mode: &str) -> anyhow::Result<Self> {
        let model_sha256 = match &engine.config {
            TranscriptionConfig::Whisper { model } => Some(artifact::fingerprint(model)?),
            TranscriptionConfig::WhisperCandle { model_directory }
            | TranscriptionConfig::Moonshine { model_directory }
            | TranscriptionConfig::Nemotron { model_directory }
            | TranscriptionConfig::Parakeet {
                model_directory, ..
            } => Some(artifact::fingerprint(model_directory)?),
            TranscriptionConfig::ParakeetMlx { .. } => None,
        };
        Ok(Self {
            backend: engine.backend.into(),
            configuration: match &engine.config {
                TranscriptionConfig::Parakeet { device, .. } => {
                    format!("{mode}; device={device:?}")
                }
                TranscriptionConfig::ParakeetMlx { model, interpreter } => format!(
                    "{mode}; repository={model}; interpreter_sha256={}",
                    artifact::fingerprint(interpreter)?
                ),
                _ => mode.into(),
            },
            model_sha256,
            startup_ms: None,
            samples_ms: BTreeMap::new(),
            warmup_ms: BTreeMap::new(),
            daemon_log: None,
            delivery_traces: Vec::new(),
            vocabulary: None,
        })
    }
}

fn validate(run: &Run) -> anyhow::Result<()> {
    ensure!(
        run.schema == 1 && run.failure.is_none(),
        "incomplete or unsupported benchmark artifact"
    );
    let mut backends = run
        .entries
        .iter()
        .map(|entry| entry.backend.clone())
        .collect::<Vec<_>>();
    backends.sort();
    let mut expected = run.expected_backends.clone();
    expected.sort();
    ensure!(
        backends == expected
            && !expected.is_empty()
            && expected.windows(2).all(|pair| pair[0] != pair[1]),
        "missing or duplicate backend observations"
    );
    if run.kind == "dictation-latency" {
        ensure!(
            run.daemon_sha256.is_some(),
            "daemon executable identity is missing"
        );
    }
    let mut previous_trial = 0;
    for entry in &run.entries {
        ensure!(
            entry.startup_ms.is_some(),
            "model/daemon startup observation is missing"
        );
        ensure!(
            entry.model_sha256.is_some(),
            "{} has an unresolved model identity",
            entry.backend
        );
        let streaming = entry.configuration.contains("; insert; streaming; manual;");
        let receipt_evidence = streaming && entry.configuration.contains("rpc-delivery-v2");
        let metrics: &[&str] = match run.kind.as_str() {
            "dictation-latency" if receipt_evidence => &[
                "recording",
                "transcription",
                "total (daemon)",
                "client wall",
                "capture->first-segment",
                "request->first-receipt",
                "request->final-receipt",
            ],
            "dictation-latency" if streaming => &[
                "recording",
                "transcription",
                "total (daemon)",
                "client wall",
                "capture->first-segment",
            ],
            "first-text" => &["first-finalized"],
            "dictation-latency" => &[
                "recording",
                "transcription",
                "delivery",
                "total (daemon)",
                "release->delivered",
                "client wall",
            ],
            _ => anyhow::bail!("unknown benchmark kind"),
        };
        for metric in metrics {
            ensure!(
                entry
                    .samples_ms
                    .get(*metric)
                    .is_some_and(|samples| samples.len() == run.runs as usize)
                    && run.runs > 0,
                "{} has incomplete {metric} evidence",
                entry.backend
            );
            ensure!(
                entry.warmup_ms.get(*metric).map_or(0, Vec::len) == run.warmups as usize,
                "{} has incomplete {metric} warmup evidence",
                entry.backend
            );
        }
        if entry.configuration.contains("rpc-delivery-v1")
            || entry.configuration.contains("rpc-delivery-v2")
        {
            ensure!(
                entry.delivery_traces.len() == (run.runs + run.warmups) as usize,
                "incomplete delivered-text evidence"
            );
            ensure!(
                entry
                    .samples_ms
                    .get("capture->delivered")
                    .is_some_and(|samples| samples.len() == run.runs as usize),
                "incomplete capture-to-delivered evidence"
            );
            ensure!(
                entry
                    .warmup_ms
                    .get("capture->delivered")
                    .map_or(0, Vec::len)
                    == run.warmups as usize,
                "incomplete capture-to-delivered warmup evidence"
            );
            for (index, row) in entry.delivery_traces.iter().enumerate() {
                let readback = if receipt_evidence {
                    &row["target"]["text"]
                } else if streaming {
                    &row["target_appended_text"]
                } else {
                    &row["clipboard_text"]
                };
                ensure!(
                    row["text"].is_string() && &row["text"] == readback,
                    "delivered text differs from result"
                );
                if receipt_evidence {
                    let trial = validate_receipts(&row["target"])?;
                    ensure!(trial > previous_trial, "target trials are out of order");
                    previous_trial = trial;
                    for (metric, field) in [
                        ("request->first-receipt", "request_to_first_receipt_ms"),
                        ("request->final-receipt", "request_to_final_receipt_ms"),
                    ] {
                        let sample = if index < run.warmups as usize {
                            entry.warmup_ms[metric][index]
                        } else {
                            entry.samples_ms[metric][index - run.warmups as usize]
                        };
                        ensure!(
                            row["target"][field].as_u64() == Some(sample),
                            "{metric} sample differs from target trace"
                        );
                    }
                }
                if streaming {
                    let first_ms = row["capture_to_first_segment_ms"]
                        .as_u64()
                        .context("missing first segment trace timing")?;
                    let sample = if index < run.warmups as usize {
                        entry.warmup_ms["capture->first-segment"][index]
                    } else {
                        entry.samples_ms["capture->first-segment"][index - run.warmups as usize]
                    };
                    ensure!(
                        first_ms == sample,
                        "first segment timing differs from trace"
                    );
                    let segments = row["corrected_segments"]
                        .as_array()
                        .context("missing corrected segment evidence")?;
                    ensure!(!segments.is_empty(), "missing corrected segment evidence");
                    let text = segments
                        .iter()
                        .map(|segment| {
                            segment["text"]
                                .as_str()
                                .context("missing corrected segment text")
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?
                        .join(" ");
                    ensure!(row["text"] == text, "corrected segments differ from result");
                }
            }
        }
    }
    Ok(())
}

fn validate_receipts(target: &serde_json::Value) -> anyhow::Result<u64> {
    let trial = target["trial"].as_u64().context("missing target trial")?;
    ensure!(trial > 0, "target trial must be positive");
    ensure!(
        target["clock"] == "CLOCK_UPTIME_RAW",
        "invalid target clock"
    );
    let text = target["text"].as_str().context("missing target text")?;
    let receipts = target["receipts"]
        .as_array()
        .context("missing target receipts")?;
    ensure!(!receipts.is_empty(), "missing target receipts");
    let mut previous_us = 0;
    let mut previous_bytes = 0;
    for (index, receipt) in receipts.iter().enumerate() {
        let elapsed_us = receipt["elapsed_us"]
            .as_u64()
            .context("missing target receipt timing")?;
        let bytes = receipt["utf8_bytes"]
            .as_u64()
            .context("missing target receipt byte count")?;
        ensure!(
            elapsed_us >= previous_us && bytes > previous_bytes && bytes <= text.len() as u64,
            "target receipts are out of order"
        );
        if index == 0 {
            ensure!(
                receipt
                    .get("gap_us")
                    .is_some_and(serde_json::Value::is_null),
                "first target gap must be null"
            );
        } else {
            let gap = receipt["gap_us"]
                .as_u64()
                .context("missing target receipt gap")?;
            let elapsed_gap = elapsed_us - previous_us;
            ensure!(
                gap <= elapsed_gap && elapsed_gap - gap <= 1,
                "target receipt gap differs from timing"
            );
        }
        previous_us = elapsed_us;
        previous_bytes = bytes;
    }
    ensure!(
        previous_bytes == text.len() as u64,
        "final target byte count differs from text"
    );
    ensure!(
        target["request_to_first_receipt_ms"].as_u64()
            == receipts[0]["elapsed_us"]
                .as_u64()
                .map(|elapsed| elapsed / 1000)
            && target["request_to_final_receipt_ms"].as_u64() == Some(previous_us / 1000),
        "target receipt timings differ from observations"
    );
    Ok(trial)
}

fn compatible_conditions(current: &Run, recorded: &Run) -> anyhow::Result<()> {
    ensure!(
        current.kind == recorded.kind
            && current.fixture_sha256 == recorded.fixture_sha256
            && current.warmups == recorded.warmups
            && current.expected_backends == recorded.expected_backends
            && current.provenance.host == recorded.provenance.host
            && current.provenance.os == recorded.provenance.os
            && current.provenance.arch == recorded.provenance.arch
            && current.provenance.debug_assertions == recorded.provenance.debug_assertions,
        "incompatible benchmark conditions"
    );
    Ok(())
}

fn compatible_entry<'a>(entry: &Observation, recorded: &'a Run) -> anyhow::Result<&'a Observation> {
    let before = recorded
        .entries
        .iter()
        .find(|before| before.backend == entry.backend)
        .context("baseline backend missing")?;
    ensure!(
        entry.configuration == before.configuration && entry.model_sha256 == before.model_sha256,
        "{} configuration or model differs",
        entry.backend
    );
    Ok(before)
}

fn compare(current: &Run, recorded: &Run, metric: &str, margin: f64) -> anyhow::Result<()> {
    validate(current)?;
    validate(recorded)?;
    compatible_conditions(current, recorded)?;
    let mut failed = Vec::new();
    for entry in &current.entries {
        let before = compatible_entry(entry, recorded)?;
        if current.require_same_text {
            let current_text: Vec<_> = entry
                .delivery_traces
                .iter()
                .skip(current.warmups as usize)
                .map(|row| &row["text"])
                .collect();
            let baseline_text: Vec<_> = before
                .delivery_traces
                .iter()
                .skip(recorded.warmups as usize)
                .map(|row| &row["text"])
                .collect();
            ensure!(
                !current_text.is_empty() && current_text == baseline_text,
                "{} delivered text changed (including missing/duplicated words)",
                entry.backend
            );
        }
        let baseline_samples = before
            .samples_ms
            .get(metric)
            .context("baseline metric missing")?;
        let candidate_samples = entry
            .samples_ms
            .get(metric)
            .context("candidate metric missing")?;
        let interval = stats::difference_interval(baseline_samples, candidate_samples);
        let decision = stats::Decision::of(interval, margin);
        println!(
            "{} {metric}: {} (candidate minus baseline median, 95% interval {interval:?} ms; allowed increase {margin} ms)",
            entry.backend,
            decision.label()
        );
        if decision != stats::Decision::Accepted {
            failed.push(entry.backend.clone());
        }
    }
    ensure!(
        failed.is_empty(),
        "non-regression not established for {}",
        failed.join(", ")
    );
    Ok(())
}

pub(crate) fn stream(backend: &str, stride_ms: Option<usize>) -> anyhow::Result<Stream> {
    if let Some(stride) = stride_ms {
        ensure!(
            stride > 0 && stride < 20000,
            "agreement stride must be between 1 and 19999 ms"
        );
    }
    let cadence = match backend {
        backend::WHISPER => aria_stt::whisper::CADENCE,
        backend::MOONSHINE => aria_stt::moonshine::CADENCE,
        backend::PARAKEET => aria_stt::parakeet::CADENCE,
        backend::NEMOTRON => {
            ensure!(
                stride_ms.is_none(),
                "Nemotron native streaming does not accept an agreement stride"
            );
            return Ok(Stream::Native);
        }
        _ => anyhow::bail!("{backend} has no shipping first-text streaming mode"),
    };
    Ok(Stream::Agreement(stride_ms.map_or(cadence, |stride| {
        Cadence::from_millis(stride, 20000)
    })))
}

pub(crate) fn stream_description(stream: Stream) -> String {
    match stream {
        Stream::Agreement(cadence) => format!(
            "agreement; stride_samples={}; max_buffer_samples={}",
            cadence.stride_samples(),
            cadence.max_buffer_samples()
        ),
        Stream::Native => "native".into(),
    }
}

#[cfg(test)]
#[allow(dead_code, unused_imports)]
mod tests {
    use super::*;

    fn run(samples: &[u64]) -> Run {
        Run {
            output: None,
            schema: 1,
            kind: "first-text".into(),
            provenance: Provenance {
                run_id: "test".into(),
                host: "host".into(),
                os: "os".into(),
                arch: "arch".into(),
                executable_sha256: "test-binary".into(),
                debug_assertions: false,
            },
            daemon_sha256: None,
            fixture_sha256: "fixture".into(),
            runs: samples.len() as u32,
            warmups: 1,
            expected_backends: vec!["whisper".into()],
            entries: vec![Observation {
                backend: "whisper".into(),
                configuration: "agreement".into(),
                model_sha256: Some("model".into()),
                startup_ms: Some(500),
                samples_ms: BTreeMap::from([("first-finalized".into(), samples.to_vec())]),
                warmup_ms: BTreeMap::from([("first-finalized".into(), vec![1000])]),
                daemon_log: None,
                delivery_traces: Vec::new(),
                vocabulary: None,
            }],
            failure: None,
            require_same_text: false,
        }
    }

    #[test]
    fn output_collisions_fail_before_collection() {
        let directory = tempfile::tempdir().unwrap();
        let path = prepare_output(directory.path(), "first-text").unwrap();
        artifact::write_json(&path, &run(&[100; 3])).unwrap();
        assert!(prepare_output(directory.path(), "first-text").is_err());
        let dangling = directory.path().join("dictation-latency.json");
        std::os::unix::fs::symlink(directory.path().join("missing"), dangling).unwrap();
        assert!(prepare_output(directory.path(), "dictation-latency").is_err());
    }

    #[test]
    fn delivery_comparison_rejects_delay_and_lost_or_duplicated_words() {
        fn delivery(samples: &[u64]) -> Run {
            let mut run = run(samples);
            run.kind = "dictation-latency".into();
            run.daemon_sha256 = Some("daemon".into());
            run.require_same_text = true;
            let entry = &mut run.entries[0];
            entry.configuration = "rpc-delivery-v1".into();
            for metric in [
                "recording",
                "transcription",
                "delivery",
                "total (daemon)",
                "release->delivered",
                "client wall",
                "capture->delivered",
            ] {
                entry.samples_ms.insert(metric.into(), samples.to_vec());
                entry.warmup_ms.insert(metric.into(), vec![100]);
            }
            entry.delivery_traces = (0..samples.len()+1).map(|_| serde_json::json!({"text": "open aria now", "clipboard_text": "open aria now"})).collect();
            run
        }
        let baseline = delivery(&[100; 30]);
        assert!(compare(&delivery(&[100; 30]), &baseline, "capture->delivered", 5.0).is_ok());
        assert!(compare(&delivery(&[150; 30]), &baseline, "capture->delivered", 5.0).is_err());
        for text in ["open aria", "open aria now now"] {
            let mut candidate = delivery(&[100; 30]);
            candidate.entries[0].delivery_traces[1] =
                serde_json::json!({"text": text, "clipboard_text": text});
            assert!(compare(&candidate, &baseline, "capture->delivered", 5.0).is_err());
        }
    }

    fn streaming() -> Run {
        let mut run = run(&[100; 30]);
        run.kind = "dictation-latency".into();
        run.daemon_sha256 = Some("daemon".into());
        run.require_same_text = true;
        let entry = &mut run.entries[0];
        entry.configuration =
            "agreement; insert; streaming; manual; language=en; rpc-delivery-v1".into();
        entry.samples_ms.clear();
        entry.warmup_ms.clear();
        for metric in [
            "recording",
            "transcription",
            "total (daemon)",
            "client wall",
            "capture->delivered",
            "capture->first-segment",
        ] {
            entry.samples_ms.insert(metric.into(), vec![100; 30]);
            entry.warmup_ms.insert(metric.into(), vec![100]);
        }
        entry.delivery_traces = (0..31)
            .map(|_| {
                serde_json::json!({
                    "text": "open aria now", "target_appended_text": "open aria now",
                    "capture_to_first_segment_ms": 100,
                    "corrected_segments": [{"text": "open aria"}, {"text": "now"}]
                })
            })
            .collect();
        run
    }

    #[test]
    fn streaming_comparison_requires_first_segments_and_target_readback() {
        let baseline = streaming();
        assert!(compare(&streaming(), &baseline, "capture->first-segment", 5.0).is_ok());
        assert!(compare(&streaming(), &baseline, "capture->delivered", 5.0).is_ok());
        assert!(compare(&streaming(), &baseline, "release->delivered", 5.0).is_err());
        for field in [
            "capture_to_first_segment_ms",
            "target_appended_text",
            "corrected_segments",
        ] {
            let mut candidate = streaming();
            candidate.entries[0].delivery_traces[1][field] = serde_json::Value::Null;
            assert!(compare(&candidate, &baseline, "capture->first-segment", 5.0).is_err());
        }
        for (field, value) in [
            ("target_appended_text", serde_json::json!("open arianow")),
            ("capture_to_first_segment_ms", serde_json::json!(99)),
            (
                "corrected_segments",
                serde_json::json!([{"text": "open"}, {"text": "now"}]),
            ),
        ] {
            let mut candidate = streaming();
            candidate.entries[0].delivery_traces[1][field] = value;
            assert!(validate(&candidate).is_err());
        }
        let mut candidate = streaming();
        candidate.entries[0]
            .samples_ms
            .remove("capture->first-segment");
        assert!(validate(&candidate).is_err());
    }

    #[test]
    fn streaming_receipts_reject_corrupt_observations_and_samples() {
        fn observed() -> Run {
            let mut run = streaming();
            let entry = &mut run.entries[0];
            entry.configuration = entry
                .configuration
                .replace("rpc-delivery-v1", "rpc-delivery-v2");
            for (metric, duration) in [
                ("request->first-receipt", 100),
                ("request->final-receipt", 200),
            ] {
                entry.samples_ms.insert(metric.into(), vec![duration; 30]);
                entry.warmup_ms.insert(metric.into(), vec![duration]);
            }
            for (index, row) in entry.delivery_traces.iter_mut().enumerate() {
                row["text"] = serde_json::json!("héllo world");
                row["corrected_segments"] =
                    serde_json::json!([{"text": "héllo"}, {"text": "world"}]);
                row["target"] = serde_json::json!({
                    "trial": index + 40,
                    "clock": "CLOCK_UPTIME_RAW",
                    "text": "héllo world",
                    "request_to_first_receipt_ms": 100,
                    "request_to_final_receipt_ms": 200,
                    "receipts": [
                        {"elapsed_us": 100_000, "gap_us": null, "utf8_bytes": 6},
                        {"elapsed_us": 200_000, "gap_us": 100_000, "utf8_bytes": 12}
                    ]
                });
                row.as_object_mut().unwrap().remove("target_appended_text");
            }
            run
        }
        let baseline = observed();
        assert!(compare(&observed(), &baseline, "request->first-receipt", 5.0).is_ok());
        assert!(compare(&observed(), &baseline, "request->final-receipt", 5.0).is_ok());
        let mut delayed = observed();
        let entry = &mut delayed.entries[0];
        entry
            .samples_ms
            .insert("request->final-receipt".into(), vec![300; 30]);
        entry
            .warmup_ms
            .insert("request->final-receipt".into(), vec![300]);
        for row in &mut entry.delivery_traces {
            row["target"]["request_to_final_receipt_ms"] = serde_json::json!(300);
            row["target"]["receipts"][1]["elapsed_us"] = serde_json::json!(300_000);
            row["target"]["receipts"][1]["gap_us"] = serde_json::json!(200_000);
        }
        assert!(compare(&delayed, &baseline, "request->first-receipt", 5.0).is_ok());
        assert!(compare(&delayed, &baseline, "request->final-receipt", 5.0).is_err());
        for (path, value) in [
            ("/trial", serde_json::json!(0)),
            ("/trial", serde_json::json!(40)),
            ("/clock", serde_json::json!("unix_us")),
            ("/text", serde_json::json!("hello world")),
            ("/receipts", serde_json::json!([])),
            ("/receipts/0/gap_us", serde_json::json!(0)),
            ("/receipts/1/elapsed_us", serde_json::json!(99_999)),
            ("/receipts/1/gap_us", serde_json::json!(99_998)),
            ("/receipts/1/utf8_bytes", serde_json::json!(6)),
            ("/receipts/1/utf8_bytes", serde_json::json!(11)),
            ("/request_to_first_receipt_ms", serde_json::json!(99)),
            ("/request_to_final_receipt_ms", serde_json::json!(199)),
        ] {
            let mut candidate = observed();
            *candidate.entries[0].delivery_traces[1]["target"]
                .pointer_mut(path)
                .unwrap() = value;
            assert!(
                validate(&candidate).is_err(),
                "accepted corrupt target {path}"
            );
        }
        for metric in ["request->first-receipt", "request->final-receipt"] {
            for warmup in [false, true] {
                let mut candidate = observed();
                let series = if warmup {
                    &mut candidate.entries[0].warmup_ms
                } else {
                    &mut candidate.entries[0].samples_ms
                };
                series.get_mut(metric).unwrap()[0] += 1;
                assert!(
                    validate(&candidate).is_err(),
                    "accepted inconsistent {metric}"
                );
                let series = if warmup {
                    &mut candidate.entries[0].warmup_ms
                } else {
                    &mut candidate.entries[0].samples_ms
                };
                series.remove(metric);
                assert!(validate(&candidate).is_err(), "accepted missing {metric}");
            }
        }
        let mut candidate = observed();
        candidate.entries[0].delivery_traces[0]["target"]["receipts"][1]["gap_us"] =
            serde_json::json!(99_999);
        assert!(validate(&candidate).is_ok());
        let mut second = observed().entries.pop().unwrap();
        second.backend = "moonshine".into();
        for row in &mut second.delivery_traces {
            row["target"]["trial"] =
                serde_json::json!(row["target"]["trial"].as_u64().unwrap() + 31);
        }
        candidate.expected_backends.push(second.backend.clone());
        candidate.entries.push(second);
        assert!(validate(&candidate).is_ok());
        candidate.entries[1].delivery_traces[0]["target"]["trial"] = serde_json::json!(70);
        assert!(validate(&candidate).is_err());
    }

    #[test]
    fn incompatible_conditions_fail_without_measured_samples() {
        let options = Options {
            runs: 3,
            warmups: 1,
            baseline: Some(run(&[100; 10])),
            margin_ms: 0.0,
        };
        let mut candidate = run(&[]);
        assert!(options.preflight(&candidate).is_ok());
        assert!(options.preflight_engine(&candidate.entries[0]).is_ok());
        candidate.fixture_sha256 = "changed fixture".into();
        assert!(options.preflight(&candidate).is_err());
        candidate.entries[0].configuration = "changed cadence".into();
        assert!(options.preflight_engine(&candidate.entries[0]).is_err());
    }

    #[test]
    fn repetition_count_does_not_disable_comparison() {
        let baseline = run(&[100; 10]);
        assert!(compare(&run(&[90; 3]), &baseline, "first-finalized", 0.0).is_ok());
        assert!(compare(&run(&[110; 3]), &baseline, "first-finalized", 0.0).is_err());
        assert!(compare(&run(&[110; 3]), &baseline, "first-finalized", 10.0).is_ok());
        assert!(compare(&run(&[90]), &baseline, "first-finalized", 0.0).is_err());
    }

    #[test]
    fn every_daemon_stage_is_required_even_when_only_the_tail_is_gated() {
        let mut artifact = run(&[90; 3]);
        artifact.kind = "dictation-latency".into();
        artifact.daemon_sha256 = Some("daemon".into());
        let observation = &mut artifact.entries[0];
        for name in [
            "recording",
            "transcription",
            "delivery",
            "total (daemon)",
            "release->delivered",
            "client wall",
        ] {
            observation.samples_ms.insert(name.into(), vec![90; 3]);
            observation.warmup_ms.insert(name.into(), vec![1000]);
        }
        assert!(validate(&artifact).is_ok());
        artifact.entries[0].samples_ms.remove("delivery");
        assert!(validate(&artifact).is_err());
    }

    #[test]
    fn incomplete_or_unidentified_evidence_cannot_pass() {
        let baseline = run(&[100; 3]);
        let mut candidate = run(&[90; 3]);
        candidate.entries[0].warmup_ms.clear();
        assert!(compare(&candidate, &baseline, "first-finalized", 0.0).is_err());
        let mut candidate = run(&[90; 3]);
        candidate.entries[0].model_sha256 = None;
        assert!(compare(&candidate, &baseline, "first-finalized", 0.0).is_err());
        let mut candidate = run(&[90; 3]);
        candidate.fixture_sha256 = "changed audio".into();
        assert!(compare(&candidate, &baseline, "first-finalized", 0.0).is_err());
        let mut candidate = run(&[90; 3]);
        candidate.failure = Some("capture failed".into());
        assert!(compare(&candidate, &baseline, "first-finalized", 0.0).is_err());
    }
}
