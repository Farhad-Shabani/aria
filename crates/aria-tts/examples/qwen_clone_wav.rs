//! End-to-end voice cloning: synthesize new text with a voice enrolled from
//! a reference clip, through the Base model.

use aria_tts::qwen::SpeechCodecDecoder;
use aria_tts::qwen::generate::{CustomVoiceGenerator, GenerationParams};
use aria_tts::qwen::prompt::assemble_custom_voice;
use aria_tts::qwen::text::{ChatTokenizer, assistant_text};
use aria_tts::qwen::{CodePredictor, ModelConfig, Talker};
use candle_core::{DType, Device, Tensor};

type Error = Box<dyn std::error::Error + Send + Sync>;

fn main() -> Result<(), Error> {
    let dir = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .ok_or("usage: qwen_clone_wav <fixture-directory>")?;
    let model_dir = dir.join("qwen3-tts-0.6b-base");
    let model_config: ModelConfig =
        serde_json::from_str(&std::fs::read_to_string(model_dir.join("config.json"))?)?;
    let tokenizer_config: aria_tts::qwen::SpeechTokenizerConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("tokenizer-config.json"))?)?;

    println!("loading base model...");
    let weights =
        candle_core::safetensors::load(model_dir.join("model.safetensors"), &Device::Cpu)?;
    let vb = candle_nn::VarBuilder::from_tensors(weights, DType::F32, &Device::Cpu);
    let mut talker = Talker::load(vb.pp("talker"), &model_config.talker_config)?;
    let mut predictor = CodePredictor::load(
        vb.pp("talker.code_predictor"),
        &model_config.talker_config.code_predictor_config,
    )?;
    let codec = {
        let tw = candle_core::safetensors::load(
            dir.join("qwen3-tts-tokenizer.safetensors"),
            &Device::Cpu,
        )?;
        let tvb = candle_nn::VarBuilder::from_tensors(tw, DType::F32, &Device::Cpu).pp("decoder");
        SpeechCodecDecoder::load(tvb, &tokenizer_config.decoder_config)?
    };
    let chat_tokenizer = ChatTokenizer::load(dir.join("qwen-tok"))?;

    // Enrolled embedding (1024 f32 LE) extracted from the reference clip.
    let emb_bytes = std::fs::read(model_dir.join("voices/vivian.f32"))?;
    let emb_values: Vec<f32> = emb_bytes
        .as_chunks()
        .0
        .iter()
        .copied()
        .map(f32::from_le_bytes)
        .collect();
    let speaker_row = Tensor::from_vec(emb_values, (1, 1024), &Device::Cpu)?;

    let text = "Hello there. This is Aria speaking from pure Rust.";
    println!("generating with cloned voice: {text:?}");
    let prompt_ids = chat_tokenizer.encode(&assistant_text(text))?;
    let prompt =
        assemble_custom_voice(&talker, &model_config, &prompt_ids, Some(speaker_row), None)?;

    let mut generator = CustomVoiceGenerator::new(
        &mut talker,
        &mut predictor,
        &model_config,
        GenerationParams {
            max_frames: 256,
            ..Default::default()
        },
    );
    let frame_codes = generator.generate(prompt, &aria_kit::cancel::CancelToken::never())?;
    let frames = frame_codes.len();
    println!("generated {frames} frames");
    if frames == 0 {
        return Err("no frames generated".into());
    }
    let flat: Vec<u32> = frame_codes.into_iter().flatten().collect();
    let codes = Tensor::from_vec(flat, (frames, 16), &Device::Cpu)?
        .transpose(0, 1)?
        .contiguous()?;
    let wav = codec.decode_chunked(&codes)?;
    let samples = wav.flatten_all()?.to_vec1::<f32>()?;
    println!(
        "audio: {} samples ({} ms)",
        samples.len(),
        samples.len() * 1000 / 24000
    );

    // PCM16 mono wav
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + samples.len() * 2) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24000u32.to_le_bytes());
    out.extend_from_slice(&48000u32.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&((samples.len() * 2) as u32).to_le_bytes());
    for s in &samples {
        out.extend_from_slice(&((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes());
    }
    std::fs::write(dir.join("rust-clone.wav"), out)?;
    println!("wrote {}", dir.join("rust-clone.wav").display());
    Ok(())
}
