//! Weight-backed parity gates against the reference Python implementation.
//!
//! These need real model assets and are skipped by default. Point
//! `QWEN_TTS_TEST_ASSETS` at a directory laid out as:
//!
//! ```text
//! <assets>/
//!   tokenizer-config.json          # Qwen/Qwen3-TTS-Tokenizer-12Hz config.json
//!   qwen3-tts-tokenizer.safetensors
//!   ref_codes.npy                  # [16, T] int64 codes
//!   ref_wav.npy                    # python-decoded f32 samples
//! ```
//!
//! The fixtures were produced by `qwen-tts` decoding fixed random codes on CPU.

use std::path::PathBuf;

use aria_kit::cancel::CancelToken;
use aria_kit::synthesis::{SpeakingRate, SpeechText, Synthesis};
use aria_tts::qwen::Qwen;

fn assets() -> Option<PathBuf> {
    let dir = std::env::var_os("QWEN_TTS_TEST_ASSETS")?;
    Some(PathBuf::from(dir))
}

fn engine_assets() -> Option<(PathBuf, PathBuf)> {
    let model = std::env::var_os("QWEN_TTS_MODEL_DIR")?;
    let tokenizer = std::env::var_os("QWEN_TTS_TOKENIZER_DIR")?;
    Some((model.into(), tokenizer.into()))
}

/// Minimal .npy reader for little-endian arrays; enough for f32/i64 fixtures.
fn read_npy(path: &std::path::Path) -> Result<(Vec<u8>, Vec<usize>, String), String> {
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    if data.len() < 10 || &data[..6] != b"\x93NUMPY" {
        return Err("not an npy file".to_owned());
    }
    let header_len = data[8] as usize;
    let header = String::from_utf8_lossy(&data[10..10 + header_len]).to_string();
    let descr = header
        .split('\'')
        .nth(3)
        .ok_or("npy header missing descr")?
        .to_string();
    let shape: Vec<usize> = header
        .split('(')
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .map(|dims| {
            dims.split(',')
                .filter_map(|d| d.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    Ok((data[10 + header_len..].to_vec(), shape, descr))
}

#[test]
#[ignore = "requires QWEN_TTS_TEST_ASSETS with tokenizer weights and reference decode"]
fn codec_decode_matches_python_reference() {
    let dir = assets().expect("QWEN_TTS_TEST_ASSETS not set");
    let config: aria_tts::qwen::SpeechTokenizerConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("tokenizer-config.json")).unwrap())
            .unwrap();
    let weights = candle_core::safetensors::load(
        dir.join("qwen3-tts-tokenizer.safetensors"),
        &candle_core::Device::Cpu,
    )
    .unwrap();
    let vb = candle_nn::VarBuilder::from_tensors(
        weights,
        candle_core::DType::F32,
        &candle_core::Device::Cpu,
    )
    .pp("decoder");
    let decoder = aria_tts::qwen::SpeechCodecDecoder::load(vb, &config.decoder_config).unwrap();

    let (codes_bytes, shape, descr) = read_npy(&dir.join("ref_codes.npy")).unwrap();
    assert_eq!(descr, "<i8", "fixture dtype");
    let codes: Vec<u32> = codes_bytes
        .chunks(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()) as u32)
        .collect();
    let codes =
        candle_core::Tensor::from_vec(codes, (shape[0], shape[1]), &candle_core::Device::Cpu)
            .unwrap();

    let wav = decoder.decode_chunked(&codes).unwrap();
    let got = wav.flatten_all().unwrap().to_vec1::<f32>().unwrap();

    let (wav_bytes, _, _) = read_npy(&dir.join("ref_wav.npy")).unwrap();
    let expected: Vec<f32> = wav_bytes
        .chunks(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(got.len(), expected.len(), "sample count");

    let max_diff = got
        .iter()
        .zip(expected.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    // f32 accumulation noise across the conv stack; observed ~3.5e-5.
    assert!(max_diff < 5e-4, "codec parity broken: {max_diff}");
}

#[test]
#[ignore = "requires QWEN_TTS_MODEL_DIR and QWEN_TTS_TOKENIZER_DIR"]
fn custom_voice_engine_synthesizes_pcm() {
    let (model, tokenizer) = engine_assets().expect("Qwen model paths are not set");
    let engine = Qwen::load(model, tokenizer).expect("the model should load");
    let synthesis = Synthesis::new(engine);
    let text = SpeechText::new("Hello.").unwrap();
    let mut session = synthesis.start(&text).expect("the session should start");

    let mut sample_count = 0usize;
    let mut buffer = vec![0.0; 24_000];
    loop {
        let copied = session.read(&mut buffer).expect("synthesis should succeed");
        if copied == 0 {
            break;
        }
        sample_count += copied;
    }

    assert!(sample_count > 2_400, "speech should contain audible PCM");
}

#[test]
#[ignore = "requires QWEN_TTS_MODEL_DIR and QWEN_TTS_TOKENIZER_DIR"]
fn custom_voice_engine_cancels_during_generation() {
    let (model, tokenizer) = engine_assets().expect("Qwen model paths are not set");
    let engine = Qwen::load(model, tokenizer).expect("the model should load");
    let synthesis = Synthesis::new(engine);
    let text = SpeechText::new("This sentence should not finish.").unwrap();
    let cancel = CancelToken::new();
    let mut session = synthesis
        .start_with(&text, None, SpeakingRate::standard(), &cancel)
        .expect("the session should start");

    std::thread::sleep(std::time::Duration::from_millis(100));
    cancel.cancel();

    assert!(matches!(
        session.read(&mut [0.0; 1]),
        Err(aria_kit::synthesis::SynthesisError::Cancelled)
    ));
}
