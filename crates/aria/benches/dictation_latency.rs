use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use aria::config::{TranscriptionConfig, TranscriptionDevice, backend};
use aria_harness::{Sandbox, inputs, log, start_service, stats, yaml_string};

mod dictation_delivery;
mod engines;

const BENCH: &str = "dictation latency";
const RELEASE_TO_DELIVERED: &str = "release->delivered";

fn main() -> anyhow::Result<()> {
    let options = engines::Options::from_environment()?;
    let streaming = match std::env::var("ARIA_BENCH_STREAMING").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("0") => false,
        Ok("1") => true,
        _ => bail!("ARIA_BENCH_STREAMING must be 0 or 1"),
    };
    let metric = if streaming {
        "request->first-receipt"
    } else {
        RELEASE_TO_DELIVERED
    };
    if options.offline("dictation-latency", metric)? {
        return Ok(());
    }
    let Some(inputs) = BenchInputs::from_environment()? else {
        return options.missing_inputs(BENCH);
    };
    announce(&inputs, &options, streaming);
    if streaming {
        println!(
            "Streaming Insert, manual stop at WAV EOF; segment events contain corrected output, not raw capture ASR."
        );
    }
    let binary = inputs::path_input("ARIA_BENCH_DAEMON")?
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_aria")));
    let preparation = dictation_delivery::Preparation::load(streaming)?;
    let mut run = engines::Run::new(
        "dictation-latency",
        &inputs.wav,
        inputs
            .engines
            .iter()
            .map(|engine| engine.backend.into())
            .collect(),
        &options,
    )?;
    run.daemon_sha256 = Some(aria_harness::artifact::fingerprint(&binary)?);
    let outcome: anyhow::Result<()> = (|| {
        options.preflight(&run)?;
        let mut target = streaming
            .then(dictation_delivery::target::Target::launch)
            .transpose()?;
        for engine in &inputs.engines {
            println!("--- {} ---", engine.backend);
            let mode = match engine.backend {
                backend::WHISPER_CANDLE | backend::PARAKEET_MLX => "whole".into(),
                _ => engines::stream_description(engines::stream(engine.backend, None)?),
            };
            let mut observation = engines::Observation::new(
                engine,
                &format!(
                    "{mode}; realtime; {}; language=en; {}; personal={}",
                    if streaming {
                        "insert; streaming; manual"
                    } else {
                        "copy"
                    },
                    if streaming {
                        "rpc-delivery-v2"
                    } else {
                        "rpc-delivery-v1"
                    },
                    preparation.personal_identity()?
                ),
            )?;
            let result = (|| {
                options.preflight_engine(&observation)?;
                let sandbox = Sandbox::create(&inputs.config(engine)?)?;
                preparation.seed(&sandbox)?;
                measure(
                    &binary,
                    &sandbox,
                    &options,
                    &mut observation,
                    &preparation,
                    target.as_mut(),
                )?;
                report(&observation, options.runs)
            })();
            run.entries.push(observation);
            result?;
        }
        if let Some(target) = target {
            target.stop()?;
        }
        Ok(())
    })();
    run.failure = outcome.as_ref().err().map(|error| format!("{error:#}"));
    run.save()?;
    outcome?;
    options.compare(&run, metric)?;
    if streaming {
        options.compare(&run, "request->final-receipt")?;
    }
    options.compare(&run, "capture->delivered")
}

struct BenchInputs {
    engines: Vec<engines::BenchEngine>,
    wav: PathBuf,
}

impl BenchInputs {
    fn from_environment() -> anyhow::Result<Option<Self>> {
        let discovered = engines::discover()?;
        if discovered.is_empty() {
            println!("skipping the {BENCH} bench: no ARIA_TEST_*_MODEL is set");
            return Ok(None);
        }
        let Some(wav) = inputs::fixture(BENCH, engines::WAV_ENV)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            engines: discovered,
            wav,
        }))
    }

    fn config(&self, engine: &engines::BenchEngine) -> anyhow::Result<String> {
        bench_config(engine, &self.wav)
    }
}

fn announce(inputs: &BenchInputs, options: &engines::Options, streaming: bool) {
    let backends: Vec<&str> = inputs.engines.iter().map(|engine| engine.backend).collect();
    println!(
        "dictation end-to-end bench: {}, {} measured runs, {} warmups each, fixture {}",
        backends.join(", "),
        options.runs,
        options.warmups,
        inputs.wav.display()
    );
    if !streaming {
        println!(
            "note: the fixture replays at 1x, so `release->delivered` is what a \
             speaker waits through after they stop talking\n"
        );
    }
}

fn measure(
    binary: &Path,
    sandbox: &Sandbox,
    options: &engines::Options,
    observation: &mut engines::Observation,
    preparation: &dictation_delivery::Preparation,
    mut target: Option<&mut dictation_delivery::target::Target>,
) -> anyhow::Result<()> {
    let startup_started = Instant::now();
    let daemon = start_service(binary, sandbox)?.into_running()?;
    observation.startup_ms = Some(startup_started.elapsed().as_millis() as u64);
    let mut client_wall = Vec::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let outcome: anyhow::Result<()> = runtime.block_on(async {
        let channel = dictation_delivery::connect(&sandbox.socket_path()).await?;
        observation.vocabulary = tokio::time::timeout(
            Duration::from_secs(120),
            preparation.prepare(channel.clone()),
        )
        .await??;
        for index in 0..options.runs + options.warmups {
            let trace = dictation_delivery::record(channel.clone(), target.as_deref_mut()).await?;
            client_wall.push(Duration::from_millis(
                trace["client_ms"]
                    .as_u64()
                    .context("missing client duration")?,
            ));
            let samples = if index < options.warmups {
                &mut observation.warmup_ms
            } else {
                &mut observation.samples_ms
            };
            samples
                .entry("capture->delivered".into())
                .or_default()
                .push(
                    trace["capture_to_delivered_ms"]
                        .as_u64()
                        .context("missing delivery duration")?,
                );
            if target.is_some() {
                samples
                    .entry("capture->first-segment".into())
                    .or_default()
                    .push(
                        trace["capture_to_first_segment_ms"]
                            .as_u64()
                            .context("missing first segment duration")?,
                    );
                for (metric, field) in [
                    ("request->first-receipt", "request_to_first_receipt_ms"),
                    ("request->final-receipt", "request_to_final_receipt_ms"),
                ] {
                    samples.entry(metric.into()).or_default().push(
                        trace["target"][field]
                            .as_u64()
                            .context("missing application receipt timing")?,
                    );
                }
            }
            observation.delivery_traces.push(trace);
        }
        Ok(())
    });
    drop(runtime);
    let stopped = daemon.stop();
    let daemon_log =
        std::fs::read_to_string(sandbox.log_path()).context("failed to read daemon log");
    if let Ok(log) = &daemon_log {
        observation.daemon_log = Some(log.clone());
        for (name, values) in series(log, &client_wall) {
            if target.is_some() && matches!(name, "delivery" | RELEASE_TO_DELIVERED) {
                continue;
            }
            let warmup_count = (options.warmups as usize).min(values.len());
            observation
                .warmup_ms
                .insert(name.into(), values[..warmup_count].to_vec());
            observation
                .samples_ms
                .insert(name.into(), values[warmup_count..].to_vec());
        }
    }
    outcome?;
    stopped?;
    verify_stage_coverage(&daemon_log?)?;
    Ok(())
}

fn bench_config(engine: &engines::BenchEngine, wav: &Path) -> anyhow::Result<String> {
    let wav = yaml_string(&wav.to_string_lossy())?;
    Ok(format!(
        "capture:\n  backend: wav-file\n  path: {wav}\n  pace: realtime\n\
         transcription:\n  backend: {}\n{}",
        engine.backend,
        settings(&engine.config)?
    ))
}

fn settings(config: &TranscriptionConfig) -> anyhow::Result<String> {
    Ok(match config {
        TranscriptionConfig::Whisper { model }
        | TranscriptionConfig::WhisperCandle {
            model_directory: model,
        }
        | TranscriptionConfig::Moonshine {
            model_directory: model,
        }
        | TranscriptionConfig::Nemotron {
            model_directory: model,
        } => {
            format!("  model: {}\n", yaml_string(&model.display().to_string())?)
        }
        TranscriptionConfig::Parakeet {
            model_directory,
            device,
        } => format!(
            "  model: {}\n  device: {}\n",
            yaml_string(&model_directory.display().to_string())?,
            match device {
                TranscriptionDevice::Cpu => "cpu",
                TranscriptionDevice::Gpu => "gpu",
            }
        ),
        TranscriptionConfig::ParakeetMlx { model, interpreter } => {
            format!(
                "  model: {}\n  interpreter: {}\n",
                yaml_string(model)?,
                yaml_string(&interpreter.display().to_string())?
            )
        }
    })
}

fn report(observation: &engines::Observation, runs: u32) -> anyhow::Result<()> {
    println!(
        "daemon startup (single observation, spawn -> ready): {} ms",
        observation
            .startup_ms
            .context("daemon startup timing missing")?
    );
    println!();
    println!(
        "{:<16} {:>5} {:>9} {:>9} {:>9}",
        "series", "runs", "min ms", "med ms", "max ms"
    );
    for (name, samples) in &observation.samples_ms {
        print_series(name, samples, runs)?;
    }
    Ok(())
}

fn verify_stage_coverage(log: &str) -> anyhow::Result<()> {
    let unknown: Vec<String> = log::stage_names(log)
        .into_iter()
        .filter(|name| !log::STAGES.contains(&name.as_str()))
        .collect();
    if !unknown.is_empty() {
        bail!("daemon log mentions stages this bench does not cover: {unknown:?}");
    }
    Ok(())
}

fn series(log: &str, client_wall: &[Duration]) -> Vec<(&'static str, Vec<u64>)> {
    let mut series: Vec<(&'static str, Vec<u64>)> = log::STAGES
        .iter()
        .map(|stage| (*stage, log::stage_samples(log, stage)))
        .collect();
    series.push(("total (daemon)", log::execution_samples(log)));
    series.push((RELEASE_TO_DELIVERED, log::tail_samples(log)));
    series.push((
        "client wall",
        client_wall
            .iter()
            .map(|duration| duration.as_millis() as u64)
            .collect(),
    ));
    series
}

fn print_series(name: &str, samples: &[u64], expected: u32) -> anyhow::Result<()> {
    if samples.len() != expected as usize {
        bail!(
            "expected {expected} {name:?} samples, found {}",
            samples.len()
        );
    }
    let summary = stats::summarize(samples).expect("count was checked above");
    println!(
        "{:<16} {:>5} {:>9} {:>9} {:>9}",
        name,
        format!("{}/{expected}", summary.count),
        summary.min,
        summary.median,
        summary.max
    );
    Ok(())
}
