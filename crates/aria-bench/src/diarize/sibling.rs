//! Labels speakers independently of transcription, using fixed embedding windows
//! and voice-activity detection to identify speech regions.

use std::ops::Range;
use std::path::Path;

use aria_kit::audio::read_wav;
use aria_kit::voice_activity::SileroVad;
use aria_speaker::wespeaker::Config as SpeakerConfig;

use crate::diarize::cluster::{Stop, group};
use crate::diarize::{BenchError, Embedded, Labeled, SAMPLE_RATE, Turn, sequence};

/// How a recording is cut up for the embedder: how much audio each window
/// carries, and how far it advances. The window has to hold enough voice for
/// a trustworthy voiceprint; the hop is the resolution of every boundary
/// this path reports.
#[derive(Clone, Copy)]
pub struct Windowing {
    pub window_samples: usize,
    pub hop_samples: usize,
}

impl Default for Windowing {
    fn default() -> Self {
        Self {
            window_samples: 24_000,
            hop_samples: 12_000,
        }
    }
}

/// Labels a recording from its voice alone — no transcriber in the path.
pub fn label_by_voice(
    speaker: SpeakerConfig,
    wav: &Path,
    speakers: Option<usize>,
    windowing: Windowing,
) -> Result<Labeled, BenchError> {
    let samples = read(wav)?;
    let regions = speech(&samples)?;
    let mut windows = cut(samples.len(), windowing);
    for window in &mut windows {
        window.opens_speech = regions
            .iter()
            .any(|region| window.speaks_for.contains(&region.start));
    }

    let mut diarizer = crate::diarize::diarizer(speaker)?;
    let voices: Vec<Option<Vec<f32>>> = windows
        .iter()
        .map(|window| diarizer.embed(&samples[window.heard.clone()]))
        .collect::<Result<_, _>>()
        .map_err(BenchError::SpeakerModel)?;

    // A similarity threshold must serve every recording; a speaker count is
    // a fact about this one. Given a count, the windows are grouped until
    // exactly that many remain.
    let mut named = match speakers {
        Some(count) => group_into(&voices, count),
        None => diarizer.relabel(&voices),
    };
    // A window the embedder would not judge still covers real speech.
    // Reporting silence there is a miss; a neighbour's name is the better
    // guess, and the one the shipped path already makes.
    aria::speaker::settle(&mut named, |index| windows[index].opens_speech);

    let mut turns: Vec<Turn> = named
        .into_iter()
        .zip(&windows)
        .flat_map(|(speaker, window)| {
            let spoken = speech_within(&window.speaks_for, &regions);
            spoken.into_iter().filter_map(move |stretch| {
                speaker.clone().map(|speaker| Turn {
                    start: stretch.start as f64 / SAMPLE_RATE,
                    duration: (stretch.end - stretch.start) as f64 / SAMPLE_RATE,
                    speaker,
                })
            })
        })
        .collect();
    sequence(&mut turns);
    merge(&mut turns);

    Ok(Labeled {
        turns,
        dropped_segments: voices.iter().filter(|voice| voice.is_none()).count(),
        dropped_seconds: 0.0,
        embeddings: windows
            .iter()
            .zip(voices)
            // The stretch it answers for, not the wider audio it listened
            // to. Dumping the listening windows would overlap them, and an
            // offline re-cluster would score every overlap as false alarm.
            .map(|(window, embedding)| Embedded {
                start: window.speaks_for.start as f64 / SAMPLE_RATE,
                end: window.speaks_for.end as f64 / SAMPLE_RATE,
                embedding,
            })
            .collect(),
    })
}

/// One embedding window: the audio it listens to, and the stretch of the
/// recording it answers for. The two differ because a window reaches wider
/// than its hop to give the embedder enough voice, while the stretches it
/// speaks for tile the recording exactly.
struct Window {
    heard: Range<usize>,
    speaks_for: Range<usize>,
    /// Whether speech starts inside this window's stretch. One that opens
    /// speech may follow a speaker change, so it takes its name from what
    /// comes after rather than what came before.
    opens_speech: bool,
}

/// Uses fixed windows to avoid embedding the detector's shortest speech runs.
/// Speech detection later determines which parts contribute to labeled turns.
fn cut(samples: usize, windowing: Windowing) -> Vec<Window> {
    let Windowing {
        window_samples,
        hop_samples,
    } = windowing;
    (0..samples)
        .step_by(hop_samples)
        .map(|start| Window {
            // The tail of a recording reaches back rather than falling short,
            // so the last window is a full one wherever the audio allows.
            heard: start.min(samples.saturating_sub(window_samples))
                ..(start + window_samples).min(samples),
            speaks_for: start..(start + hop_samples).min(samples),
            opens_speech: false,
        })
        .collect()
}

/// Groups voiceprints until exactly `wanted` groups remain, and names each
/// group after the order it was first heard.
fn group_into(voices: &[Option<Vec<f32>>], wanted: usize) -> Vec<Option<String>> {
    group(voices, Stop::Count(wanted))
        .into_iter()
        .map(|number| number.map(|number| format!("Speaker {}", number + 1)))
        .collect()
}

/// The parts of a stretch that carry speech, as the detector marked it.
fn speech_within(stretch: &Range<usize>, regions: &[Range<usize>]) -> Vec<Range<usize>> {
    regions
        .iter()
        .filter_map(|region| {
            let start = region.start.max(stretch.start);
            let end = region.end.min(stretch.end);
            (start < end).then_some(start..end)
        })
        .collect()
}

/// Joins neighbouring turns that name the same voice, so a paragraph reads as
/// one turn rather than one per hop.
fn merge(turns: &mut Vec<Turn>) {
    let mut joined: Vec<Turn> = Vec::new();
    for turn in turns.drain(..) {
        match joined.last_mut() {
            Some(open)
                if open.speaker == turn.speaker
                    && (turn.start - (open.start + open.duration)).abs() < 1e-6 =>
            {
                open.duration += turn.duration;
            }
            _ => joined.push(turn),
        }
    }
    *turns = joined;
}

pub(crate) fn read(wav: &Path) -> Result<Vec<f32>, BenchError> {
    let clip = read_wav(wav).map_err(|source| BenchError::Wav {
        scenario: "recording",
        source,
    })?;
    Ok(clip.into_canonical()?.into_samples())
}

pub(crate) fn speech(samples: &[f32]) -> Result<Vec<Range<usize>>, BenchError> {
    SileroVad::load()
        .and_then(|mut vad| vad.speech_regions(samples))
        .map_err(BenchError::SpeechGate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_tile_the_recording_by_hop() {
        let cuts = cut(Windowing::default().hop_samples * 4, Windowing::default());
        assert_eq!(cuts.len(), 4);
        for pair in cuts.windows(2) {
            assert_eq!(pair[0].speaks_for.end, pair[1].speaks_for.start);
        }
        assert_eq!(cuts[0].speaks_for.start, 0);
        assert_eq!(
            cuts.last().unwrap().speaks_for.end,
            Windowing::default().hop_samples * 4
        );
    }

    #[test]
    fn every_window_hears_a_full_length_of_audio() {
        for window in cut(
            Windowing::default().window_samples * 4,
            Windowing::default(),
        ) {
            assert_eq!(window.heard.len(), Windowing::default().window_samples);
        }
    }

    #[test]
    fn a_recording_shorter_than_one_window_still_yields_one() {
        let cuts = cut(Windowing::default().hop_samples / 2, Windowing::default());
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0].heard, 0..Windowing::default().hop_samples / 2);
    }

    #[test]
    fn only_the_speech_inside_a_stretch_is_reported() {
        let regions = Vec::from([0..100, 300..400]);
        let spoken = speech_within(&(50..350), &regions);
        assert_eq!(spoken, Vec::from([50..100, 300..350]));
    }

    fn turn(start: f64, duration: f64, speaker: &str) -> Turn {
        Turn {
            start,
            duration,
            speaker: speaker.to_owned(),
        }
    }

    #[test]
    fn touching_turns_of_one_voice_join() {
        let mut turns = Vec::from([
            turn(0.0, 1.0, "a"),
            turn(1.0, 1.0, "a"),
            turn(2.0, 1.0, "b"),
        ]);
        merge(&mut turns);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].duration, 2.0);
    }
}
