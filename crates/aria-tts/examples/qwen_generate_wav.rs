use std::io::Write as _;
use std::path::Path;

use aria_tts::qwen::generate::{CustomVoiceGenerator, GenerationParams};
use aria_tts::qwen::text::{ChatTokenizer, assistant_text};
use aria_tts::qwen::{CodePredictor, ModelConfig, SpeechCodecDecoder, Talker};
use candle_core::{DType, Device, Tensor};

type Error = Box<dyn std::error::Error + Send + Sync>;

fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), Error> {
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + samples.len() * 2) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&((samples.len() * 2) as u32).to_le_bytes());
    for s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        out.extend_from_slice(&((clamped * i16::MAX as f32) as i16).to_le_bytes());
    }
    std::fs::File::create(path)?.write_all(&out)?;
    Ok(())
}

fn main() -> Result<(), Error> {
    let dir = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .ok_or("usage: qwen_generate_wav <fixture-directory>")?;
    let model_config: ModelConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("model-config.json"))?)?;
    let tokenizer_config: aria_tts::qwen::SpeechTokenizerConfig =
        serde_json::from_str(&std::fs::read_to_string(dir.join("tokenizer-config.json"))?)?;

    println!("loading weights...");
    let weights = candle_core::safetensors::load(
        dir.join("qwen3-tts-0.6b-customvoice.safetensors"),
        &Device::Cpu,
    )?;
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

    let text = "Hello there.";
    let speaker = "vivian";
    println!("generating: {text:?} as {speaker}");

    let prompt_ids = chat_tokenizer.encode(&assistant_text(text))?;
    // CustomVoice: look up the preset speaker and embed its id.
    let spk_token = model_config.talker_config.spk_id[speaker];
    let speaker_row = talker.embed_codec(spk_token)?;
    let prompt = aria_tts::qwen::prompt::assemble_custom_voice(
        &talker,
        &model_config,
        &prompt_ids,
        Some(speaker_row),
        None, // auto language
    )?;

    let start = std::time::Instant::now();
    let mut generator = CustomVoiceGenerator::new(
        &mut talker,
        &mut predictor,
        &model_config,
        GenerationParams {
            temperature: 0.1,
            ..Default::default()
        },
    );
    let codes_flat: Vec<u32> = generator
        .generate(prompt, &aria_kit::cancel::CancelToken::never())?
        .into_iter()
        .flatten()
        .collect();
    let frames = codes_flat.len() / 16;
    println!("generated {frames} frames in {:.1?}", start.elapsed());
    if frames == 0 {
        return Err("no frames generated".into());
    }

    // frame_codes are collected per frame; the decoder wants [books, frames].
    let codes = Tensor::from_vec(codes_flat.clone(), (frames, 16), &Device::Cpu)?
        .transpose(0, 1)?
        .contiguous()?;
    let bytes: Vec<u8> = codes_flat.iter().flat_map(|c| c.to_le_bytes()).collect();
    std::fs::write(dir.join("rust_codes.bin"), bytes).unwrap();
    let wav = codec.decode_chunked(&codes)?;
    let samples = wav.flatten_all()?.to_vec1::<f32>()?;
    println!(
        "audio: {} samples ({} ms)",
        samples.len(),
        samples.len() * 1000 / 24000
    );

    write_wav(&dir.join("rust-out.wav"), &samples, 24_000)?;
    println!("wrote {}", dir.join("rust-out.wav").display());
    Ok(())
}
