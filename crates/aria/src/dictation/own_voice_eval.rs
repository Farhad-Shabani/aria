//! Replays the own-voice corpus (`aria-bench own-voice-corpus`) through the
//! production dictation pipeline with real models, and scores what it typed:
//! the owner's words lost, the other voices' words typed, and how long after
//! each owner turn its words were typed.

mod tracker;

use super::testing::settings;
use super::{
    Cursor, DeliveryMode, DictationError, DictationEvent, DictationEventEmitter, DictationPipeline,
    DictationStage, ExecutionControl, HeardSpeaker, StopMode,
};
use crate::audio::{
    AudioCapture, CaptureError, RecordingMode, RecordingReader, ReplayPace, StopSignal,
    WavReplayCapture,
};
use crate::config::SpeakerConfig;
use crate::delivery::{DeliveryError, DeliveryOutcome, OutputMode, TextDelivery};
use crate::speaker::Diarizer;
use crate::transcription::Transcription;
use aria_harness::own_voice::{ArtifactKind, Placed, SPEAKERS_DIR, Truth};
use aria_kit::cancel::CancelToken;
use aria_kit::speaker::{ProfileStore, Role};
use aria_kit::transcription::{
    Hypothesis, Language, StreamingEngine as _, Transcriber, TranscriptionAudio, TranscriptionError,
};
use aria_kit::voice_activity::SileroVad;
use aria_segmentation::pyannote::Pyannote;
use aria_speaker::wespeaker::WeSpeaker;
use aria_stt::parakeet::{Device, Parakeet};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Share of a turn's own words that must be typed before the turn counts as
/// typed. Below all of them, so one misheard word does not hide the delay.
const TURN_TYPED_SHARE: f64 = 0.8;

/// What acceptance demands, published with every result.
#[derive(serde::Serialize)]
struct Policy {
    minimum_control_owner_reference_recall: f64,
    maximum_added_owner_reference_loss: usize,
    maximum_other_exclusive_reference_output: usize,
    maximum_other_only_output_words: usize,
    maximum_owner_exclusive_repetitions: usize,
    owner_reference_order_required: bool,
}

const POLICY: Policy = Policy {
    minimum_control_owner_reference_recall: TURN_TYPED_SHARE,
    maximum_added_owner_reference_loss: 0,
    maximum_other_exclusive_reference_output: 0,
    maximum_other_only_output_words: 0,
    maximum_owner_exclusive_repetitions: 0,
    owner_reference_order_required: true,
};

#[derive(Default)]
struct Typed {
    /// Each text the cursor typed, timed from the start of capture.
    deliveries: Vec<(Duration, String)>,
    voices: Vec<(Duration, HeardSpeaker)>,
}

struct TimedCapture {
    capture: WavReplayCapture,
    started: Arc<OnceLock<Instant>>,
}

impl AudioCapture for TimedCapture {
    fn start(
        &self,
        mode: RecordingMode,
        stop: StopSignal,
    ) -> Result<RecordingReader, CaptureError> {
        self.started
            .set(Instant::now())
            .expect("one replay per press");
        self.capture.start(mode, stop)
    }
}

/// Types into nothing and records each text with when it was typed.
struct TimedDelivery {
    started: Arc<OnceLock<Instant>>,
    typed: Arc<Mutex<Typed>>,
}

impl TextDelivery for TimedDelivery {
    type Prepared = ();
    const DEFAULT_MODE: OutputMode = OutputMode::Insert;

    const OUTLIVES_PROCESS: bool = true;

    fn frontmost(&self) -> Option<crate::delivery::Application> {
        None
    }

    fn prepare(
        &self,
        _mode: OutputMode,
        _should_cancel: impl Fn() -> bool,
    ) -> Result<(), DeliveryError> {
        Ok(())
    }

    fn finish(
        &self,
        _prepared: &(),
        _should_cancel: impl Fn() -> bool,
    ) -> Result<(), DeliveryError> {
        Ok(())
    }

    fn deliver(
        &self,
        _prepared: &(),
        text: &str,
        _should_cancel: impl Fn() -> bool,
    ) -> Result<DeliveryOutcome, DeliveryError> {
        let at = self.started.get().expect("capture started").elapsed();
        self.typed
            .lock()
            .unwrap()
            .deliveries
            .push((at, text.to_owned()));
        Ok(DeliveryOutcome::Inserted)
    }
}

#[derive(Clone, Copy)]
struct Press {
    name: &'static str,
    own_voice_only: bool,
    delivery: DeliveryMode,
    timed: bool,
}

const CLEAN_OWNER_PRESS: Press = Press {
    name: "clean-owner-off-streaming",
    own_voice_only: false,
    delivery: DeliveryMode::Streaming,
    timed: true,
};

/// The unfiltered press the filtered ones are judged against.
const OFF_STREAMING: Press = Press {
    name: "off-streaming",
    own_voice_only: false,
    delivery: DeliveryMode::Streaming,
    timed: true,
};

const PRESSES: [Press; 4] = [
    OFF_STREAMING,
    Press {
        name: "on-streaming",
        own_voice_only: true,
        delivery: DeliveryMode::Streaming,
        timed: true,
    },
    Press {
        name: "on-complete",
        own_voice_only: true,
        delivery: DeliveryMode::Complete,
        timed: true,
    },
    Press {
        name: "on-streaming-untimed",
        own_voice_only: true,
        delivery: DeliveryMode::Streaming,
        timed: false,
    },
];

struct ReplayParakeet {
    model: Arc<Parakeet>,
    timed: bool,
}

impl Transcriber for ReplayParakeet {
    fn validate_language(
        &self,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<(), TranscriptionError> {
        self.model.validate_language(language, cancel)
    }

    fn transcribe_with(
        &self,
        audio: TranscriptionAudio<'_>,
        language: &Language,
        cancel: &CancelToken,
    ) -> Result<Hypothesis, TranscriptionError> {
        let hypothesis = self.model.transcribe_with(audio, language, cancel)?;
        Ok(if self.timed {
            hypothesis
        } else {
            Hypothesis::new(hypothesis.as_str())
        })
    }
}

fn required(variable: &str) -> PathBuf {
    aria_harness::inputs::path_input(variable)
        .expect("the path reads")
        .unwrap_or_else(|| panic!("set {variable}"))
}

/// The speaker models from the environment, enrolling the corpus's speakers.
fn speaker_config(corpus: &Path) -> SpeakerConfig {
    SpeakerConfig {
        model: required("ARIA_TEST_SPEAKER_MODEL"),
        segmentation_model: required("ARIA_TEST_SEGMENTATION_MODEL"),
        profiles_dir: corpus.join(SPEAKERS_DIR),
    }
}

/// The models dictation tracks voices with, loaded as the daemon loads them.
fn tracking_models(speaker: &SpeakerConfig) -> (Diarizer<WeSpeaker>, Pyannote) {
    let (diarizer, segmenter) = crate::speaker::tracking_models(speaker);
    (
        diarizer.expect("the speaker model loads"),
        segmenter.expect("the segmentation model loads"),
    )
}

#[test]
#[ignore = "replays nonspeech controls through real models in realtime"]
fn nonspeech_controls_type_nothing() {
    let corpus = required("ARIA_OWN_VOICE_CORPUS");
    let controls = required("ARIA_TEST_NOISE_CORPUS");
    let parakeet =
        Arc::new(Parakeet::load(required("ARIA_TEST_PARAKEET_MODEL"), Device::Cpu).unwrap());
    let speaker = speaker_config(&corpus.join(&owner_folders(&corpus)[0]));
    let mut files: Vec<_> = std::fs::read_dir(controls)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "wav"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "nonspeech controls are required");
    for (index, wav) in files.iter().enumerate() {
        for press in PRESSES {
            let typed = dictate(&speaker, wav, &parakeet, press);
            let words: usize = typed
                .deliveries
                .iter()
                .map(|(_, text)| text.split_whitespace().count())
                .sum();
            println!("control {index} {}: typed={words}", press.name);
            assert_eq!(words, 0, "nonspeech control {index}: {}", press.name);
        }
    }
}

/// One owner's scenario, ready to replay.
struct Case {
    owner: String,
    scenario: String,
    speaker: SpeakerConfig,
    owner_name: String,
    wav: PathBuf,
    truth: Truth,
    control: Option<(PathBuf, Truth)>,
}

/// Every selected scenario of every selected owner in `ARIA_OWN_VOICE_CORPUS`.
/// An overlap's clean owner control is replayed only as that overlap's
/// baseline, never as a scenario of its own.
fn cases() -> Vec<Case> {
    let corpus = required("ARIA_OWN_VOICE_CORPUS");
    let owners = selected(
        &owner_folders(&corpus),
        std::env::var("ARIA_OWN_VOICE_OWNERS").ok().as_deref(),
    );
    let mut cases = Vec::new();
    for owner in owners {
        let folder = corpus.join(&owner);
        let speaker = speaker_config(&folder);
        let owner_name = ProfileStore::open(&speaker.profiles_dir)
            .read_all()
            .expect("the owner's profiles read")
            .profiles
            .into_iter()
            .find(|profile| profile.role == Role::Owner)
            .expect("each owner's folder enrolls an owner")
            .name
            .as_str()
            .to_owned();
        let mut scenarios: Vec<(String, Truth)> = std::fs::read_dir(&folder)
            .expect("the owner's folder reads")
            .filter_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension()? == "wav").then_some(path)
            })
            .map(|wav| {
                let name = wav.file_stem().unwrap().to_str().unwrap().to_owned();
                (name, read_truth(&wav.with_extension("json")))
            })
            .filter(|(_, truth)| {
                truth
                    .overlap_probe
                    .as_ref()
                    .is_none_or(|probe| probe.artifact_kind != ArtifactKind::OwnerControl)
            })
            .collect();
        scenarios.sort_by(|a, b| a.0.cmp(&b.0));
        let names: Vec<String> = scenarios.iter().map(|(name, _)| name.clone()).collect();
        let wanted = selected(
            &names,
            std::env::var("ARIA_OWN_VOICE_SCENARIOS").ok().as_deref(),
        );
        for (scenario, truth) in scenarios {
            if !wanted.contains(&scenario) {
                continue;
            }
            let wav = folder.join(format!("{scenario}.wav"));
            let control = clean_owner_control(&folder, &wav, &truth);
            cases.push(Case {
                owner: owner.clone(),
                scenario,
                speaker: speaker.clone(),
                owner_name: owner_name.clone(),
                wav,
                truth,
                control,
            });
        }
    }
    cases
}

#[test]
#[ignore = "replays the own-voice corpus through the speaker tracker"]
fn the_tracker_keeps_the_owner_and_drops_other_voices() {
    let cases = cases();
    // Each run loads its own models, so as many run at once as there are cores.
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let mut results: Vec<(Value, tracker::Admitted)> = Vec::new();
    for batch in cases.chunks(jobs(cores)) {
        std::thread::scope(|scope| {
            let runs: Vec<_> = batch
                .iter()
                .map(|case| {
                    scope.spawn(|| tracker::evaluate(&case.speaker, &case.wav, &case.truth))
                })
                .collect();
            results.extend(
                runs.into_iter()
                    .map(|run| run.join().expect("the tracker run finishes")),
            );
        });
    }
    println!(
        "{:<6} {:<14} {:>14} {:>14}",
        "owner", "scenario", "owner kept", "other admitted"
    );
    let mut by_owner: BTreeMap<&str, tracker::Admitted> = BTreeMap::new();
    let mut rows = Vec::new();
    for (case, (tracking, admitted)) in cases.iter().zip(results) {
        println!(
            "{:<6} {:<14} {:>14} {:>14}",
            case.owner,
            case.scenario,
            admitted.owner_kept(),
            admitted.other_admitted(),
        );
        by_owner.entry(&case.owner).or_default().add(&admitted);
        rows.push(json!({
            "owner": case.owner,
            "scenario": case.scenario,
            "tracker": tracking,
        }));
    }
    let owners: Vec<tracker::Admitted> = by_owner.into_values().collect();
    let mut pooled = tracker::Admitted::default();
    for owner in &owners {
        pooled.add(owner);
    }
    let kept = owner_interval(&owners, |owner| (owner.owner_admitted_ms, owner.owner_ms));
    let admitted = owner_interval(&owners, |owner| {
        (owner.other_exclusive_admitted_ms, owner.other_exclusive_ms)
    });
    println!(
        "{:<21} {:>14} {:>14}\n{:<21} {:>14} {:>14}",
        "pooled",
        pooled.owner_kept(),
        pooled.other_admitted(),
        "95% over owners",
        interval_text(kept),
        interval_text(admitted),
    );
    if let Some(output) = std::env::var_os("ARIA_OWN_VOICE_TRACKING_RESULTS") {
        aria_harness::artifact::write_json(
            Path::new(&output),
            &json!({
                "schema": 1,
                "inputs": {
                    "corpus_sha256": aria_harness::artifact::fingerprint(&required("ARIA_OWN_VOICE_CORPUS")).unwrap(),
                    "speaker_model_sha256": aria_harness::artifact::fingerprint(&required("ARIA_TEST_SPEAKER_MODEL")).unwrap(),
                    "segmentation_model_sha256": aria_harness::artifact::fingerprint(&required("ARIA_TEST_SEGMENTATION_MODEL")).unwrap(),
                },
                "pooled": pooled,
                "owner_kept_ci95": kept,
                "other_admitted_ci95": admitted,
                "provenance": aria_harness::artifact::Provenance::capture().unwrap(),
                "rows": rows,
            }),
        )
        .unwrap();
    }
}

/// How many presses replay at once, each with its own transcription model.
/// The more share the machine, the longer the delays measure.
const DEFAULT_JOBS: usize = 4;

#[test]
#[ignore = "replays the own-voice corpus through real models in realtime"]
fn the_own_voice_corpus_passes_acceptance() {
    let parakeet_path = required("ARIA_TEST_PARAKEET_MODEL");
    let jobs = jobs(DEFAULT_JOBS);
    let press_names: Vec<String> = PRESSES.iter().map(|press| press.name.to_owned()).collect();
    let selected_presses = selected(
        &press_names,
        std::env::var("ARIA_OWN_VOICE_PRESSES").ok().as_deref(),
    );
    let presses: Vec<Press> = PRESSES
        .into_iter()
        .filter(|press| selected_presses.iter().any(|name| name == press.name))
        .collect();
    // `RUST_LOG=aria_kit::speaker=debug` shows each own-voice check.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();

    let cases = cases();
    let mut tasks = std::collections::VecDeque::new();
    for (index, case) in cases.iter().enumerate() {
        if case.control.is_some() {
            tasks.push_back((index, CLEAN_OWNER_PRESS));
        }
        tasks.extend(presses.iter().map(|press| (index, *press)));
    }
    let tasks = Mutex::new(tasks);
    let finished = Mutex::new(Vec::new());
    println!(
        "{:<6} {:<14} {:<26} {:>10} {:>11} {:>9} {:>9} {:>11}",
        "owner", "scenario", "press", "owner lost", "other typed", "p50 ms", "max ms", "shown ms"
    );
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                let parakeet =
                    Arc::new(Parakeet::load(&parakeet_path, Device::Cpu).expect("parakeet loads"));
                loop {
                    let Some((index, press)) = tasks.lock().unwrap().pop_front() else {
                        break;
                    };
                    let case = &cases[index];
                    let (wav, shown_truth, own_voice_only) = match &case.control {
                        Some((control, control_truth)) if press.name == CLEAN_OWNER_PRESS.name => {
                            (control, control_truth, false)
                        }
                        _ => (&case.wav, &case.truth, press.own_voice_only),
                    };
                    let typed = dictate(&case.speaker, wav, &parakeet, press);
                    let score = Score::new(&case.truth, &typed);
                    let shown = shown(shown_truth, &typed.voices, &case.owner_name, own_voice_only);
                    println!(
                        "{:<6} {:<14} {:<26} {:>4}/{:<5} {:>4}/{:<6} {:>9} {:>9} {:>5}/{:<5}",
                        case.owner,
                        case.scenario,
                        press.name,
                        score.owner_lost,
                        score.owner_words,
                        score.other_typed,
                        score.other_words,
                        score.delay_ms(0.5).map_or("-".into(), |ms| ms.to_string()),
                        score.delay_ms(1.0).map_or("-".into(), |ms| ms.to_string()),
                        shown.correct_ms,
                        shown.speech_ms,
                    );
                    finished.lock().unwrap().push((index, press, score, shown));
                }
            });
        }
    });
    let mut finished = finished.into_inner().unwrap();
    let order = |press: &Press| {
        std::iter::once(CLEAN_OWNER_PRESS)
            .chain(PRESSES)
            .position(|known| known.name == press.name)
    };
    finished.sort_by_key(|(index, press, _, _)| (*index, order(press)));

    let mut rows = Vec::new();
    let mut failures = Vec::new();
    let mut pooled: Vec<(Press, Pooled)> = presses
        .iter()
        .map(|press| (*press, Pooled::default()))
        .collect();
    let mut by_case: Vec<Vec<_>> = cases.iter().map(|_| Vec::new()).collect();
    for (index, press, score, shown) in finished {
        by_case[index].push((press, score, shown));
    }
    for (case, done) in cases.iter().zip(by_case) {
        let mut scores = Vec::new();
        for (press, score, shown) in done {
            rows.push(score.row(&case.owner, &case.scenario, press, &shown));
            if let Some((_, pool)) = pooled
                .iter_mut()
                .find(|(pooled, _)| pooled.name == press.name)
            {
                pool.add(&case.owner, &score);
            }
            scores.push((press, score));
        }
        failures.extend(
            acceptance(&case.truth, &scores)
                .into_iter()
                .map(|failure| format!("{}/{}: {failure}", case.owner, case.scenario)),
        );
    }
    println!(
        "pooled, with 95% over owners\n{:<26} {:>26} {:>26} {:>9} {:>9} {:>7}",
        "press", "owner lost", "other typed", "p50 ms", "p90 ms", "untyped"
    );
    for (press, pool) in &pooled {
        let (lost, typed) = pool.intervals();
        let total = pool.total();
        println!(
            "{:<26} {:>5}/{:<5} {:>14} {:>5}/{:<5} {:>14} {:>9} {:>9} {:>7}",
            press.name,
            total.owner_lost,
            total.owner,
            interval_text(lost),
            total.other_typed,
            total.other,
            interval_text(typed),
            quantile_ms(&pool.delays_ms, 0.5).map_or("-".into(), |ms| ms.to_string()),
            quantile_ms(&pool.delays_ms, 0.9).map_or("-".into(), |ms| ms.to_string()),
            pool.delays_ms
                .iter()
                .filter(|delay| delay.is_none())
                .count(),
        );
    }
    if let Some(output) = std::env::var_os("ARIA_OWN_VOICE_RESULTS") {
        let pooled: Vec<_> = pooled
            .iter()
            .map(|(press, pool)| pool.row(*press))
            .collect();
        let scenarios: Vec<String> = cases
            .iter()
            .map(|case| format!("{}/{}", case.owner, case.scenario))
            .collect();
        aria_harness::artifact::write_json(
            Path::new(&output),
            &json!({
                "schema": 7,
                "inputs": {
                    "corpus_sha256": aria_harness::artifact::fingerprint(&required("ARIA_OWN_VOICE_CORPUS")).unwrap(),
                    "speaker_model_sha256": aria_harness::artifact::fingerprint(&required("ARIA_TEST_SPEAKER_MODEL")).unwrap(),
                    "segmentation_model_sha256": aria_harness::artifact::fingerprint(&required("ARIA_TEST_SEGMENTATION_MODEL")).unwrap(),
                    "transcription_model_sha256": aria_harness::artifact::fingerprint(&parakeet_path).unwrap(),
                },
                "jobs": jobs,
                "policy": POLICY,
                "scenarios": scenarios,
                "presses": selected_presses,
                "pooled": pooled,
                "acceptance_failures": failures,
                "provenance": aria_harness::artifact::Provenance::capture().unwrap(),
                "rows": rows,
            }),
        )
        .unwrap();
    }
    assert!(
        failures.is_empty(),
        "voice acceptance failed:\n{}",
        failures.join("\n")
    );
}

/// How many replays run at once: `ARIA_OWN_VOICE_JOBS`, or `default`.
fn jobs(default: usize) -> usize {
    std::env::var("ARIA_OWN_VOICE_JOBS").map_or(default, |jobs| {
        jobs.parse()
            .ok()
            .filter(|jobs| *jobs > 0)
            .expect("ARIA_OWN_VOICE_JOBS is a positive count")
    })
}

/// Each owner's folder in the corpus, by name.
fn owner_folders(corpus: &Path) -> Vec<String> {
    let mut owners: Vec<String> = std::fs::read_dir(corpus)
        .expect("the corpus directory reads")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            path.is_dir()
                .then(|| path.file_name()?.to_str().map(str::to_owned))?
        })
        .collect();
    owners.sort();
    owners
}

/// Words under one press: the owner's spoken and lost, other voices' spoken
/// and typed.
#[derive(Default, Clone, Copy)]
struct Words {
    owner: usize,
    owner_lost: usize,
    other: usize,
    other_typed: usize,
}

impl Words {
    fn add(&mut self, more: &Self) {
        self.owner += more.owner;
        self.owner_lost += more.owner_lost;
        self.other += more.other;
        self.other_typed += more.other_typed;
    }
}

/// One press's scores across every owner and scenario.
#[derive(Default)]
struct Pooled {
    /// Each owner turn with words of its own, `None` when it never typed.
    delays_ms: Vec<Option<i64>>,
    by_owner: BTreeMap<String, Words>,
}

impl Pooled {
    fn add(&mut self, owner: &str, score: &Score) {
        self.delays_ms.extend(score.owned_delays_ms());
        self.by_owner
            .entry(owner.to_owned())
            .or_default()
            .add(&Words {
                owner: score.owner_words,
                owner_lost: score.owner_lost,
                other: score.other_words,
                other_typed: score.other_typed,
            });
    }

    fn total(&self) -> Words {
        let mut total = Words::default();
        for words in self.by_owner.values() {
            total.add(words);
        }
        total
    }

    /// The owner-lost and other-typed shares, at 95% over owners.
    fn intervals(&self) -> (Interval, Interval) {
        let owners: Vec<Words> = self.by_owner.values().copied().collect();
        let share = |part: usize, whole: usize| (part as f64, whole as f64);
        (
            owner_interval(&owners, |words| share(words.owner_lost, words.owner)),
            owner_interval(&owners, |words| share(words.other_typed, words.other)),
        )
    }

    fn row(&self, press: Press) -> serde_json::Value {
        let (lost, typed) = self.intervals();
        let total = self.total();
        json!({
            "owner_lost_ci95": lost,
            "other_typed_ci95": typed,
            "press": press.name,
            "owner_words": total.owner,
            "owner_lost": total.owner_lost,
            "other_words": total.other,
            "other_typed": total.other_typed,
            "owner_turns": self.delays_ms.len(),
            "owner_turns_untyped": self.delays_ms.iter().filter(|delay| delay.is_none()).count(),
            "delay_p50_ms": quantile_ms(&self.delays_ms, 0.5),
            "delay_p90_ms": quantile_ms(&self.delays_ms, 0.9),
        })
    }
}

/// A 95% interval, `None` below two owners.
type Interval = Option<(f64, f64)>;

/// Where a pooled share plausibly lies, at 95%, resampling owners: turns of
/// one owner share a voice, so they are not independent.
fn owner_interval<T: Copy>(owners: &[T], share: impl Fn(&T) -> (f64, f64)) -> Interval {
    aria_harness::stats::bootstrap(owners, |owners| {
        let (part, whole) = owners
            .iter()
            .map(&share)
            .fold((0.0, 0.0), |(part, whole), (more, of)| {
                (part + more, whole + of)
            });
        if whole == 0.0 { 0.0 } else { part / whole }
    })
}

fn interval_text(interval: Interval) -> String {
    interval.map_or("-".into(), |(low, high)| {
        format!("{:.1}–{:.1}%", low * 100.0, high * 100.0)
    })
}

/// A turn that never typed counts as the slowest.
fn quantile_ms(delays: &[Option<i64>], quantile: f64) -> Option<i64> {
    let mut delays: Vec<i64> = delays
        .iter()
        .map(|delay| delay.unwrap_or(i64::MAX))
        .collect();
    delays.sort_unstable();
    let index = ((delays.len() as f64 - 1.0) * quantile).round() as usize;
    delays
        .get(index)
        .copied()
        .filter(|delay| *delay != i64::MAX)
}

fn selected(available: &[String], requested: Option<&str>) -> Vec<String> {
    assert!(!available.is_empty(), "no evaluation cases available");
    let Some(requested) = requested else {
        return available.to_vec();
    };
    let names: Vec<_> = requested
        .split(',')
        .map(str::trim)
        .map(str::to_owned)
        .collect();
    for name in &names {
        assert!(
            available.contains(name),
            "unknown evaluation case: {name:?}"
        );
    }
    names
}

fn read_truth(path: &Path) -> Truth {
    let truth: Truth = serde_json::from_slice(&std::fs::read(path).expect("ground truth reads"))
        .expect("ground truth parses");
    for turn in &truth.turns {
        assert!(
            turn.start.is_finite()
                && turn.end.is_finite()
                && turn.start >= 0.0
                && turn.start < turn.end,
            "each truth turn starts at or after zero and ends after it starts"
        );
    }
    truth
}

fn confined_file(corpus: &Path, name: &str) -> PathBuf {
    let path = Path::new(name);
    assert!(
        matches!(path.components().next(), Some(Component::Normal(_)))
            && path.components().count() == 1,
        "control paths must be filenames within the corpus"
    );
    let root = corpus.canonicalize().expect("the corpus resolves");
    let resolved = root.join(path).canonicalize().expect("the control exists");
    assert!(
        resolved.starts_with(&root),
        "the control stays inside the corpus"
    );
    resolved
}

fn clean_owner_control(corpus: &Path, wav: &Path, truth: &Truth) -> Option<(PathBuf, Truth)> {
    let overlapping = truth.turns.iter().any(|owner| {
        owner.owner
            && truth
                .turns
                .iter()
                .any(|other| !other.owner && owner.start < other.end && other.start < owner.end)
    });
    let Some(probe) = &truth.overlap_probe else {
        assert!(
            !overlapping,
            "overlapping truth requires a clean owner control"
        );
        return None;
    };
    if probe.artifact_kind != ArtifactKind::Mix {
        assert!(
            !overlapping,
            "a control cannot contain overlapping owner and other truth"
        );
        return None;
    }
    assert!(
        overlapping,
        "an overlap mixture contains overlapping owner and other truth"
    );
    let name = Path::new(&probe.controls.owner_control);
    assert!(
        name.extension().is_some_and(|extension| extension == "wav"),
        "the owner control is a WAV"
    );
    let control = confined_file(corpus, &probe.controls.owner_control);
    assert!(
        control != wav.canonicalize().expect("the mixture resolves"),
        "the clean owner control differs from the mixture"
    );
    let control_json = name.with_extension("json");
    let control_truth = read_truth(&confined_file(corpus, control_json.to_str().unwrap()));
    assert!(
        control_truth.turns.iter().all(|turn| turn.owner)
            && truth
                .turns
                .iter()
                .filter(|turn| turn.owner)
                .eq(&control_truth.turns),
        "the control contains exactly the mixture's owner references and clock positions"
    );
    let mixed_audio = aria_kit::audio::read_wav(wav).expect("the mixture WAV reads");
    let control_audio = aria_kit::audio::read_wav(&control).expect("the control WAV reads");
    assert!(
        mixed_audio.is_canonical() && control_audio.is_canonical(),
        "evaluation audio is canonical"
    );
    assert_eq!(
        mixed_audio.samples().len(),
        control_audio.samples().len(),
        "the control preserves the full mixture clock"
    );
    assert!(
        control_truth
            .turns
            .iter()
            .all(|turn| turn.end <= control_audio.duration().as_secs_f64()),
        "control truth fits its audio"
    );
    Some((control, control_truth))
}

fn source_regions(truth: &Truth) -> Vec<(Range<f64>, bool)> {
    let mut boundaries: Vec<f64> = truth
        .turns
        .iter()
        .flat_map(|turn| [turn.start, turn.end])
        .collect();
    boundaries.sort_by(f64::total_cmp);
    boundaries.dedup();
    boundaries
        .windows(2)
        .filter_map(|pair| {
            let active: Vec<_> = truth
                .turns
                .iter()
                .filter(|turn| turn.start < pair[1] && turn.end > pair[0])
                .collect();
            (!active.is_empty()).then(|| (pair[0]..pair[1], active.iter().any(|turn| turn.owner)))
        })
        .collect()
}

fn acceptance(truth: &Truth, scores: &[(Press, Score)]) -> Vec<String> {
    let mut failures = Vec::new();
    let has_owner = truth.turns.iter().any(|turn| turn.owner);
    let has_other = truth.turns.iter().any(|turn| !turn.owner);
    if !scores.iter().any(|(press, _)| press.own_voice_only) {
        failures.push("coverage: no filtered press evaluated".into());
    }
    let mixed_baseline = scores
        .iter()
        .find(|(press, _)| press.name == OFF_STREAMING.name)
        .map(|(_, score)| score);
    let clean_baseline = scores
        .iter()
        .find(|(press, _)| press.name == CLEAN_OWNER_PRESS.name)
        .map(|(_, score)| score);
    let needs_clean = truth
        .overlap_probe
        .as_ref()
        .is_some_and(|probe| probe.artifact_kind == ArtifactKind::Mix);
    let baseline = if needs_clean {
        clean_baseline
    } else {
        mixed_baseline
    };
    if has_owner && needs_clean && mixed_baseline.is_none() {
        failures.push("recognition: missing off-streaming mixture diagnostics".into());
    }
    if has_owner {
        match baseline {
            None => failures.push(if needs_clean {
                "recognition: missing clean-owner off-streaming control".into()
            } else {
                "recognition: missing off-streaming control".into()
            }),
            Some(score) if score.owner_words == 0 => {
                failures.push("recognition: owner reference has no distinguishable words".into());
            }
            Some(score) => {
                for (index, (&words, &lost)) in score
                    .turn_reference_words
                    .iter()
                    .zip(&score.turn_owner_lost)
                    .enumerate()
                {
                    if words == 0 {
                        failures.push(format!(
                            "recognition: owner turn {index} has no distinguishable words"
                        ));
                    } else if (words - lost) as f64 / (words as f64)
                        < POLICY.minimum_control_owner_reference_recall
                    {
                        failures.push(format!("recognition: control lost {lost}/{words} reference words in owner turn {index}"));
                    }
                }
            }
        }
    }
    for (press, score) in scores.iter().filter(|(press, _)| press.own_voice_only) {
        if score.owner_repetitions > POLICY.maximum_owner_exclusive_repetitions {
            failures.push(format!(
                "{}: emitted {} excess owner-exclusive reference repetitions",
                press.name, score.owner_repetitions
            ));
        } else if POLICY.owner_reference_order_required && !score.owner_ordered {
            failures.push(format!(
                "{}: owner reference words were delivered out of order",
                press.name
            ));
        }
        if !has_owner && score.delivered_words > POLICY.maximum_other_only_output_words {
            failures.push(format!(
                "{}: other-only press delivered {} words",
                press.name, score.delivered_words
            ));
        } else if has_owner && has_other && score.other_words == 0 {
            failures.push(format!(
                "{}: other-voice rejection unscorable; no other-exclusive reference words",
                press.name
            ));
        } else if score.other_typed > POLICY.maximum_other_exclusive_reference_output {
            failures.push(format!(
                "{}: emitted {} other-exclusive reference words",
                press.name, score.other_typed
            ));
        }
        for (label, baseline) in [
            ("clean control", clean_baseline),
            ("mixed control", mixed_baseline),
        ] {
            let Some(baseline) = baseline else {
                continue;
            };
            let lost = baseline
                .owner_kept
                .iter()
                .map(|(word, count)| {
                    count.saturating_sub(score.owner_kept.get(word).copied().unwrap_or(0))
                })
                .sum::<usize>();
            if lost > POLICY.maximum_added_owner_reference_loss {
                failures.push(format!(
                    "{}: lost {lost} owner reference words present in control ({label})",
                    press.name
                ));
            }
        }
    }
    failures
}

fn dictate(speaker: &SpeakerConfig, wav: &Path, parakeet: &Arc<Parakeet>, press: Press) -> Typed {
    let spool = tempfile::tempdir().unwrap();
    let capture = WavReplayCapture::new(
        spool.path(),
        Duration::from_secs(120),
        wav,
        ReplayPace::Realtime,
    )
    .unwrap();
    let (diarizer, segmenter) = tracking_models(speaker);
    let typed = Arc::new(Mutex::new(Typed::default()));
    let started = Arc::new(OnceLock::new());
    let pipeline = DictationPipeline::new(
        TimedCapture {
            capture,
            started: Arc::clone(&started),
        },
        Transcription::stream(
            Parakeet::STREAM,
            ReplayParakeet {
                model: Arc::clone(parakeet),
                timed: press.timed,
            },
        ),
        Cursor::new(TimedDelivery {
            started: Arc::clone(&started),
            typed: Arc::clone(&typed),
        }),
    )
    .hearing_speech(Some(SileroVad::load().expect("silero loads")))
    .tracking_voices(Some(diarizer), Some(segmenter));

    let mut settings = settings(press.delivery, OutputMode::Insert);
    settings.own_voice_only = press.own_voice_only;
    let events = DictationEventEmitter::forwarded_by({
        let typed = Arc::clone(&typed);
        move |event| {
            if let DictationEvent::Stage(DictationStage::SpeakerChanged(voice)) = event {
                let mut typed = typed.lock().unwrap();
                let at = started.get().expect("capture started").elapsed();
                typed.voices.push((at, voice));
            }
        }
    });
    match pipeline.execute(
        settings,
        &ExecutionControl::for_test_lasting(
            StopMode::Manual,
            CancelToken::new(),
            Duration::from_secs(120),
        ),
        &events,
        None,
    ) {
        Ok(_) | Err(DictationError::NoSpeech) => {}
        Err(error) => panic!("{} {}: {error}", wav.display(), press.name),
    }
    drop(pipeline);
    drop(events);
    Arc::try_unwrap(typed)
        .ok()
        .expect("the pipeline is gone")
        .into_inner()
        .unwrap()
}

#[derive(serde::Serialize)]
struct WordEvidence {
    sha256: String,
    reference: Reference,
}

/// Who in the script says a typed word.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Reference {
    Owner,
    Other,
    Shared,
    Unreferenced,
}

struct Score {
    owner_words: usize,
    owner_lost: usize,
    owner_kept: HashMap<String, usize>,
    owner_repetitions: usize,
    owner_ordered: bool,
    delivered_words: usize,
    other_words: usize,
    other_typed: usize,
    other_repetitions: usize,
    shared_output_words: usize,
    unreferenced_output_words: usize,
    word_evidence: Vec<WordEvidence>,
    /// Per owner turn: from the end of its audio to when most of its own
    /// words were typed. `None` when they never were.
    turn_delays_ms: Vec<Option<i64>>,
    turn_reference_words: Vec<usize>,
    turn_owner_lost: Vec<usize>,
}

impl Score {
    fn row(&self, owner: &str, name: &str, press: Press, shown: &ShownScore) -> serde_json::Value {
        json!({
                "owner": owner,
                "scenario": name,
                "press": press.name,
                "delivered_words": self.delivered_words,
                "owner_words": self.owner_words,
                "owner_lost": self.owner_lost,
                "owner_excess_repetitions": self.owner_repetitions,
                "owner_reference_order_preserved": self.owner_ordered,
                "other_words": self.other_words,
                "other_typed": self.other_typed,
                "other_excess_repetitions": self.other_repetitions,
                "shared_output_words": self.shared_output_words,
                "unreferenced_output_words": self.unreferenced_output_words,
                "normalized_word_evidence": self.word_evidence,
                "turn_delays_ms": self.turn_delays_ms,
                "turn_reference_words": self.turn_reference_words,
                "turn_owner_lost": self.turn_owner_lost,
                "shown_right_ms": shown.correct_ms,
                "shown_judged_ms": shown.speech_ms,
                "shown_undecided_ms": shown.undecided_ms,
                "shown_kept_out_ms": shown.kept_out_ms,
        })
    }

    fn new(truth: &Truth, typed: &Typed) -> Self {
        let owner_said = words(truth.turns.iter().filter(|turn| turn.owner));
        let other_said = words(truth.turns.iter().filter(|turn| !turn.owner));
        let owner_only = only(&owner_said, &other_said);
        let other_only = only(&other_said, &owner_said);
        let delivered = typed
            .deliveries
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let all_typed = counts(&delivered);
        let word_evidence: Vec<_> = normalized_words(&delivered)
            .map(|word| WordEvidence {
                sha256: format!("{:x}", Sha256::digest(word.as_bytes())),
                reference: match (
                    owner_said.contains_key(&word),
                    other_said.contains_key(&word),
                ) {
                    (true, false) => Reference::Owner,
                    (false, true) => Reference::Other,
                    (true, true) => Reference::Shared,
                    (false, false) => Reference::Unreferenced,
                },
            })
            .collect();
        let owner_delivered: Vec<_> = normalized_words(&delivered)
            .filter(|word| owner_only.contains_key(word))
            .collect();
        let mut owner_turns: Vec<_> = truth.turns.iter().filter(|turn| turn.owner).collect();
        owner_turns.sort_by(|left, right| left.start.total_cmp(&right.start));
        let owner_reference: Vec<_> = owner_turns
            .iter()
            .flat_map(|turn| normalized_words(&turn.text))
            .filter(|word| owner_only.contains_key(word))
            .collect();
        let owner_words = owner_only.values().sum();
        let kept = overlap(&owner_only, &all_typed);
        let mut turn_reference_words = Vec::new();
        let mut turn_owner_lost = Vec::new();
        let turn_delays_ms = truth
            .turns
            .iter()
            .filter(|turn| turn.owner)
            .map(|turn| {
                let other_turns = words(
                    truth
                        .turns
                        .iter()
                        .filter(|other| !std::ptr::eq(*other, turn)),
                );
                let own = only(&counts(&turn.text), &other_turns);
                let reference_words = own.values().sum::<usize>();
                turn_reference_words.push(reference_words);
                turn_owner_lost.push(reference_words - overlap(&own, &all_typed));
                typed_at(&own, typed).map(|at| at.as_millis() as i64 - (turn.end * 1_000.0) as i64)
            })
            .collect();
        Self {
            owner_words,
            owner_lost: owner_words - kept,
            owner_repetitions: owner_only
                .iter()
                .map(|(word, count)| {
                    all_typed
                        .get(word)
                        .copied()
                        .unwrap_or(0)
                        .saturating_sub(*count)
                })
                .sum(),
            owner_ordered: is_subsequence(&owner_delivered, &owner_reference),
            owner_kept: owner_only
                .iter()
                .map(|(word, count)| {
                    (
                        word.clone(),
                        (*count).min(all_typed.get(word).copied().unwrap_or(0)),
                    )
                })
                .collect(),
            delivered_words: all_typed.values().sum(),
            other_words: other_only.values().sum(),
            other_typed: word_evidence
                .iter()
                .filter(|word| word.reference == Reference::Other)
                .count(),
            other_repetitions: other_only
                .iter()
                .map(|(word, expected)| {
                    all_typed
                        .get(word)
                        .copied()
                        .unwrap_or(0)
                        .saturating_sub(*expected)
                })
                .sum(),
            shared_output_words: word_evidence
                .iter()
                .filter(|word| word.reference == Reference::Shared)
                .count(),
            unreferenced_output_words: word_evidence
                .iter()
                .filter(|word| word.reference == Reference::Unreferenced)
                .count(),
            word_evidence,
            turn_delays_ms,
            turn_reference_words,
            turn_owner_lost,
        }
    }

    /// The delay at `quantile` across owner turns; a turn never typed counts
    /// as the longest.
    fn delay_ms(&self, quantile: f64) -> Option<i64> {
        quantile_ms(&self.owned_delays_ms(), quantile)
    }

    /// The delays of the owner turns that have words of their own.
    fn owned_delays_ms(&self) -> Vec<Option<i64>> {
        self.turn_delays_ms
            .iter()
            .zip(&self.turn_reference_words)
            .filter(|(_, words)| **words > 0)
            .map(|(delay, _)| *delay)
            .collect()
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ShownScore {
    correct_ms: u64,
    speech_ms: u64,
    undecided_ms: u64,
    kept_out_ms: u64,
}

fn shown(
    truth: &Truth,
    voices: &[(Duration, HeardSpeaker)],
    owner_name: &str,
    own_voice_only: bool,
) -> ShownScore {
    let mut score = ShownScore::default();
    for (span, owner) in source_regions(truth) {
        let end = Duration::from_secs_f64(span.end).as_millis() as u64;
        let mut from = Duration::from_secs_f64(span.start).as_millis() as u64;
        let mut shown = None;
        for (changed, voice) in voices {
            let to = (changed.as_millis() as u64).min(end);
            if to > from {
                score.add(to - from, shown, owner, owner_name, own_voice_only);
                from = to;
            }
            if from == end {
                break;
            }
            shown = Some(voice);
        }
        if from < end {
            score.add(end - from, shown, owner, owner_name, own_voice_only);
        }
    }
    score
}

impl ShownScore {
    fn add(
        &mut self,
        ms: u64,
        shown: Option<&HeardSpeaker>,
        owner: bool,
        owner_name: &str,
        own_voice_only: bool,
    ) {
        self.speech_ms += ms;
        let correct = match shown {
            Some(HeardSpeaker::Typed(name)) if owner => name
                .as_ref()
                .is_some_and(|name| name.as_str() == owner_name),
            Some(HeardSpeaker::Typed(None)) if !owner => !own_voice_only,
            Some(HeardSpeaker::KeptOut) => {
                self.kept_out_ms += ms;
                !owner && own_voice_only
            }
            Some(HeardSpeaker::Undecided) | None => {
                self.undecided_ms += ms;
                false
            }
            Some(HeardSpeaker::Typed(_)) => false,
        };
        if correct {
            self.correct_ms += ms;
        }
    }
}

/// When the typed text first held most of `own`.
fn typed_at(own: &HashMap<String, usize>, typed: &Typed) -> Option<Duration> {
    if own.is_empty() {
        return None;
    }
    let needed = (own.values().sum::<usize>() as f64 * TURN_TYPED_SHARE).ceil() as usize;
    let mut so_far = String::new();
    typed.deliveries.iter().find_map(|(at, text)| {
        so_far.push(' ');
        so_far.push_str(text);
        (overlap(own, &counts(&so_far)) >= needed).then_some(*at)
    })
}

fn words<'a>(turns: impl Iterator<Item = &'a Placed>) -> HashMap<String, usize> {
    let mut all = HashMap::new();
    for turn in turns {
        for (word, count) in counts(&turn.text) {
            *all.entry(word).or_default() += count;
        }
    }
    all
}

fn counts(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in normalized_words(text) {
        *counts.entry(word).or_default() += 1;
    }
    counts
}

fn normalized_words(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
}

fn is_subsequence(words: &[String], reference: &[String]) -> bool {
    let mut words = words.iter();
    let mut next = words.next();
    for word in reference {
        if next == Some(word) {
            next = words.next();
        }
    }
    next.is_none()
}

/// The words of `these` that `those` never says.
fn only(these: &HashMap<String, usize>, those: &HashMap<String, usize>) -> HashMap<String, usize> {
    these
        .iter()
        .filter(|(word, _)| !those.contains_key(*word))
        .map(|(word, count)| (word.clone(), *count))
        .collect()
}

fn overlap(expected: &HashMap<String, usize>, typed: &HashMap<String, usize>) -> usize {
    expected
        .iter()
        .map(|(word, count)| (*count).min(typed.get(word).copied().unwrap_or(0)))
        .sum()
}

fn truth(turns: serde_json::Value) -> Truth {
    serde_json::from_value(json!({ "turns": turns })).unwrap()
}

fn completed(text: &str) -> Typed {
    Typed {
        deliveries: vec![(Duration::ZERO, text.to_owned())],
        ..Typed::default()
    }
}

fn score(truth: &Truth, text: &str) -> Score {
    Score::new(truth, &completed(text))
}

#[test]
fn other_repetitions_are_not_capped_by_reference_count() {
    let truth = truth(json!([
        {"owner": true, "text": "alpha shared", "start": 0.0, "end": 1.0},
        {"owner": false, "text": "bravo shared", "start": 1.0, "end": 2.0},
    ]));
    let score = score(&truth, "ALPHA bravo bravo bravo shared mystery");
    assert_eq!(score.other_words, 1);
    assert_eq!(score.other_typed, 3);
    assert_eq!(score.other_repetitions, 2);
    assert_eq!(score.shared_output_words, 1);
    assert_eq!(score.unreferenced_output_words, 1);
    assert_eq!(score.delivered_words, 6);
    let row = score.row("owner", "synthetic", PRESSES[1], &ShownScore::default());
    let evidence = row["normalized_word_evidence"].as_array().unwrap();
    assert_eq!(evidence.len(), 6);
    assert_eq!(
        evidence[0]["sha256"],
        format!("{:x}", Sha256::digest(b"alpha"))
    );
    assert_eq!(evidence[1], evidence[2]);
    assert_eq!(evidence[2], evidence[3]);
    assert_eq!(evidence[1]["reference"], "other");
    assert!(!row.to_string().contains("bravo"));
}

#[test]
fn shown_scores_the_speaker_shown_during_each_voice() {
    let owner = crate::speaker::SpeakerName::new("owner").unwrap();
    let truth = truth(json!([
        {"owner": true, "text": "hello", "start": 1.0, "end": 2.0},
        {"owner": false, "text": "goodbye", "start": 3.0, "end": 4.0},
    ]));
    let voices = [
        (
            Duration::from_millis(1_250),
            HeardSpeaker::Typed(Some(owner)),
        ),
        (Duration::from_millis(3_250), HeardSpeaker::Undecided),
        (Duration::from_millis(3_500), HeardSpeaker::KeptOut),
    ];
    assert_eq!(
        shown(&truth, &voices, "owner", true),
        ShownScore {
            correct_ms: 1_250,
            speech_ms: 2_000,
            undecided_ms: 500,
            kept_out_ms: 500,
        }
    );
    assert_eq!(shown(&truth, &voices, "owner", false).correct_ms, 750);
}

#[test]
fn other_only_acceptance_rejects_nonreference_and_repeated_output() {
    let truth = truth(json!([
        {"owner": false, "text": "guest", "start": 0.0, "end": 1.0},
    ]));
    for text in ["hallucinated output", "guest guest"] {
        let failures = acceptance(&truth, &[(PRESSES[1], score(&truth, text))]);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("other-only press delivered 2 words"));
    }
    let silent = Score::new(&truth, &Typed::default());
    assert!(acceptance(&truth, &[(PRESSES[1], silent)]).is_empty());
}

#[test]
fn acceptance_separates_recognition_loss_from_added_delivery_loss() {
    let truth = truth(json!([
        {"owner": true, "text": "alpha bravo charlie delta echo", "start": 0.0, "end": 1.0},
    ]));
    let baseline = "alpha bravo charlie delta";
    assert!(
        acceptance(
            &truth,
            &[
                (PRESSES[0], score(&truth, baseline)),
                (PRESSES[1], score(&truth, baseline))
            ]
        )
        .is_empty()
    );
    let failures = acceptance(
        &truth,
        &[
            (PRESSES[0], score(&truth, baseline)),
            (PRESSES[1], score(&truth, "alpha bravo charlie echo")),
        ],
    );
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("lost 1 owner reference words present in control"));
    let failures = acceptance(
        &truth,
        &[
            (PRESSES[0], score(&truth, "alpha")),
            (PRESSES[1], score(&truth, "alpha")),
        ],
    );
    assert_eq!(failures.len(), 1);
    assert!(failures[0].starts_with("recognition:"));
    assert!(
        acceptance(&truth, &[(PRESSES[1], score(&truth, baseline))])[0]
            .contains("missing off-streaming")
    );
}

#[test]
fn repeated_or_shared_words_do_not_prove_a_turn_was_delivered() {
    let mut truth = truth(json!([
        {"owner": true, "text": "hello", "start": 0.0, "end": 1.0},
        {"owner": true, "text": "hello", "start": 2.0, "end": 3.0},
    ]));
    let typed = completed("hello");
    let score = Score::new(&truth, &typed);
    assert_eq!(score.owner_lost, 1);
    assert_eq!(score.turn_delays_ms, vec![None, None]);
    truth.turns[1].owner = false;
    let score = Score::new(&truth, &typed);
    assert!(
        acceptance(&truth, &[(PRESSES[0], score)])
            .iter()
            .any(|failure| failure.contains("no distinguishable words"))
    );
    assert_eq!(typed_at(&HashMap::new(), &typed), None);
}

#[test]
fn acceptance_cannot_hide_a_missing_short_turn_or_unscorable_other() {
    let mut truth = truth(json!([
        {"owner": true, "text": "alpha bravo charlie delta echo foxtrot golf hotel india juliet", "start": 0.0, "end": 5.0},
        {"owner": true, "text": "return", "start": 6.0, "end": 7.0},
    ]));
    let typed = truth.turns[0].text.clone();
    let both_presses = |truth: &Truth| {
        vec![
            (PRESSES[0], score(truth, &typed)),
            (PRESSES[1], score(truth, &typed)),
        ]
    };
    assert!(
        acceptance(&truth, &both_presses(&truth))
            .iter()
            .any(|failure| failure.contains("owner turn 1"))
    );
    truth.turns[1].owner = false;
    truth.turns[1].text = "alpha".into();
    let scores = both_presses(&truth);
    assert!(
        acceptance(&truth, &scores)
            .iter()
            .any(|failure| failure.contains("other-voice rejection unscorable"))
    );
    assert!(
        acceptance(&truth, &scores[..1])
            .iter()
            .any(|failure| failure.contains("no filtered press"))
    );
}

fn overlapping_truth() -> Truth {
    serde_json::from_value(json!({
        "turns": [
            {"owner": false, "text": "guest", "start": 0.0, "end": 3.0},
            {"owner": true, "text": "alpha bravo charlie delta echo", "start": 1.0, "end": 2.0},
        ],
        "overlap_probe": {
            "artifact_kind": "mix",
            "controls": {"owner_control": "owner.wav"},
        },
    }))
    .unwrap()
}

#[test]
fn overlap_acceptance_uses_clean_owner_word_identities() {
    let truth = overlapping_truth();
    let mixed = "alpha bravo charlie delta";
    let clean = "alpha bravo charlie echo";
    let failures = acceptance(
        &truth,
        &[
            (PRESSES[0], score(&truth, mixed)),
            (PRESSES[1], score(&truth, mixed)),
        ],
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.contains("missing clean-owner"))
    );
    let failures = acceptance(
        &truth,
        &[
            (PRESSES[0], score(&truth, mixed)),
            (CLEAN_OWNER_PRESS, score(&truth, clean)),
            (PRESSES[1], score(&truth, mixed)),
        ],
    );
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("lost 1 owner reference words present in control"));
    let failures = acceptance(
        &truth,
        &[
            (PRESSES[0], score(&truth, mixed)),
            (CLEAN_OWNER_PRESS, score(&truth, clean)),
            (PRESSES[1], score(&truth, clean)),
        ],
    );
    assert_eq!(failures.len(), 1);
    assert!(
        failures[0].contains("lost 1 owner reference words present in control (mixed control)")
    );
    assert!(
        acceptance(
            &truth,
            &[
                (PRESSES[0], score(&truth, mixed)),
                (CLEAN_OWNER_PRESS, score(&truth, clean)),
                (PRESSES[1], score(&truth, "alpha bravo charlie delta echo")),
            ]
        )
        .is_empty()
    );
}

#[test]
fn owner_delivery_rejects_reordering_and_duplicate_commits() {
    let truth = truth(json!([
        {"owner": true, "text": "alpha bravo charlie delta echo", "start": 0.0, "end": 1.0},
    ]));
    let streamed = |segments: &[&str]| {
        Score::new(
            &truth,
            &Typed {
                deliveries: segments
                    .iter()
                    .map(|text| (Duration::ZERO, (*text).to_owned()))
                    .collect(),
                ..Typed::default()
            },
        )
    };
    let reference = truth.turns[0].text.as_str();
    for (segments, failure) in [
        (vec!["echo delta charlie bravo alpha"], "out of order"),
        (
            vec!["alpha bravo", "charlie delta echo alpha"],
            "1 excess owner-exclusive",
        ),
        (
            vec!["alpha bravo charlie delta"],
            "lost 1 owner reference words",
        ),
    ] {
        let failures = acceptance(
            &truth,
            &[
                (PRESSES[0], score(&truth, reference)),
                (PRESSES[1], streamed(&segments)),
            ],
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains(failure));
    }
    assert!(
        acceptance(
            &truth,
            &[
                (
                    PRESSES[0],
                    score(&truth, "alpha alpha bravo charlie delta echo")
                ),
                (PRESSES[1], score(&truth, reference)),
            ]
        )
        .is_empty()
    );
}

#[test]
fn overlapping_speech_shows_the_owner_once() {
    let truth = overlapping_truth();
    assert_eq!(
        source_regions(&truth),
        vec![(0.0..1.0, false), (1.0..2.0, true), (2.0..3.0, false)]
    );
    let owner = crate::speaker::SpeakerName::new("owner").unwrap();
    let voices = [
        (Duration::ZERO, HeardSpeaker::KeptOut),
        (Duration::from_secs(1), HeardSpeaker::Typed(Some(owner))),
        (Duration::from_secs(2), HeardSpeaker::KeptOut),
    ];
    assert_eq!(
        shown(&truth, &voices, "owner", true),
        ShownScore {
            correct_ms: 3_000,
            speech_ms: 3_000,
            kept_out_ms: 2_000,
            undecided_ms: 0,
        }
    );
}
