//! The `diarize` subcommand: speaker-label accuracy over synthesized
//! ground-truth conversations.

use anyhow::Context;
use aria_bench::diarize::{
    Clustering, Grouping, Labeled, Report, Request, Run, Timeline, Tuning, Windowing,
    label_as_attributed, label_as_shipped, label_by_segmentation, label_by_voice, label_recording,
};
use aria_speaker::wespeaker::Config as SpeakerConfig;
use aria_tts::kokoro::Kokoro;
use clap::Parser;

use crate::flag_path::expand;

/// A model's path under the directory `aria setup` downloads into.
macro_rules! model {
    ($path:literal) => {
        concat!("~/.aria/models/", $path)
    };
}

const EXAMPLES: &str = "\
Examples:
  aria-bench diarize
  aria-bench diarize --device gpu
  aria-bench diarize --audio meeting.wav --rttm hypothesis.rttm
";

#[derive(Parser)]
#[command(about = "Speaker-label accuracy on synthesized conversations", after_help = EXAMPLES)]
pub(crate) struct Cli {
    /// kokoro TTS ONNX model
    #[arg(long, default_value = model!("kokoro/kokoro.onnx"))]
    kokoro: String,
    /// kokoro voice styles file
    #[arg(long, default_value = model!("kokoro/voices.bin"))]
    kokoro_voices: String,
    /// Parakeet model directory
    #[arg(long, default_value = model!("parakeet/parakeet-tdt-0.6b-v3"))]
    parakeet: String,
    /// Run parakeet on the GPU instead of the CPU
    #[arg(long)]
    gpu: bool,
    /// wespeaker speaker-embedding ONNX model
    #[arg(long, default_value = model!("wespeaker/en/model.onnx"))]
    speaker_model: String,
    /// Label this recording instead of the synthesized scenarios
    #[arg(long, requires = "rttm")]
    audio: Option<String>,
    /// Where to write the labels for `--audio`, as RTTM
    #[arg(long, requires = "audio")]
    rttm: Option<String>,
    /// Take the speech boundaries from the word spans, not the voice detector
    #[arg(long, conflicts_with = "by_voice")]
    word_timeline: bool,
    /// Label from the voice alone, with no transcriber in the path
    #[arg(long, requires = "audio")]
    by_voice: bool,
    /// Label through the pyannote segmentation net, which can report overlap
    #[arg(long, requires = "audio", conflicts_with_all = ["by_voice", "word_timeline"])]
    segmented: bool,
    /// pyannote segmentation ONNX model
    #[arg(long, default_value = model!("pyannote/segmentation-3.0.onnx"))]
    segmentation_model: String,
    /// Milliseconds each segmentation chunk advances
    #[arg(long, requires = "segmented", default_value_t = 1000)]
    shift_ms: usize,
    /// Cosine below which two masked voiceprints are different people
    #[arg(long, requires = "segmented", default_value_t = 0.25)]
    seg_threshold: f32,
    /// Place each voiceprint as it arrives, the way a live session must
    #[arg(long, requires = "segmented", conflicts_with = "speakers")]
    online: bool,
    /// Run the voice track listen ships, rather than this crate's copy of it
    #[arg(long, requires = "audio", conflicts_with_all = ["by_voice", "word_timeline", "segmented"])]
    as_shipped: bool,
    /// Ask the track who owned each speech run, the way the transcript is
    /// written, rather than reading its own timeline
    #[arg(long, requires = "as_shipped")]
    attributed: bool,
    /// Keep what the nets heard here, so a sweep pays for them once
    #[arg(long, requires = "as_shipped")]
    chunk_cache: Option<String>,
    /// Write what the track believed after every chunk here, as JSON lines
    #[arg(long, requires = "as_shipped")]
    chunk_log: Option<String>,
    /// Cosine below which the shipped track calls two voiceprints different
    /// people
    #[arg(long, requires = "as_shipped", default_value_t = Grouping::default().threshold)]
    cluster_threshold: f32,
    /// Voiceprints the shipped track needs before a voice may claim audio
    #[arg(long, requires = "as_shipped", default_value_t = Grouping::default().least_voiceprints)]
    min_voiceprints: usize,
    /// Group into exactly this many voices instead of using the threshold
    #[arg(long)]
    speakers: Option<usize>,
    /// Milliseconds of audio each voiceprint window carries
    #[arg(long, requires = "by_voice", default_value_t = 1500)]
    window_ms: usize,
    /// Milliseconds each voiceprint window advances
    #[arg(long, requires = "by_voice", default_value_t = 750)]
    hop_ms: usize,
    /// Also dump each segment's voiceprint here, to re-cluster offline
    #[arg(long, requires = "audio")]
    embeddings: Option<String>,
}

pub(crate) fn run(cli: &Cli) -> anyhow::Result<()> {
    let device = match cli.gpu {
        true => aria_stt::parakeet::Device::Gpu,
        false => aria_stt::parakeet::Device::Cpu,
    };
    let stt = || {
        aria_stt::parakeet::Parakeet::load(expand(&cli.parakeet)?, device)
            .context("loading parakeet")
    };
    let speaker = SpeakerConfig {
        model: expand(&cli.speaker_model)?,
    };
    if let (Some(audio), Some(rttm)) = (&cli.audio, &cli.rttm) {
        let audio = expand(audio)?;
        let name = audio
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("recording")
            .to_owned();
        let timeline = match cli.word_timeline {
            true => Timeline::Transcriber,
            false => Timeline::VoiceActivity,
        };
        if cli.as_shipped {
            let model = expand(&cli.segmentation_model)?;
            let cache = cli.chunk_cache.as_deref().map(expand).transpose()?;
            let journal = cli.chunk_log.as_deref().map(expand).transpose()?;
            let run = Run {
                segmentation: &model,
                speaker,
                wav: &audio,
                grouping: Grouping {
                    threshold: cli.cluster_threshold,
                    least_voiceprints: cli.min_voiceprints,
                },
                cache: cache.as_deref(),
                journal: journal.as_deref(),
            };
            let heard = match cli.attributed {
                true => label_as_attributed(run)?,
                false => label_as_shipped(run)?,
            };
            return write(cli, &name, rttm, &heard);
        }
        if cli.segmented {
            let heard = label_by_segmentation(
                &expand(&cli.segmentation_model)?,
                speaker,
                &audio,
                Tuning {
                    clustering: match cli.online {
                        true => Clustering::Online,
                        false => Clustering::Batch,
                    },
                    shift_samples: cli.shift_ms * 16,
                    speakers: cli.speakers,
                    threshold: cli.seg_threshold,
                },
            )?;
            return write(cli, &name, rttm, &heard);
        }
        let heard = match cli.by_voice {
            true => label_by_voice(
                speaker,
                &audio,
                cli.speakers,
                Windowing {
                    window_samples: cli.window_ms * 16,
                    hop_samples: cli.hop_ms * 16,
                },
            )?,
            false => label_recording(stt()?, speaker, &audio, timeline)?,
        };
        return write(cli, &name, rttm, &heard);
    }
    let report = Request {
        tts: Kokoro::load(expand(&cli.kokoro)?, expand(&cli.kokoro_voices)?)
            .context("loading kokoro")?,
        stt: stt()?,
        speaker,
    }
    .run()?;
    print(&report);
    Ok(())
}

/// Writes one recording's labels where the caller asked for them.
fn write(cli: &Cli, name: &str, rttm: &str, heard: &Labeled) -> anyhow::Result<()> {
    let rows: String = heard
        .turns
        .iter()
        .map(|turn| {
            format!(
                "SPEAKER {name} 1 {:.3} {:.3} <NA> <NA> {} <NA> <NA>\n",
                turn.start,
                turn.duration,
                // RTTM splits on whitespace, so the name must hold none.
                turn.speaker.replace(' ', "_")
            )
        })
        .collect();
    std::fs::write(expand(rttm)?, rows).context("writing the hypothesis rttm")?;
    if let Some(path) = &cli.embeddings {
        let rows: String = heard
            .embeddings
            .iter()
            .map(|segment| {
                let voice = match &segment.embedding {
                    Some(values) => values
                        .iter()
                        .map(|value| value.to_string())
                        .collect::<Vec<_>>()
                        .join(" "),
                    None => String::new(),
                };
                format!("{:.3}\t{:.3}\t{voice}\n", segment.start, segment.end)
            })
            .collect();
        std::fs::write(expand(path)?, rows).context("writing the embeddings")?;
    }
    println!(
        "{name}: {} turns labeled, {} unnamed ({:.0}s)",
        heard.turns.len(),
        heard.dropped_segments,
        heard.dropped_seconds,
    );
    Ok(())
}

fn print(report: &Report) {
    println!(
        "{:<10} {:>9} {:>8} {:>9} {:>7} {:>7} {:>8}",
        "scenario", "segments", "dropped", "drop secs", "live", "final", "recall"
    );
    for scenario in &report.scenarios {
        println!(
            "{:<10} {:>9} {:>8} {:>9.1} {:>6.1}% {:>6.1}% {:>7.1}%",
            scenario.name,
            scenario.segments,
            scenario.dropped,
            scenario.dropped_seconds,
            scenario.live_accuracy * 100.0,
            scenario.final_accuracy * 100.0,
            scenario.word_recall * 100.0,
        );
    }
}
