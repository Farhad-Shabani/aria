use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use aria::audio::{AudioCapture, RecordingMode, ReplayPace, StopSignal, Stopper, WavReplayCapture};
use aria::config::TranscriptionConfig;
use aria::transcription::{Language, LanguageCode, Stream, Streaming, Transcriber, Transcription};
use aria_harness::{inputs, stats};

mod engines;

const BENCH: &str = "first text";
const METRIC: &str = "first-finalized";
const STRIDE_ENV: &str = "ARIA_TEST_STRIDE_MS";
const MAXIMUM_UTTERANCE: Duration = Duration::from_secs(120);

fn main() -> anyhow::Result<()> {
    let options = engines::Options::from_environment()?;
    if options.offline("first-text", METRIC)? {
        return Ok(());
    }
    let Some(inputs) = BenchInputs::from_environment()? else {
        return options.missing_inputs(BENCH);
    };
    let mut run = engines::Run::new(
        "first-text",
        &inputs.wav,
        inputs
            .engines
            .iter()
            .map(|engine| engine.backend.into())
            .collect(),
        &options,
    )?;
    let outcome: anyhow::Result<()> = (|| {
        options.preflight(&run)?;
        for engine in &inputs.engines {
            let stream = engines::stream(engine.backend, inputs.stride_ms)?;
            let mode = engines::stream_description(stream);
            println!(
                "{}: {mode}, realtime, {} warmups, {} measured runs",
                engine.backend, options.warmups, options.runs
            );
            let mut observation = engines::Observation::new(
                engine,
                &format!(
                    "{mode}; realtime; language=en; stop=first-finalized; max_utterance_s=120; timer=capture-start"
                ),
            )?;
            let result = options
                .preflight_engine(&observation)
                .and_then(|()| measure(&inputs, engine, stream, &options, &mut observation));
            run.entries.push(observation);
            result?;
            report(
                engine.backend,
                &run.entries.last().expect("entry was pushed").samples_ms[METRIC],
                options.runs,
            )?;
        }
        Ok(())
    })();
    run.failure = outcome.as_ref().err().map(|error| format!("{error:#}"));
    run.save()?;
    outcome?;
    options.compare(&run, METRIC)
}

struct BenchInputs {
    engines: Vec<engines::BenchEngine>,
    wav: PathBuf,
    stride_ms: Option<usize>,
}

impl BenchInputs {
    fn from_environment() -> anyhow::Result<Option<Self>> {
        let mut engines = engines::discover()?;
        if std::env::var_os("ARIA_BENCH_BACKENDS").is_some() {
            for engine in &engines {
                engines::stream(engine.backend, None)?;
            }
        }
        engines.retain(|engine| engines::stream(engine.backend, None).is_ok());
        if engines.is_empty() {
            return Ok(None);
        }
        let Some(wav) = inputs::fixture(BENCH, engines::WAV_ENV)? else {
            return Ok(None);
        };
        let stride_ms = std::env::var(STRIDE_ENV)
            .ok()
            .map(|value| {
                value
                    .parse::<usize>()
                    .context("ARIA_TEST_STRIDE_MS must be a positive integer")
            })
            .transpose()?;
        if let Some(stride) = stride_ms {
            ensure!(
                stride > 0 && stride < 20000,
                "ARIA_TEST_STRIDE_MS must be between 1 and 19999"
            );
        }
        Ok(Some(Self {
            engines,
            wav,
            stride_ms,
        }))
    }
}

fn measure(
    inputs: &BenchInputs,
    engine: &engines::BenchEngine,
    stream: Stream,
    options: &engines::Options,
    observation: &mut engines::Observation,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let transcriber: Box<dyn Transcriber> = match &engine.config {
        TranscriptionConfig::Whisper { model } => {
            Box::new(aria_stt::whisper::Whisper::load(model.clone())?)
        }
        TranscriptionConfig::Moonshine { model_directory } => Box::new(
            aria_stt::moonshine::Moonshine::load(model_directory.clone())?,
        ),
        TranscriptionConfig::Parakeet {
            model_directory,
            device,
        } => Box::new(aria_stt::parakeet::Parakeet::load(
            model_directory.clone(),
            *device,
        )?),
        TranscriptionConfig::Nemotron { model_directory } => {
            Box::new(aria_stt::nemotron::Nemotron::load(model_directory.clone())?)
        }
        _ => unreachable!("input discovery filters unsupported modes"),
    };
    observation.startup_ms = Some(started.elapsed().as_millis() as u64);
    let transcription = Transcription::stream(stream, transcriber);
    let language = Language::Specific(LanguageCode::english());
    for run in 0..options.warmups + options.runs {
        let elapsed = first_text_ms(&transcription, &language, &inputs.wav, Instant::now)
            .with_context(|| {
                format!(
                    "{} run {} ({})",
                    engine.backend,
                    run + 1,
                    if run < options.warmups {
                        "warmup"
                    } else {
                        "measured"
                    }
                )
            })?;
        let series = if run < options.warmups {
            &mut observation.warmup_ms
        } else {
            &mut observation.samples_ms
        };
        series.entry(METRIC.into()).or_default().push(elapsed);
    }
    Ok(())
}

fn first_text_ms<T: Transcriber>(
    transcription: &Transcription<T, Streaming>,
    language: &Language,
    wav: &Path,
    now: impl FnOnce() -> Instant,
) -> anyhow::Result<u64> {
    let spool = tempfile::TempDir::new().context("cannot create replay spool")?;
    let stopper = Stopper::new();
    let capture =
        WavReplayCapture::new(spool.path(), MAXIMUM_UTTERANCE, wav, ReplayPace::Realtime)?;
    let started = now();
    let mut recording = capture.start(
        RecordingMode::UntilStopped,
        StopSignal::from(stopper.clone()),
    )?;
    // Every exit after capture starts must stop replay, including initialization failures.
    let result = (|| {
        let mut operation = transcription.start(language)?;
        let segment = operation.next_finalized(&mut recording)?;
        ensure!(segment.is_some(), "no segment was finalized");
        Ok(started.elapsed().as_millis() as u64)
    })();
    stopper.stop();
    result
}

fn report(backend: &str, samples: &[u64], expected: u32) -> anyhow::Result<()> {
    ensure!(
        samples.len() == expected as usize,
        "expected {expected} {backend:?} samples, found {}",
        samples.len()
    );
    let summary = stats::summarize(samples).context("no measured samples")?;
    println!(
        "{backend} capture start -> first finalized: {}/{} samples, min {} ms, median {} ms, max {} ms",
        summary.count, expected, summary.min, summary.median, summary.max
    );
    Ok(())
}

#[cfg(test)]
#[allow(dead_code, unused_imports)]
mod tests {
    use super::*;
    use aria::config::backend;
    use aria::transcription::{Hypothesis, TranscriptionAudio, TranscriptionError};
    use aria_kit::cancel::CancelToken;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    struct InitializationProbe(Arc<AtomicBool>);

    impl Transcriber for InitializationProbe {
        fn validate_language(
            &self,
            _: &Language,
            _: &CancelToken,
        ) -> Result<(), TranscriptionError> {
            assert!(
                self.0.load(Ordering::Relaxed),
                "initialization must be timed"
            );
            Err(TranscriptionError::NoNativeStream)
        }

        fn transcribe_with(
            &self,
            _: TranscriptionAudio<'_>,
            _: &Language,
            _: &CancelToken,
        ) -> Result<Hypothesis, TranscriptionError> {
            unreachable!("initialization fails before decoding")
        }
    }

    #[test]
    fn first_text_times_initialization_and_cleans_up_its_failure() {
        let directory = tempfile::tempdir().unwrap();
        let wav = aria::audio::fixture_of(directory.path(), 2);
        let clock_started = Arc::new(AtomicBool::new(false));
        let transcription =
            Transcription::stream(Stream::Native, InitializationProbe(clock_started.clone()));
        let error = first_text_ms(&transcription, &Language::Automatic, &wav, || {
            clock_started.store(true, Ordering::Relaxed);
            Instant::now()
        })
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<TranscriptionError>(),
            Some(TranscriptionError::NoNativeStream)
        ));
    }

    #[test]
    fn shipping_modes_and_override_constraints_are_explicit() {
        for (backend, expected) in [
            (backend::WHISPER, aria_stt::whisper::CADENCE),
            (backend::MOONSHINE, aria_stt::moonshine::CADENCE),
            (backend::PARAKEET, aria_stt::parakeet::CADENCE),
        ] {
            let Stream::Agreement(cadence) = engines::stream(backend, None).unwrap() else {
                panic!("expected agreement");
            };
            assert_eq!(cadence.stride_samples(), expected.stride_samples());
        }
        assert!(matches!(
            engines::stream(backend::NEMOTRON, None),
            Ok(Stream::Native)
        ));
        assert!(engines::stream(backend::NEMOTRON, Some(1000)).is_err());
        assert!(engines::stream(backend::WHISPER_CANDLE, None).is_err());
    }
}
