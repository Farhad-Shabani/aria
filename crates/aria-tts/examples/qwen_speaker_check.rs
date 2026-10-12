use std::path::Path;

use aria_tts::qwen::SpeakerEncoder;
use aria_tts::qwen::mel::log_mel;
use candle_core::{DType, Device};

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
        .ok_or("usage: qwen_speaker_check <fixture-directory>")?;

    // The reference clip, as raw f32 samples exported by python.
    let samples = read_npy_f32(&dir.join("ref_clip.npy"));
    let samples = match samples {
        Ok(v) => v,
        Err(_) => {
            // decode rust-out.wav PCM16 instead
            import_wave(&dir.join("rust-out.wav"))?
        }
    };
    println!("clip samples: {}", samples.len());

    let device = Device::Cpu;
    let mel = log_mel(&samples, &device)?;
    println!("mel dims: {:?}", mel.dims());
    let ref_mel = read_npy_f32(&dir.join("ref_mel.npy"))?;
    let got_mel = mel.flatten_all()?.to_vec1::<f32>()?;
    let n = got_mel.len().min(ref_mel.len());
    let diff = got_mel[..n]
        .iter()
        .zip(ref_mel[..n].iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    println!(
        "mel maxdiff vs python: {diff:.4e} ({n} of {} vs {})",
        got_mel.len(),
        ref_mel.len()
    );

    let weights =
        candle_core::safetensors::load(dir.join("qwen3-tts-0.6b-base.safetensors"), &device)?;
    let vb = candle_nn::VarBuilder::from_tensors(weights, DType::F32, &device);
    let mut encoder = SpeakerEncoder::load(vb.pp("speaker_encoder"), &device)?;
    let embedding = encoder.embed(&mel)?;
    let got_emb = embedding.flatten_all()?.to_vec1::<f32>()?;
    let ref_emb = read_npy_f32(&dir.join("ref_speaker_embedding.npy"))?;
    let diff_emb = got_emb
        .iter()
        .zip(ref_emb.iter())
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    println!(
        "embedding dims {:?} maxdiff vs python: {diff_emb:.4e}",
        embedding.dims()
    );
    Ok(())
}

fn import_wave(path: &Path) -> Result<Vec<f32>, Error> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    // assume 44-byte canonical header
    Ok(data[44..]
        .chunks(2)
        .map(|c| i16::from_le_bytes(c.try_into().unwrap()) as f32 / 32768.0)
        .collect())
}
