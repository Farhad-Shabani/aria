//! Caches segmentation and embedding outputs for repeated clustering experiments.
//! A parameter sweep can reuse model inference while varying grouping rules.

use std::io;
use std::path::{Path, PathBuf};

use aria::speaker::{Chunk, PyannoteVoices, Voiceprint};
use aria_kit::segmentation::{Activity, Segmenter};
use aria_kit::speaker::Embedder;
use serde::{Deserialize, Serialize};

use crate::diarize::BenchError;
use aria_kit::audio::RecordingReader;

/// Hears a recording through the track, or loads what an earlier run heard.
///
/// A cache under `dir` is used only when it was written on the same terms;
/// anything else is read afresh and overwritten. Pass `None` to skip the
/// cache entirely.
pub(crate) fn heard<S: Segmenter, E: Embedder>(
    voices: &mut PyannoteVoices<S, E>,
    recording: &RecordingReader,
    wav: &Path,
    nets: Nets<'_>,
    dir: Option<&Path>,
) -> Result<Vec<Chunk>, BenchError> {
    let terms = Terms::of(voices, wav, nets)?;
    let file = dir.map(|dir| path(dir, wav));

    if let Some(file) = &file
        && let Some(chunks) = load(file, &terms)
    {
        // The track never walked, so its own position stays at zero. Nothing
        // downstream of grouping reads it, and a cached run has no live
        // capture to release audio for.
        return Ok(chunks);
    }

    let mut chunks = Vec::new();
    while let Some(chunk) = voices.hear(recording) {
        chunks.push(chunk);
    }
    if let Some(file) = &file {
        save(file, &terms, &chunks).map_err(BenchError::Cache)?;
    }
    Ok(chunks)
}

/// The two nets whose answers a cache holds.
#[derive(Clone, Copy)]
pub(crate) struct Nets<'a> {
    pub(crate) segmentation: &'a Path,
    pub(crate) speaker: &'a Path,
}

/// What a cache describes. Anything that changes what the nets hear lives
/// here, so a stale cache is a mismatch rather than a wrong answer.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
struct Terms {
    audio: Fingerprint,
    segmentation: Fingerprint,
    speaker: Fingerprint,
    chunk_samples: usize,
    frame_shift: usize,
    frame_size: usize,
    speakers: usize,
    hop: u64,
}

/// File identity by path, size, and modification time. Content changes that
/// preserve all three are not detected.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
struct Fingerprint {
    path: String,
    bytes: u64,
    modified: u64,
}

impl Fingerprint {
    fn of(file: &Path) -> Result<Self, BenchError> {
        let facts = std::fs::metadata(file).map_err(BenchError::Cache)?;
        let modified = facts
            .modified()
            .map_err(BenchError::Cache)?
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        Ok(Self {
            path: file.display().to_string(),
            bytes: facts.len(),
            modified,
        })
    }
}

impl Terms {
    fn of<S: Segmenter, E: Embedder>(
        voices: &PyannoteVoices<S, E>,
        wav: &Path,
        nets: Nets<'_>,
    ) -> Result<Self, BenchError> {
        let shape = voices.shape();
        Ok(Self {
            audio: Fingerprint::of(wav)?,
            segmentation: Fingerprint::of(nets.segmentation)?,
            speaker: Fingerprint::of(nets.speaker)?,
            chunk_samples: shape.samples,
            frame_shift: shape.frame_shift,
            frame_size: shape.frame_size,
            speakers: shape.speakers,
            hop: voices.hop(),
        })
    }
}

/// One recording's chunks as they sit on disk.
#[derive(Serialize, Deserialize)]
struct Cached {
    terms: Terms,
    chunks: Vec<CachedChunk>,
}

#[derive(Serialize, Deserialize)]
struct CachedChunk {
    at: u64,
    /// One byte per frame, bit `n` for the chunk's local voice `n`.
    frames: Vec<u8>,
    voices: Vec<CachedVoiceprint>,
}

#[derive(Serialize, Deserialize)]
struct CachedVoiceprint {
    local: usize,
    values: Vec<f32>,
}

/// Appended rather than swapped for the extension: a corpus names its cuts
/// `ES2004b.cut.wav`, and replacing `.cut` would fold every cut of one
/// meeting onto a single file.
fn path(dir: &Path, wav: &Path) -> PathBuf {
    let mut name = wav.file_name().unwrap_or(wav.as_os_str()).to_os_string();
    name.push(".chunks.json");
    dir.join(name)
}

/// One frame's speakers as a byte, bit `n` for local voice `n`. Built from
/// the public predicate rather than the set's own byte, so a frame read back
/// can never claim a voice the segmenter would not have.
fn pack(frame: Activity) -> u8 {
    (0..Activity::MAX_SPEAKERS)
        .filter(|&local| frame.speaks(local))
        .fold(0, |bits, local| bits | 1 << local)
}

fn unpack(bits: u8) -> Activity {
    (0..Activity::MAX_SPEAKERS)
        .filter(|&local| bits & (1 << local) != 0)
        .fold(Activity::SILENT, Activity::speaking)
}

/// The cached chunks, when a cache is there and describes this run.
fn load(file: &Path, terms: &Terms) -> Option<Vec<Chunk>> {
    let text = std::fs::read_to_string(file).ok()?;
    let cached: Cached = serde_json::from_str(&text).ok()?;
    if &cached.terms != terms {
        tracing::info!(?file, "cached chunks were heard on other terms; re-reading");
        return None;
    }
    let chunks = cached
        .chunks
        .into_iter()
        .map(|chunk| Chunk {
            at: chunk.at,
            frames: chunk.frames.into_iter().map(unpack).collect(),
            voices: chunk
                .voices
                .into_iter()
                .map(|voice| Voiceprint {
                    local: voice.local,
                    values: voice.values,
                })
                .collect(),
        })
        .collect();
    Some(chunks)
}

fn save(file: &Path, terms: &Terms, chunks: &[Chunk]) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let cached = Cached {
        terms: terms.clone(),
        chunks: chunks
            .iter()
            .map(|chunk| CachedChunk {
                at: chunk.at,
                frames: chunk.frames.iter().copied().map(pack).collect(),
                voices: chunk
                    .voices
                    .iter()
                    .map(|voice| CachedVoiceprint {
                        local: voice.local,
                        values: voice.values.clone(),
                    })
                    .collect(),
            })
            .collect(),
    };
    let text = serde_json::to_string(&cached).map_err(io::Error::other)?;
    std::fs::write(file, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(path: &str) -> Fingerprint {
        Fingerprint {
            path: path.to_owned(),
            bytes: 1024,
            modified: 1_700_000_000,
        }
    }

    fn terms() -> Terms {
        Terms {
            audio: fingerprint("meeting.wav"),
            segmentation: fingerprint("segmentation-3.0.onnx"),
            speaker: fingerprint("wespeaker.onnx"),
            chunk_samples: 160_000,
            frame_shift: 270,
            frame_size: 991,
            speakers: 3,
            hop: 32_000,
        }
    }

    fn chunk() -> Chunk {
        Chunk {
            at: 32_000,
            frames: vec![
                Activity::SILENT,
                Activity::SILENT.speaking(0),
                Activity::SILENT.speaking(0).speaking(2),
            ],
            voices: vec![Voiceprint {
                local: 2,
                values: vec![0.5, -0.25],
            }],
        }
    }

    #[test]
    fn a_saved_chunk_comes_back_whole() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("one.chunks.json");
        save(&file, &terms(), &[chunk()]).expect("saving");

        let read = load(&file, &terms()).expect("the cache to describe this run");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].at, 32_000);
        assert_eq!(read[0].frames, chunk().frames, "the frame masks survive");
        assert_eq!(read[0].voices[0].local, 2);
        assert_eq!(read[0].voices[0].values, [0.5, -0.25]);
    }

    #[test]
    fn a_cache_heard_on_other_terms_is_not_used() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("one.chunks.json");
        save(&file, &terms(), &[chunk()]).expect("saving");

        let moved = Terms {
            hop: 16_000,
            ..terms()
        };
        assert!(load(&file, &moved).is_none(), "a different hop moves them");
    }

    #[test]
    fn a_cache_heard_through_another_model_is_not_used() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = dir.path().join("one.chunks.json");
        save(&file, &terms(), &[chunk()]).expect("saving");

        let swapped = Terms {
            segmentation: fingerprint("community-1.onnx"),
            ..terms()
        };
        assert!(load(&file, &swapped).is_none(), "another net, another run");

        let embedder = Terms {
            speaker: Fingerprint {
                bytes: 2048,
                ..fingerprint("wespeaker.onnx")
            },
            ..terms()
        };
        assert!(load(&file, &embedder).is_none(), "a rebuilt embedder too");
    }

    #[test]
    fn two_cuts_of_one_meeting_keep_separate_caches() {
        let dir = Path::new("/cache");
        assert_ne!(
            path(dir, Path::new("ES2004b.cut.wav")),
            path(dir, Path::new("ES2004b.far.wav")),
        );
    }

    #[test]
    fn a_frame_survives_the_trip_as_a_byte() {
        let frame = Activity::SILENT.speaking(0).speaking(2);
        assert_eq!(unpack(pack(frame)), frame);
        assert_eq!(pack(Activity::SILENT), 0);
    }

    #[test]
    fn a_cache_that_is_not_there_is_not_an_error() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(load(&dir.path().join("absent.chunks.json"), &terms()).is_none());
    }
}
