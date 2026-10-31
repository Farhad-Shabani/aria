use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use aria::config::{TranscriptionConfig, TranscriptionDevice, backend};
use aria_harness::{Sandbox, inputs, log, start_service, stats, toml_string};

mod dictation_delivery;
mod engines;

const BENCH: &str = "dictation latency";
const RELEASE_TO_DELIVERED: &str = "release->delivered";

fn main() -> anyhow::Result<()> {
    let options = engines::Options::from_environment()?;
    let streaming = inputs::switch("ARIA_BENCH_STREAMING")?;
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
    if preparation.own_voice_only {
        require_own_voice(inputs.speaker.as_ref())?;
    }
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
                    "{mode}; realtime; {}; language=en; {}; personal={}; speaker={}; own_voice_only={}",
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
                    preparation.personal_identity()?,
                    inputs.speaker_identity()?,
                    preparation.own_voice_only
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
    speaker: Option<SpeakerInputs>,
}

struct SpeakerInputs {
    model: PathBuf,
    profiles_dir: PathBuf,
    /// Owner-only dictation refuses to run without it.
    segmentation: Option<PathBuf>,
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
            speaker: speaker_inputs(
                inputs::path_input("ARIA_BENCH_SPEAKER_MODEL")?,
                inputs::path_input("ARIA_BENCH_SPEAKERS_DIR")?,
                inputs::path_input("ARIA_BENCH_SEGMENTATION_MODEL")?,
            )?,
        }))
    }

    fn config(&self, engine: &engines::BenchEngine) -> anyhow::Result<String> {
        bench_config(engine, &self.wav, self.speaker.as_ref())
    }

    fn speaker_identity(&self) -> anyhow::Result<String> {
        let Some(speaker) = &self.speaker else {
            return Ok("disabled".into());
        };
        let segmentation = match &speaker.segmentation {
            Some(model) => aria_harness::artifact::fingerprint(model)?,
            None => "none".into(),
        };
        Ok(format!(
            "{}:{}:{segmentation}",
            aria_harness::artifact::fingerprint(&speaker.model)?,
            aria_harness::artifact::fingerprint(&speaker.profiles_dir)?
        ))
    }
}

fn speaker_inputs(
    model: Option<PathBuf>,
    profiles_dir: Option<PathBuf>,
    segmentation: Option<PathBuf>,
) -> anyhow::Result<Option<SpeakerInputs>> {
    match (model, profiles_dir) {
        (None, None) => {
            ensure!(
                segmentation.is_none(),
                "ARIA_BENCH_SEGMENTATION_MODEL requires ARIA_BENCH_SPEAKER_MODEL and ARIA_BENCH_SPEAKERS_DIR"
            );
            Ok(None)
        }
        (Some(model), Some(profiles_dir)) => {
            ensure!(model.is_file(), "speaker model must be a file");
            ensure!(
                profiles_dir.is_dir(),
                "the speakers directory must be a directory"
            );
            let segmentation = match segmentation {
                Some(segmentation) => {
                    ensure!(segmentation.is_file(), "segmentation model must be a file");
                    Some(segmentation.canonicalize()?)
                }
                None => None,
            };
            Ok(Some(SpeakerInputs {
                model: model.canonicalize()?,
                profiles_dir: profiles_dir.canonicalize()?,
                segmentation,
            }))
        }
        _ => bail!("set both ARIA_BENCH_SPEAKER_MODEL and ARIA_BENCH_SPEAKERS_DIR"),
    }
}

fn require_own_voice(speaker: Option<&SpeakerInputs>) -> anyhow::Result<()> {
    let speaker = speaker.context(
        "ARIA_BENCH_OWN_VOICE_ONLY=1 requires ARIA_BENCH_SPEAKER_MODEL and ARIA_BENCH_SPEAKERS_DIR",
    )?;
    ensure!(
        speaker.segmentation.is_some(),
        "ARIA_BENCH_OWN_VOICE_ONLY=1 requires ARIA_BENCH_SEGMENTATION_MODEL"
    );
    require_owner(&speaker.profiles_dir)
}

fn require_owner(profiles_dir: &Path) -> anyhow::Result<()> {
    use aria::speaker::{ProfileStore, Role};
    use aria_speaker::wespeaker;
    let stored =
        ProfileStore::open(profiles_dir).read_for(wespeaker::NAME, wespeaker::EMBEDDING_DIM)?;
    ensure!(
        stored
            .profiles
            .iter()
            .any(|profile| profile.role == Role::Owner),
        "ARIA_BENCH_OWN_VOICE_ONLY=1 requires a compatible enrolled owner"
    );
    Ok(())
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
    let daemon_log = daemon_log?;
    if preparation.own_voice_only {
        verify_own_voice(&daemon_log)?;
    }
    verify_stage_coverage(&daemon_log)?;
    Ok(())
}

fn bench_config(
    engine: &engines::BenchEngine,
    wav: &Path,
    speaker: Option<&SpeakerInputs>,
) -> anyhow::Result<String> {
    let wav = toml_string(&wav.to_string_lossy())?;
    let mut config = format!(
        "[capture]\nbackend = \"wav_file\"\npath = {wav}\npace = \"realtime\"\n\
         [stt]\nbackend = \"{}\"\n{}",
        engine.backend,
        settings(&engine.config)?
    );
    if let Some(speaker) = speaker {
        config.push_str(&format!(
            "[speaker]\nmodel = {}\nprofiles_dir = {}\n",
            toml_string(&speaker.model.to_string_lossy())?,
            toml_string(&speaker.profiles_dir.to_string_lossy())?,
        ));
        if let Some(segmentation) = &speaker.segmentation {
            config.push_str(&format!(
                "segmentation_model = {}\n",
                toml_string(&segmentation.to_string_lossy())?,
            ));
        }
    }
    Ok(config)
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
            format!("model = {}\n", toml_string(&model.display().to_string())?)
        }
        TranscriptionConfig::Parakeet {
            model_directory,
            device,
        } => format!(
            "model = {}\ndevice = \"{}\"\n",
            toml_string(&model_directory.display().to_string())?,
            match device {
                TranscriptionDevice::Cpu => "cpu",
                TranscriptionDevice::Gpu => "gpu",
            }
        ),
        TranscriptionConfig::ParakeetMlx { model, interpreter } => {
            format!(
                "model = {}\ninterpreter = {}\n",
                toml_string(model)?,
                toml_string(&interpreter.display().to_string())?
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

fn verify_own_voice(log: &str) -> anyhow::Result<()> {
    ensure!(
        log.contains("speaker model loaded"),
        "own-voice benchmark has no evidence that the daemon loaded its speaker model"
    );
    for failure in [
        "no speaker model; the transcript stays unlabeled",
        "speaker model unusable; the transcript stays unlabeled",
        "no voice-activity model; audio is not speech-gated",
        "the speakers directory is unreadable; nobody is recognized",
        "skipping an unreadable speaker profile",
        "own-voice filtering lacks a model",
        "own-voice filtering is unavailable",
        "own-voice filtering has no continuity verifier",
        "the turn thread did not start",
        "the turn thread panicked",
        "speaker embedding failed",
        "voice-activity inference failed",
        "the voice timeline could not score audio",
    ] {
        ensure!(
            !log.contains(failure),
            "own-voice benchmark encountered a daemon fallback or failure: {failure}"
        );
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

#[cfg(test)]
#[allow(
    dead_code,
    unused_imports,
    reason = "a bench without the test harness compiles this module but drops its tests"
)]
mod tests {
    use super::*;

    #[test]
    fn own_voice_requires_successful_loading_without_filter_fallbacks() {
        let loaded = "INFO aria::speaker: speaker model loaded engine=\"wespeaker\" load_ms=7\n";
        assert!(verify_own_voice(loaded).is_ok());
        assert!(verify_own_voice("INFO aria: daemon ready\n").is_err());
        for failure in [
            "INFO aria::speaker: no speaker model; the transcript stays unlabeled",
            "WARN aria::speaker: speaker model unusable; the transcript stays unlabeled",
            "WARN aria::speaker: no voice-activity model; audio is not speech-gated",
            "WARN aria::speaker: the speakers directory is unreadable; nobody is recognized",
            "WARN aria::speaker: skipping an unreadable speaker profile",
            "WARN aria::dictation::pipeline: own-voice filtering lacks a model speech=true diarizer=true segmenter=false",
            "WARN aria::dictation::pipeline: own-voice filtering is unavailable",
            "WARN aria_kit::speaker::turns::tracker: own-voice filtering has no continuity verifier",
            "ERROR aria::hearing: the turn thread did not start",
            "ERROR aria::hearing::turns: the turn thread panicked; nothing more passes",
            "WARN aria_kit::speaker: speaker embedding failed",
            "WARN aria_kit::speaker::turns: voice-activity inference failed; no more speech can be told apart",
            "WARN aria_kit::speaker: the voice timeline could not score audio",
        ] {
            assert!(verify_own_voice(&format!("{loaded}{failure}\n")).is_err());
        }
        assert!(verify_own_voice(&format!(
            "{loaded}WARN aria::dictation::pipeline: dictation cleanup failed; delivering deterministic text\n"
        )).is_ok());
    }

    #[test]
    fn speaker_inputs_require_both_existing_paths() {
        let directory = tempfile::tempdir().unwrap();
        let model = directory.path().join("speaker.onnx");
        std::fs::write(&model, []).unwrap();
        let profiles_dir = || Some(directory.path().to_path_buf());
        assert!(speaker_inputs(Some(model.clone()), None, None).is_err());
        assert!(speaker_inputs(None, profiles_dir(), None).is_err());
        assert!(speaker_inputs(Some(model.clone()), Some(model.clone()), None).is_err());
        assert!(speaker_inputs(None, None, Some(model.clone())).is_err());
        assert!(speaker_inputs(Some(model.clone()), profiles_dir(), profiles_dir()).is_err());
        let speaker = speaker_inputs(Some(model.clone()), profiles_dir(), Some(model))
            .unwrap()
            .unwrap();
        assert!(speaker.segmentation.is_some());
        assert!(speaker_inputs(None, None, None).unwrap().is_none());
    }

    #[test]
    fn own_voice_requires_speaker_inputs_with_segmentation() {
        assert!(require_own_voice(None).is_err());
        let directory = tempfile::tempdir().unwrap();
        let speaker = SpeakerInputs {
            model: "/models/speaker.onnx".into(),
            profiles_dir: directory.path().into(),
            segmentation: None,
        };
        let error = require_own_voice(Some(&speaker)).unwrap_err();
        assert!(error.to_string().contains("ARIA_BENCH_SEGMENTATION_MODEL"));
    }

    #[test]
    fn own_voice_requires_a_compatible_owner_profile() {
        use aria::speaker::{Profile, ProfileStore, Replace, Role, SpeakerName};
        use aria_speaker::wespeaker;
        let directory = tempfile::tempdir().unwrap();
        let store = ProfileStore::open(directory.path());
        assert!(require_owner(directory.path()).is_err());
        let mut vector = vec![0.0; wespeaker::EMBEDDING_DIM];
        vector[0] = 1.0;
        let mut profile = Profile {
            name: SpeakerName::new("benchmark").unwrap(),
            role: Role::Guest,
            engine: wespeaker::NAME.into(),
            enrolled_at: std::time::SystemTime::UNIX_EPOCH,
            vector,
        };
        store.save(&profile, Replace::Allowed).unwrap();
        assert!(require_owner(directory.path()).is_err());
        profile.role = Role::Owner;
        profile.engine = "incompatible".into();
        store.save(&profile, Replace::Allowed).unwrap();
        assert!(require_owner(directory.path()).is_err());
        profile.engine = wespeaker::NAME.into();
        profile.vector.pop();
        store.save(&profile, Replace::Allowed).unwrap();
        assert!(require_owner(directory.path()).is_err());
        profile.vector.push(0.0);
        store.save(&profile, Replace::Allowed).unwrap();
        assert!(require_owner(directory.path()).is_ok());
    }

    #[test]
    fn speaker_inputs_reach_the_daemon_config() {
        let engine = engines::BenchEngine {
            backend: backend::WHISPER,
            config: TranscriptionConfig::Whisper {
                model: "/models/whisper.bin".into(),
            },
        };
        let speaker = SpeakerInputs {
            model: "/models/speaker.onnx".into(),
            profiles_dir: "/profiles_dir/voice names".into(),
            segmentation: Some("/models/segmentation.onnx".into()),
        };
        let config = bench_config(&engine, Path::new("/speech.wav"), Some(&speaker)).unwrap();
        assert!(config.ends_with(
            "[speaker]\nmodel = \"/models/speaker.onnx\"\n\
             profiles_dir = \"/profiles_dir/voice names\"\n\
             segmentation_model = \"/models/segmentation.onnx\"\n"
        ));
        assert!(
            !bench_config(&engine, Path::new("/speech.wav"), None)
                .unwrap()
                .contains("[speaker]")
        );
    }
}
