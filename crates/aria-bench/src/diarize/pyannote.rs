//! Segmentation first, embedding second — the shape pyannote uses.
//!
//! Segments overlapping audio into local speaker masks, embeds each mask, and
//! clusters embeddings across chunks. Frames can retain more than one speaker.

use std::ops::Range;
use std::path::Path;

use aria_kit::backend::BackendError;
use aria_kit::speaker::SpeakerError;
use aria_speaker::wespeaker::{Config as SpeakerConfig, WeSpeaker};
use ort::session::Session;
use ort::value::Tensor;

use crate::diarize::cluster::{Stop, group, group_online};
use crate::diarize::{BenchError, Embedded, Labeled, SAMPLE_RATE, Turn};

/// The chunk the model was trained on: ten seconds at 16 kHz.
const WINDOW_SAMPLES: usize = 160_000;
/// Samples between one output frame and the next.
const FRAME_SHIFT: usize = 270;
/// Samples an output frame answers for. Wider than the shift, so a frame's
/// time is its centre rather than its start.
const FRAME_SIZE: usize = 991;
/// Voices the model tells apart inside a single chunk.
const LOCAL_SPEAKERS: usize = 3;
const AUDIO_INPUT: &str = "x";
const SCORES_OUTPUT: &str = "y";

/// Which local voices each of the model's seven classes names: silence, then
/// each voice alone, then each pair. Three at once is not a class — the model
/// reports the two loudest.
const POWERSET: [&[usize]; 7] = [&[], &[0], &[1], &[2], &[0, 1], &[0, 2], &[1, 2]];

/// Frames a voice needs inside a chunk before its mask holds enough audio to
/// embed. Thirty frames is about half a second, which is what the embedder
/// asks for; below it the mask comes back unembedded anyway.
const MIN_ACTIVE_FRAMES: usize = 30;

/// A turn shorter than this is noise. pyannote's own default.
const MIN_TURN: f64 = 0.3;
/// A silence shorter than this closes rather than splitting one turn in two.
const MIN_GAP: f64 = 0.5;

/// How voiceprints become voices.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Clustering {
    /// Clusters all voiceprints after the recording ends.
    Batch,
    /// Assigns each voiceprint as it arrives.
    Online,
}

/// The knobs worth turning from the command line.
#[derive(Clone, Copy)]
pub struct Tuning {
    /// Whether the voices are settled all at once or as they arrive.
    pub clustering: Clustering,
    /// How far each chunk advances. Chunks overlap heavily so every frame is
    /// judged several times; a smaller shift means more chunks and a slower run.
    pub shift_samples: usize,
    /// Group into exactly this many voices, or use `threshold` when absent.
    pub speakers: Option<usize>,
    /// Cosine below which two voiceprints are different people.
    pub threshold: f32,
}

/// Labels a recording through the segmentation net. Turns may overlap: this
/// is the only path here that can report two people speaking at once.
pub fn label_by_segmentation(
    model: &Path,
    speaker: SpeakerConfig,
    wav: &Path,
    tuning: Tuning,
) -> Result<Labeled, BenchError> {
    let samples = crate::diarize::sibling::read(wav)?;
    let mut session = open(model)?;
    let starts = chunk_starts(samples.len(), tuning.shift_samples);
    let masks = starts
        .iter()
        .map(|&start| segment(&mut session, &samples, start))
        .collect::<Result<Vec<_>, _>>()?;

    let Some(mut diarizer) = aria::speaker::open::<WeSpeaker>(speaker, Vec::new()) else {
        return Err(BenchError::NoSpeakerModel);
    };
    let mut voices = Vec::new();
    for (chunk, mask) in masks.iter().enumerate() {
        for local in 0..LOCAL_SPEAKERS {
            if mask.iter().filter(|frame| frame[local]).count() < MIN_ACTIVE_FRAMES {
                continue;
            }
            let heard = masked(&samples, starts[chunk], mask, local);
            voices.push(Voice {
                chunk,
                local,
                span: at(starts[chunk])..at(starts[chunk] + WINDOW_SAMPLES),
                embedding: diarizer.embed(&heard),
            });
        }
    }

    let embeddings: Vec<Option<Vec<f32>>> = voices.iter().map(|v| v.embedding.clone()).collect();
    let numbers = match (tuning.clustering, tuning.speakers) {
        (Clustering::Online, _) => group_online(&embeddings, tuning.threshold),
        (Clustering::Batch, Some(count)) => group(&embeddings, Stop::Count(count)),
        (Clustering::Batch, None) => group(&embeddings, Stop::Similarity(tuning.threshold)),
    };

    let frames = frame_grid(starts.len(), tuning.shift_samples, samples.len());
    let mut turns = Vec::new();
    for (number, speech) in vote(&masks, &voices, &numbers, frames, tuning.shift_samples) {
        turns.extend(shape(&speech).into_iter().map(|span| Turn {
            start: span.start,
            duration: span.end - span.start,
            speaker: format!("Speaker {}", number + 1),
        }));
    }
    turns.sort_by(|a, b| a.start.total_cmp(&b.start));

    Ok(Labeled {
        turns,
        dropped_segments: voices.iter().filter(|v| v.embedding.is_none()).count(),
        dropped_seconds: 0.0,
        embeddings: voices
            .into_iter()
            .map(|voice| Embedded {
                start: voice.span.start,
                end: voice.span.end,
                embedding: voice.embedding,
            })
            .collect(),
    })
}

/// One voice the net found inside one chunk, and the voiceprint taken
/// through its mask.
struct Voice {
    chunk: usize,
    local: usize,
    span: Range<f64>,
    embedding: Option<Vec<f32>>,
}

/// How many frames the whole recording lands on. Frames past the end of the
/// audio come from the last chunk's zero padding and say nothing.
fn frame_grid(chunks: usize, shift: usize, samples: usize) -> usize {
    let spanned = (WINDOW_SAMPLES + chunks.saturating_sub(1) * shift) / FRAME_SHIFT + 1;
    spanned.min(samples / FRAME_SHIFT + 1)
}

/// Settles who speaks in each frame, then hands back each voice's frames.
///
/// A frame is judged by every chunk covering it, and those chunks disagree —
/// one may have run out of local voices, another may have split a person in
/// two. So the frame keeps as many speakers as the chunks counted on average,
/// filled by the clusters with the most votes. That is what stops a
/// briefly-confused chunk from inventing a fourth person in the transcript.
fn vote(
    masks: &[Vec<[bool; LOCAL_SPEAKERS]>],
    voices: &[Voice],
    numbers: &[Option<usize>],
    frames_total: usize,
    shift: usize,
) -> Vec<(usize, Vec<bool>)> {
    let speakers = numbers
        .iter()
        .flatten()
        .copied()
        .max()
        .map_or(0, |top| top + 1);
    if speakers == 0 || frames_total == 0 {
        return Vec::new();
    }
    let mut cluster_of = vec![None; masks.len() * LOCAL_SPEAKERS];
    for (voice, number) in voices.iter().zip(numbers) {
        cluster_of[voice.chunk * LOCAL_SPEAKERS + voice.local] = *number;
    }

    let mut votes = vec![0_u32; frames_total * speakers];
    let mut heads = vec![0_f32; frames_total];
    let mut judged = vec![0_f32; frames_total];
    for (chunk, mask) in masks.iter().enumerate() {
        let offset = (chunk * shift + FRAME_SHIFT / 2) / FRAME_SHIFT;
        for (frame, active) in mask.iter().enumerate() {
            let Some(at) = Some(offset + frame).filter(|at| *at < frames_total) else {
                break;
            };
            judged[at] += 1.0;
            heads[at] += active.iter().filter(|speaking| **speaking).count() as f32;
            for local in 0..LOCAL_SPEAKERS {
                if let (true, Some(number)) =
                    (active[local], cluster_of[chunk * LOCAL_SPEAKERS + local])
                {
                    votes[at * speakers + number] += 1;
                }
            }
        }
    }

    let mut speech = vec![vec![false; frames_total]; speakers];
    let mut ranked: Vec<usize> = (0..speakers).collect();
    for at in 0..frames_total {
        let row = &votes[at * speakers..(at + 1) * speakers];
        let heard = (heads[at] / judged[at].max(1.0) + 0.5) as usize;
        ranked.sort_by_key(|&number| std::cmp::Reverse(row[number]));
        for &number in ranked.iter().take(heard) {
            if row[number] > 0 {
                speech[number][at] = true;
            }
        }
    }
    speech.into_iter().enumerate().collect()
}

/// Turns a voice's active frames into the spans worth reporting: runs of
/// speech, short gaps closed, short turns dropped.
fn shape(speech: &[bool]) -> Vec<Range<f64>> {
    // A frame's time is the centre of the audio it answers for, not its edge.
    let centre = FRAME_SIZE as f64 / SAMPLE_RATE * 0.5;
    let mut spans: Vec<Range<f64>> = Vec::new();
    for (frame, _) in speech.iter().enumerate().filter(|(_, active)| **active) {
        let start = frame as f64 * FRAME_SHIFT as f64 / SAMPLE_RATE + centre;
        let end = start + FRAME_SHIFT as f64 / SAMPLE_RATE;
        match spans.last_mut() {
            Some(open) if start - open.end <= MIN_GAP => open.end = end,
            _ => spans.push(start..end),
        }
    }
    spans.retain(|span| span.end - span.start >= MIN_TURN);
    spans
}

/// Concatenates the audio frames marked active for one local speaker.
fn masked(
    samples: &[f32],
    start: usize,
    mask: &[[bool; LOCAL_SPEAKERS]],
    local: usize,
) -> Vec<f32> {
    let frames = mask.len().max(1);
    mask.iter()
        .enumerate()
        .filter(|(_, active)| active[local])
        .flat_map(|(frame, _)| {
            let from = (start + frame * WINDOW_SAMPLES / frames).min(samples.len());
            let to = (start + (frame + 1) * WINDOW_SAMPLES / frames).min(samples.len());
            samples[from..to].iter().copied()
        })
        .collect()
}

/// Where each chunk begins. The last one runs past the end of the recording
/// and is zero-padded, so the tail is segmented rather than dropped.
fn chunk_starts(samples: usize, shift: usize) -> Vec<usize> {
    let reach = samples.saturating_sub(WINDOW_SAMPLES);
    let mut starts: Vec<usize> = (0..=reach).step_by(shift.max(1)).collect();
    if starts
        .last()
        .is_none_or(|&last| last + WINDOW_SAMPLES < samples)
    {
        starts.push(starts.last().map_or(0, |&last| last + shift));
    }
    starts
}

/// Who speaks in each frame of one chunk. The model scores seven classes per
/// frame and the winner names the voices; there is nothing to threshold.
fn segment(
    session: &mut Session,
    samples: &[f32],
    start: usize,
) -> Result<Vec<[bool; LOCAL_SPEAKERS]>, BenchError> {
    let mut chunk = vec![0.0_f32; WINDOW_SAMPLES];
    let heard = samples.len().saturating_sub(start).min(WINDOW_SAMPLES);
    chunk[..heard].copy_from_slice(&samples[start..start + heard]);

    let shape = [1_i64, 1, WINDOW_SAMPLES as i64];
    let input = Tensor::from_array((shape, chunk)).map_err(inference)?;
    let mut outputs = session
        .run(ort::inputs![AUDIO_INPUT => input])
        .map_err(inference)?;
    let scores = outputs.remove(SCORES_OUTPUT).ok_or_else(|| {
        BenchError::SpeakerModel(SpeakerError::Inference {
            backend: "pyannote",
            source: BackendError::Unusable(format!("the model returned no {SCORES_OUTPUT} output")),
        })
    })?;
    let (shape, values) = scores.try_extract_tensor::<f32>().map_err(inference)?;
    let classes = *shape.last().unwrap_or(&0) as usize;
    if classes != POWERSET.len() {
        return Err(BenchError::SpeakerModel(SpeakerError::Inference {
            backend: "pyannote",
            source: BackendError::Unusable(format!(
                "the model scores {classes} classes, expected {}",
                POWERSET.len()
            )),
        }));
    }
    Ok(values.chunks_exact(classes).map(loudest).collect())
}

/// The voices named by the highest-scoring class of one frame.
fn loudest(frame: &[f32]) -> [bool; LOCAL_SPEAKERS] {
    let winner = frame
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(class, _)| class);
    let mut active = [false; LOCAL_SPEAKERS];
    for &voice in POWERSET[winner] {
        active[voice] = true;
    }
    active
}

fn open(model: &Path) -> Result<Session, BenchError> {
    Session::builder()
        .and_then(|mut builder| builder.commit_from_file(model))
        .map_err(|source| {
            BenchError::SpeakerModel(SpeakerError::Load {
                backend: "pyannote",
                model: model.display().to_string(),
                source: BackendError::engine(source),
            })
        })
}

fn inference(source: ort::Error) -> BenchError {
    BenchError::SpeakerModel(SpeakerError::Inference {
        backend: "pyannote",
        source: BackendError::engine(source),
    })
}

fn at(sample: usize) -> f64 {
    sample as f64 / SAMPLE_RATE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_classes_name_silence_then_singles_then_pairs() {
        assert!(POWERSET[0].is_empty());
        assert_eq!(
            loudest(&[0.0, 9.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
            [true, false, false]
        );
        assert_eq!(
            loudest(&[0.0, 0.0, 0.0, 0.0, 9.0, 0.0, 0.0]),
            [true, true, false]
        );
        assert_eq!(loudest(&[9.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]), [false; 3]);
    }

    #[test]
    fn chunks_cover_a_recording_that_ends_mid_window() {
        let starts = chunk_starts(WINDOW_SAMPLES + 5_000, 16_000);
        assert_eq!(starts.first(), Some(&0));
        assert!(
            starts
                .last()
                .is_some_and(|&last| last + WINDOW_SAMPLES >= WINDOW_SAMPLES + 5_000)
        );
    }

    #[test]
    fn a_recording_shorter_than_one_chunk_still_gets_one() {
        assert_eq!(chunk_starts(1_000, 16_000), Vec::from([0]));
    }

    #[test]
    fn only_a_voices_own_frames_reach_the_embedder() {
        let samples: Vec<f32> = (0..WINDOW_SAMPLES).map(|s| s as f32).collect();
        let mut mask = vec![[false; LOCAL_SPEAKERS]; 4];
        mask[1][0] = true;
        let heard = masked(&samples, 0, &mask, 0);
        assert_eq!(heard.len(), WINDOW_SAMPLES / 4);
        assert_eq!(heard[0], (WINDOW_SAMPLES / 4) as f32);
    }

    #[test]
    fn a_short_gap_closes_and_a_short_turn_is_dropped() {
        // Two runs a hair apart become one turn; a lone frame is noise.
        let mut speech = vec![false; 200];
        speech[10..50].fill(true);
        speech[52..90].fill(true);
        speech[150] = true;
        let spans = shape(&speech);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].end - spans[0].start > 1.3);
    }
}
