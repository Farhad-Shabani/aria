//! How much speech the voice detector finds in a recording, per threshold.

use aria_kit::voice_activity::{SileroVad, VoiceActivityDetector};

const FRAME: usize = 512;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: vad_coverage <wav>")?;
    let mut reader = hound::WavReader::open(&path)?;
    let rate = f64::from(reader.spec().sample_rate);
    let samples: Vec<i16> = reader.samples::<i16>().collect::<Result<_, _>>()?;

    let mut vad = SileroVad::load()?;
    let mut scores = Vec::new();
    for frame in samples.chunks(FRAME) {
        let mut padded = [0_i16; FRAME];
        padded[..frame.len()].copy_from_slice(frame);
        scores.push(vad.probability(&padded)?);
    }

    if let Some(out) = std::env::args().nth(2) {
        let rows: String = scores.iter().map(|score| format!("{score}\n")).collect();
        std::fs::write(&out, rows)?;
        println!("wrote {} frame scores to {out}", scores.len());
    }

    let audio = samples.len() as f64 / rate;
    println!("{audio:.0}s of audio, {} frames", scores.len());
    for threshold in [0.9, 0.7, 0.5, 0.4, 0.3, 0.2, 0.1, 0.05] {
        let speaking = scores.iter().filter(|&&s| s >= threshold).count();
        let speech = (speaking * FRAME) as f64 / rate;
        println!(
            "  threshold {threshold:.2}  speech {speech:5.0}s  ({:.0}% of audio)",
            100.0 * speech / audio
        );
    }
    Ok(())
}
