//! Compares the Base-prompt prefill against the captured Python reference.

use std::path::Path;

use aria_tts::qwen::prompt::assemble_custom_voice;
use aria_tts::qwen::text::{ChatTokenizer, assistant_text};
use aria_tts::qwen::{ModelConfig, Talker};
use candle_core::{DType, Device, Tensor};

type Error = Box<dyn std::error::Error + Send + Sync>;

fn read_npy_f32(path: &Path) -> Result<Vec<f32>, Error> {
    let data = std::fs::read(path)?;
    let header_len = data[8] as usize;
    Ok(data[10 + header_len..]
        .chunks(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect())
}

fn main() -> Result<(), Error> {
    let dir = std::env::args_os()
        .nth(1)
        .map(std::path::PathBuf::from)
        .ok_or("usage: qwen_clone_prefill <fixture-directory>")?;
    let model_dir = dir.join("qwen3-tts-0.6b-base");
    let model_config: ModelConfig =
        serde_json::from_str(&std::fs::read_to_string(model_dir.join("config.json"))?)?;
    let weights =
        candle_core::safetensors::load(model_dir.join("model.safetensors"), &Device::Cpu)?;
    let vb = candle_nn::VarBuilder::from_tensors(weights, DType::F32, &Device::Cpu);
    let talker = Talker::load(vb.pp("talker"), &model_config.talker_config)?;

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
    let tok = ChatTokenizer::load(dir.join("qwen-tok"))?;
    let ids = tok.encode(&assistant_text(text))?;
    let prompt = assemble_custom_voice(&talker, &model_config, &ids, Some(speaker_row), None)?;

    let got = prompt.prefill.squeeze(0)?.to_vec2::<f32>()?;
    let reference = read_npy_f32(&dir.join("ref_clone_prefill.npy"))?;
    let rows = reference.len() / 1024;
    println!("rows: rust {} vs python {}", got.len(), rows);
    let mut worst = 0f32;
    for (r, row) in got.iter().enumerate().take(rows) {
        let d = row
            .iter()
            .zip(reference[r * 1024..(r + 1) * 1024].iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        println!("row {r}: max diff {d:.4e}");
        worst = worst.max(d);
    }
    println!("worst: {worst:.4e}");
    Ok(())
}
