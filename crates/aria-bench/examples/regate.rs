//! Re-embeds a recording's segments at a chosen speech-gate threshold.
//!
//! Reuses saved segment spans to compare speech-gate thresholds without rerunning transcription.
//!
//! usage: regate <wav> <spans.tsv> <threshold> [out.tsv]

use aria_kit::audio::f32_from_pcm16;
use aria_kit::speaker::Embedder;
use aria_kit::voice_activity::{SileroVad, VoiceActivityDetector};
use aria_speaker::wespeaker::WeSpeaker;

const FRAME: usize = 512;

/// The segment's samples that a gate at `threshold` calls speech.
fn gated(gate: &mut SileroVad, segment: &[i16], threshold: f32) -> Vec<f32> {
    let mut kept = Vec::new();
    for frame in segment.chunks(FRAME) {
        let mut padded = [0_i16; FRAME];
        padded[..frame.len()].copy_from_slice(frame);
        match gate.probability(&padded) {
            Ok(probability) if probability >= threshold => {
                kept.extend(frame.iter().copied().map(f32_from_pcm16));
            }
            _ => {}
        }
    }
    kept
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let wav = args
        .next()
        .ok_or("usage: regate <wav> <spans.tsv> <threshold> [out]")?;
    let spans_path = args.next().ok_or("missing spans.tsv")?;
    let threshold: f32 = args.next().ok_or("missing threshold")?.parse()?;
    let out = args.next();

    let mut reader = hound::WavReader::open(&wav)?;
    let rate = f64::from(reader.spec().sample_rate);
    let samples: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;

    let mut gate = SileroVad::load()?;
    let model = std::env::var("HOME")? + "/.aria/models/wespeaker/en/model.onnx";
    let mut speaker = WeSpeaker::load(std::path::Path::new(&model))?;

    let mut rows = String::new();
    let (mut embedded, mut total) = (0_usize, 0_usize);
    for line in std::fs::read_to_string(&spans_path)?.lines() {
        let mut fields = line.split('\t');
        let start: f64 = fields.next().ok_or("no start")?.parse()?;
        let end: f64 = fields.next().ok_or("no end")?.parse()?;
        let from = ((start * rate) as usize).min(samples.len());
        let to = ((end * rate) as usize).min(samples.len());
        total += 1;

        let voice = gated(&mut gate, &samples[from..to], threshold);
        let printed = match speaker.embed(&voice)? {
            Some(values) => {
                embedded += 1;
                values
                    .iter()
                    .map(|value| value.to_string())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
            None => String::new(),
        };
        rows.push_str(&format!("{start:.3}\t{end:.3}\t{printed}\n"));
    }

    println!("threshold {threshold}: {embedded} of {total} segments embedded");
    if let Some(path) = out {
        std::fs::write(path, rows)?;
    }
    Ok(())
}
