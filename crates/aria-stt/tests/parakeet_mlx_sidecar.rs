//! Adversarial runs against the real interpreter and model.
//!
//! Gated: every test passes silently unless `ARIA_TEST_PYTHON` names an
//! interpreter with parakeet-mlx installed and `ARIA_TEST_PARAKEET_MLX_MODEL`
//! names a model repository. `HF_HOME` must then also be set, so a
//! misconfigured run fails loudly instead of downloading the model again.
//!
//! Every load runs from the engine's `src/parakeet_mlx/` directory, where the sidecar's
//! own filename shadows the parakeet_mlx package unless the daemon keeps the
//! working directory off the child's import path — each test doubly serves
//! as a regression test for that.

use std::path::PathBuf;

use aria_kit::audio::{RecordingReader, f32_from_pcm16, write_wav};
use aria_kit::backend::BackendError;
use aria_stt::{parakeet_mlx::ParakeetMlx, prelude::*};

/// One utterance long enough that a 16000-sample chunking leaves a 1-sample
/// tail — the shape that once drove MLX into a negative-size allocation.
const SUB_HOP_TAIL_SAMPLES: usize = 8 * 16000 + 1;

fn engine() -> Option<ParakeetMlx> {
    let Ok(python) = std::env::var("ARIA_TEST_PYTHON") else {
        return None;
    };
    let Ok(model) = std::env::var("ARIA_TEST_PARAKEET_MLX_MODEL") else {
        return None;
    };
    assert!(
        std::env::var_os("HF_HOME").is_some(),
        "set HF_HOME to the real model cache; without it the sandboxed home re-downloads the model"
    );
    std::env::set_current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src/parakeet_mlx"))
        .expect("the shadow directory should exist");
    Some(ParakeetMlx::load(python, model).expect("the sidecar should load"))
}

fn fixture_wav(name: &str, samples: impl Iterator<Item = i16>) -> PathBuf {
    let path = std::env::temp_dir().join(format!("parakeet-mlx-{}-{name}.wav", std::process::id()));
    let samples: Vec<f32> = samples.map(f32_from_pcm16).collect();
    write_wav(&path, &samples).expect("the fixture should write");
    path
}

/// Deterministic noise without a rand dependency.
fn noise(count: usize) -> impl Iterator<Item = i16> {
    let mut state: u32 = 0x2545_f491;
    std::iter::repeat_with(move || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 16) as i16 / 4
    })
    .take(count)
}

fn transcribe(
    transcription: &Transcription<ParakeetMlx, Whole>,
    path: &PathBuf,
) -> Result<Transcript, TranscriptionError> {
    let mut recording =
        RecordingReader::from_wav_file(path).map_err(TranscriptionError::Capture)?;
    let outcome = transcription.transcribe(&mut recording, &Language::Automatic);
    std::fs::remove_file(path).ok();
    outcome
}

/// Any transcript, or the pipeline's own no-speech verdict; what must not
/// happen is a crashed decode or a dead sidecar.
fn survives(name: &str, outcome: Result<Transcript, TranscriptionError>) {
    match outcome {
        Ok(_) | Err(TranscriptionError::NoSpeech) => {}
        Err(error) => panic!("{name} must decode, got {error:?}"),
    }
}

#[test]
fn the_sidecar_survives_an_adversarial_whole_decode_matrix() {
    let Some(engine) = engine() else { return };
    let second = 16000;
    let clips = [
        (
            "silence",
            fixture_wav("silence", std::iter::repeat_n(0, second)),
        ),
        ("noise", fixture_wav("noise", noise(second))),
        (
            "dc-offset",
            fixture_wav("dc-offset", std::iter::repeat_n(8000, second)),
        ),
        (
            "square",
            fixture_wav(
                "square",
                (0..second).map(|i| if i / 40 % 2 == 0 { i16::MAX } else { i16::MIN }),
            ),
        ),
        ("micro", fixture_wav("micro", std::iter::repeat_n(0, 800))),
    ];

    // One engine across the matrix: the sequence itself proves the sidecar
    // keeps serving after whatever each clip provoked.
    let transcription = Transcription::complete_wav(engine);
    for (name, clip) in clips {
        survives(name, transcribe(&transcription, &clip));
    }
}

#[test]
fn streaming_folds_a_sub_hop_tail_instead_of_crashing_the_encoder() {
    let Some(engine) = engine() else { return };
    let transcription = Transcription::complete_wav(engine.stream_chunks(16000));

    for (name, total) in [
        ("one-sample-tail", SUB_HOP_TAIL_SAMPLES),
        ("full-hop-tail", 8 * 16000 + 160),
    ] {
        let clip = fixture_wav(name, noise(total));
        survives(name, transcribe(&transcription, &clip));
    }
}

#[test]
fn a_chunk_below_one_mel_hop_fails_the_request_but_not_the_sidecar() {
    let Some(engine) = engine() else { return };
    let transcription = Transcription::complete_wav(engine.stream_chunks(1));

    // Twice: the second refusal proves the sidecar survived the first.
    for _ in 0..2 {
        let clip = fixture_wav("sub-hop-chunk", noise(16000));
        let error = transcribe(&transcription, &clip).expect_err("a sub-hop chunk must be refused");
        assert!(
            matches!(
                &error,
                TranscriptionError::Inference {
                    source: BackendError::Unusable(detail),
                    ..
                } if detail.contains("mel hop")
            ),
            "unexpected failure: {error:?}"
        );
    }
}
