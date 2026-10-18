//! A voice track built on a segmentation net.
//!
//! The segmentation model predicts active voices per frame. Each voice is
//! embedded from its active frames; overlapping speech remains mixed audio.
//! Overlapping chunks vote on frame labels, and voiceprints are grouped
//! as they arrive so labels are available during recording.

use std::ops::Range;

use aria_kit::segmentation::{Activity, ChunkShape, Segmenter};
use aria_kit::speaker::Embedder;

use super::grid::FrameGrid;
use super::timeline::Timeline;
use super::{Merged, Revision, Spoken, Voices};
use crate::audio::RecordingReader;
use crate::speaker::Diarizer;

/// How far the track advances between chunks. Every frame is then judged by
/// `chunk / hop` chunks, so halving the hop doubles the cost of the run.
const HOP_SAMPLES: u64 = 32_000;

/// Drop shorter speech runs and join gaps shorter than MIN_GAP.
const MIN_TURN: f64 = 0.3;
const MIN_GAP: f64 = 0.5;

/// Frames a voice needs inside a chunk before its mask holds enough audio to
/// embed — about half a second, which is what the embedder asks for. Below
/// it the mask comes back unembedded anyway.
const MIN_ACTIVE_FRAMES: usize = 30;

/// Clustering thresholds, adjustable without rerunning model inference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grouping {
    /// Minimum average cosine similarity for joining masked voiceprints.
    pub threshold: f32,
    /// Minimum cluster size for assigning a label, suppressing isolated bad masks.
    pub least_voiceprints: usize,
}

impl Default for Grouping {
    fn default() -> Self {
        Self {
            threshold: 0.25,
            least_voiceprints: 12,
        }
    }
}

/// One voice as it stands right now, for a caller watching a session settle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Standing {
    pub name: String,
    pub voiceprints: usize,
    /// Whether the cluster has enough voiceprints to assign labels.
    pub eligible: bool,
}

pub struct PyannoteVoices<S, E> {
    segmenter: S,
    diarizer: Diarizer<E>,
    grouping: Grouping,
    shape: ChunkShape,
    grid: FrameGrid,
    voices: Vec<Voice>,
    /// The first chunk not yet segmented. Audio before it is spent.
    next_chunk: u64,
    /// Who spoke in each frame, as the overlapping chunks voted.
    timeline: Timeline,
    /// Names handed out so far. A merge drops a voice from the list, so the
    /// list's length would hand the next voice a name somebody is still
    /// using — and both lookups here match by name.
    issued: usize,
}

/// Model outputs for one chunk, reusable across clustering threshold sweeps.
pub struct Chunk {
    /// Where the chunk starts, in stream positions.
    pub at: u64,
    /// Which of the chunk's own voices speak in each frame.
    pub frames: Vec<Activity>,
    /// One entry per local voice that held enough audio to embed.
    pub voices: Vec<Voiceprint>,
}

/// What one local voice sounded like, embedded through its own frame mask.
pub struct Voiceprint {
    /// Which of the chunk's voices this is. It names nobody: the same person
    /// is local 0 in one chunk and local 2 in the next.
    pub local: usize,
    pub values: Vec<f32>,
}

/// One voice heard across the recording: a name and the sum of every
/// voiceprint placed under it. When it spoke is the timeline's business.
struct Voice {
    name: String,
    /// Whether the name came from an enrolled profile rather than a number.
    /// Two of these are two people, however alike they sound.
    enrolled: bool,
    sum: Vec<f32>,
    count: usize,
}

impl<S: Segmenter, E: Embedder> PyannoteVoices<S, E> {
    pub fn new(segmenter: S, diarizer: Diarizer<E>, grouping: Grouping) -> Self {
        let shape = segmenter.shape();
        Self {
            segmenter,
            diarizer,
            grouping,
            grid: FrameGrid::from(&shape),
            shape,
            voices: Vec::new(),
            next_chunk: 0,
            timeline: Timeline::default(),
            issued: 0,
        }
    }

    /// The chunk width the net answers on.
    pub fn shape(&self) -> &ChunkShape {
        &self.shape
    }

    /// How far the track advances between chunks. Together with the shape it
    /// fixes where every chunk sits, so a caller keeping chunks across runs
    /// can tell whether the ones it kept still describe this track.
    pub fn hop(&self) -> u64 {
        HOP_SAMPLES
    }

    /// Whether a chunk still stands at [`Self::next_chunk`].
    ///
    /// While capture runs, only a whole one — a short read then means audio
    /// is still on its way. Once it ends, whatever is left is the last
    /// chunk there will ever be, and reading it is the only chance.
    fn has_chunk(&self, recording: &RecordingReader) -> bool {
        match recording.reached_end() {
            true => self.next_chunk < recording.held_to(),
            false => recording.held_to() >= self.next_chunk + self.shape.samples as u64,
        }
    }

    /// The chunk at [`Self::next_chunk`], padded with silence when the
    /// recording ended inside it. `None` when it cannot be read at all.
    ///
    /// A short read has two causes and they are not alike. At the end of a
    /// finished recording there is simply no more audio, and the net reads a
    /// fixed width or nothing, so silence completes it honestly. Anywhere
    /// else it means the release floor slipped: audio nobody can read back
    /// is gone, and that is a fault rather than an ending.
    fn chunk_audio(&self, recording: &RecordingReader, chunk: Range<u64>) -> Option<Vec<f32>> {
        let from = chunk.start;
        let audio = recording.span(chunk);
        if audio.len() == self.shape.samples {
            return Some(audio.to_vec());
        }
        if !recording.reached_end() {
            tracing::warn!(
                wanted = self.shape.samples,
                got = audio.len(),
                lost_samples = self.shape.samples - audio.len(),
                from,
                "audio was released before the voice track read it; \
                 this stretch will have no speaker"
            );
            return None;
        }
        let mut padded = audio.to_vec();
        padded.resize(self.shape.samples, 0.0);
        Some(padded)
    }

    /// The next chunk the net can read, `None` once the recording holds no
    /// more. A chunk that cannot be read is skipped rather than ending the
    /// walk: its audio is spent either way.
    pub fn hear(&mut self, recording: &RecordingReader) -> Option<Chunk> {
        while self.has_chunk(recording) {
            let at = self.next_chunk;
            let span = at..at + self.shape.samples as u64;
            self.next_chunk += HOP_SAMPLES;

            let Some(audio) = self.chunk_audio(recording, span) else {
                continue;
            };
            let frames = match self.segmenter.segment(&audio) {
                Ok(frames) => frames,
                Err(error) => {
                    tracing::warn!(%error, "speech segmentation failed");
                    continue;
                }
            };
            let voices = (0..self.shape.speakers)
                .filter(|&local| {
                    frames.iter().filter(|frame| frame.speaks(local)).count() >= MIN_ACTIVE_FRAMES
                })
                .filter_map(|local| {
                    let heard = masked(&audio, &frames, local, &self.grid);
                    match self.diarizer.embed(&heard) {
                        Ok(values) => values.map(|values| Voiceprint { local, values }),
                        Err(error) => {
                            tracing::warn!(%error, "speaker embedding failed");
                            None
                        }
                    }
                })
                .collect();
            return Some(Chunk { at, frames, voices });
        }
        None
    }

    /// Places every voiceprint one chunk held, then folds any voices they
    /// proved to be one.
    pub fn group(&mut self, chunk: &Chunk) -> Revision {
        let at = self.grid.at(chunk.at);
        self.timeline.judge(at, &chunk.frames);

        // Keep distinct local voices separate during clustering. Names survive
        // merges that shift indices.
        let mut concurrent: Vec<String> = Vec::new();
        for voiceprint in &chunk.voices {
            let placed = self.place(&voiceprint.values, &concurrent);
            self.timeline
                .vote(placed, at, &chunk.frames, voiceprint.local);
            concurrent.push(self.voices[placed].name.clone());
        }
        // After the chunk, not inside it: merging between placements would
        // fuse two voices this chunk is about to prove distinct.
        self.merge(&concurrent)
    }

    /// The voice this voiceprint belongs to: an enrolled person, the voice
    /// it is most alike above the threshold, or a new one.
    ///
    /// `concurrent` excludes already placed voices from cluster matching.
    /// An enrolled-profile match takes precedence over this exclusion.
    fn place(&mut self, voiceprint: &[f32], concurrent: &[String]) -> usize {
        if let Some(name) = self.diarizer.enrolled_name(voiceprint) {
            return match self.voices.iter().position(|voice| voice.name == name) {
                Some(index) => self.absorb(index, voiceprint),
                None => self.open(name, true, voiceprint),
            };
        }
        let nearest = self
            .voices
            .iter()
            .enumerate()
            .filter(|(_, voice)| !concurrent.contains(&voice.name))
            .map(|(index, voice)| (index, alike(&voice.sum, voice.count, voiceprint, 1)))
            .reduce(|best, candidate| {
                if candidate.1 > best.1 {
                    candidate
                } else {
                    best
                }
            });
        if let Some((index, similarity)) = nearest
            && similarity >= self.grouping.threshold
        {
            return self.absorb(index, voiceprint);
        }
        // Taken at birth and never given back, so a merged number leaves a
        // gap rather than renaming a label somebody already read.
        self.issued += 1;
        let name = format!("Speaker {}", self.issued);
        self.open(name, false, voiceprint)
    }

    /// Adds a voiceprint to a voice already heard.
    fn absorb(&mut self, index: usize, voiceprint: &[f32]) -> usize {
        let voice = &mut self.voices[index];
        for (sum, value) in voice.sum.iter_mut().zip(voiceprint) {
            *sum += value;
        }
        voice.count += 1;
        index
    }

    /// Opens a voice under `name`.
    fn open(&mut self, name: String, enrolled: bool, voiceprint: &[f32]) -> usize {
        self.voices.push(Voice {
            name,
            enrolled,
            sum: voiceprint.to_vec(),
            count: 1,
        });
        self.timeline.open_voice();
        self.voices.len() - 1
    }

    /// Joins voices that have drifted into one another, keeping the name
    /// that was heard first. Absorbing a voiceprint moves a voice, so this
    /// runs after every chunk — it is over voices, of which a meeting holds
    /// a handful, not over voiceprints.
    fn merge(&mut self, concurrent: &[String]) -> Revision {
        let mut revision = Revision::default();
        while let Some((keeper, absorbed)) = self.closest_pair(concurrent) {
            self.timeline.absorb(keeper, absorbed);
            let taken = self.voices.remove(absorbed);
            revision.merges.push(Merged {
                from: taken.name.clone(),
                into: self.voices[keeper].name.clone(),
            });
            let voice = &mut self.voices[keeper];
            for (sum, value) in voice.sum.iter_mut().zip(&taken.sum) {
                *sum += value;
            }
            voice.count += taken.count;
        }
        revision
    }

    /// The two most alike voices, when they are alike enough to be one and
    /// nothing proves them apart. The earlier-heard of the pair is the keeper.
    fn closest_pair(&self, concurrent: &[String]) -> Option<(usize, usize)> {
        let mut best: Option<(usize, usize, f32)> = None;
        for a in 0..self.voices.len() {
            for b in a + 1..self.voices.len() {
                if self.apart(a, b, concurrent) {
                    continue;
                }
                let similarity = alike(
                    &self.voices[a].sum,
                    self.voices[a].count,
                    &self.voices[b].sum,
                    self.voices[b].count,
                );
                if best.is_none_or(|(_, _, top)| similarity > top) {
                    best = Some((a, b, similarity));
                }
            }
        }
        best.filter(|(_, _, similarity)| *similarity >= self.grouping.threshold)
            .map(|(a, b, _)| (a, b))
    }

    /// Prevents merges between voices placed in this chunk or carrying
    /// different enrolled names.
    fn apart(&self, a: usize, b: usize, concurrent: &[String]) -> bool {
        let (a, b) = (&self.voices[a], &self.voices[b]);
        let overlapped = concurrent.contains(&a.name) && concurrent.contains(&b.name);
        overlapped || (a.enrolled && b.enrolled)
    }

    /// Every voice the track is holding, in the order they were first heard.
    ///
    /// A caller sweeping [`Grouping`] reads this after each chunk to watch a
    /// session settle: how many people the track believes in, and how long
    /// it took to stop changing its mind.
    pub fn heard_so_far(&self) -> Vec<Standing> {
        self.voices
            .iter()
            .map(|voice| Standing {
                name: voice.name.clone(),
                voiceprints: voice.count,
                eligible: voice.count >= self.grouping.least_voiceprints,
            })
            .collect()
    }

    /// Which voices are heard often enough to be people, by position.
    fn eligible(&self) -> Vec<bool> {
        self.voices
            .iter()
            .map(|voice| voice.count >= self.grouping.least_voiceprints)
            .collect()
    }
}

impl<S: Segmenter, E: Embedder> PyannoteVoices<S, E> {
    /// The earliest stream position this track still needs. Its reader may
    /// release everything before it, and nothing before it can be re-read.
    pub fn retained_from(&self) -> u64 {
        self.next_chunk
    }

    /// Gives up on the audio before `position` and starts the next chunk
    /// there.
    ///
    /// The stretch skipped ends up with no speaker, which is what a track too
    /// far behind to read it would produce anyway. Not aligned to the hop:
    /// the timeline is indexed off absolute positions, so a new origin costs
    /// nothing but the chunks that were never read.
    pub fn skip_to(&mut self, position: u64) {
        self.next_chunk = self.next_chunk.max(position);
    }

    /// The timeline this track would draw if nobody asked it about spans:
    /// every stretch it heard a voice in, in order. Stretches of two voices
    /// at once appear once per voice.
    ///
    /// Nothing in the product reads this; the diarization bench scores it.
    pub fn turns(&self) -> Vec<Spoken> {
        // Runs of frames, per voice, with short silences closed and short
        // runs dropped — the shape a listener would call a turn.
        let eligible = self.eligible();
        let gap = self.grid.frames(MIN_GAP);
        let least = self.grid.frames(MIN_TURN);
        let mut spoken: Vec<Spoken> = self
            .timeline
            .runs(&eligible, gap, least)
            .into_iter()
            .map(|(voice, run)| Spoken {
                span: self.grid.start(run.start)..self.grid.start(run.end),
                speaker: self.voices[voice].name.clone(),
            })
            .collect();
        spoken.sort_by_key(|turn| turn.span.start);
        spoken
    }
}

impl<S: Segmenter, E: Embedder> Voices for PyannoteVoices<S, E> {
    fn attribute(&self, spans: &[Range<u64>]) -> Vec<Option<String>> {
        let eligible = self.eligible();
        spans
            .iter()
            .map(|span| {
                // A paragraph gets the voice assigned to the most frames in its span.
                let opens = self.grid.at(span.start);
                let mut owed = vec![0_usize; self.voices.len()];
                for frame in opens..self.grid.at(span.end).max(opens + 1) {
                    for voice in self.timeline.speakers_in(frame, &eligible) {
                        owed[voice] += 1;
                    }
                }
                owed.into_iter()
                    .enumerate()
                    .filter(|(_, held)| *held > 0)
                    .max_by_key(|(_, held)| *held)
                    .map(|(voice, _)| self.voices[voice].name.clone())
            })
            .collect()
    }
}

/// Concatenates frames where `local` is active. Other voices in those frames remain.
fn masked(chunk: &[f32], frames: &[Activity], local: usize, grid: &FrameGrid) -> Vec<f32> {
    frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame.speaks(local))
        .flat_map(|(frame, _)| {
            let heard = grid.window(frame);
            let from = heard.start.min(chunk.len());
            let to = heard.end.min(chunk.len());
            chunk[from..to].iter().copied()
        })
        .collect()
}

/// Average cosine between every voiceprint of one voice and every voiceprint
/// of another. With unit vectors that is the dot of the sums over the
/// product of the counts.
///
/// Unlike normalized centroid cosine, this also reflects cluster cohesion.
fn alike(a: &[f32], a_count: usize, b: &[f32], b_count: usize) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    dot / (a_count * b_count) as f32
}

/// Scripted nets, so a test can say what the graphs would have answered.
/// Shared with the track's own thread tests, which drive the same walk.
#[cfg(test)]
pub(crate) mod testing {
    use aria_kit::segmentation::SegmentationError;
    use aria_kit::speaker::SpeakerError;

    use super::*;

    /// A segmenter reading from a script rather than a graph, one chunk per
    /// call. The real one is an ONNX session; what is under test here is
    /// what the track does with its answers.
    pub(crate) struct ScriptedSegmenter {
        chunks: std::vec::IntoIter<Vec<Activity>>,
        /// Chunk width. Equal to the hop leaves chunks edge to edge; wider
        /// makes them overlap, which is what the real grid does.
        samples: usize,
    }

    /// Samples per frame in the scripted grid.
    pub(crate) const SCRIPTED_SHIFT: usize = 320;

    impl Segmenter for ScriptedSegmenter {
        fn shape(&self) -> ChunkShape {
            ChunkShape {
                samples: self.samples,
                frame_shift: SCRIPTED_SHIFT,
                frame_size: SCRIPTED_SHIFT,
                speakers: 3,
            }
        }

        fn segment(&mut self, _chunk: &[f32]) -> Result<Vec<Activity>, SegmentationError> {
            Ok(self.chunks.next().unwrap_or_default())
        }
    }

    /// An embedder that answers with the next scripted voiceprint. Two
    /// identical vectors are one person; orthogonal ones are two.
    pub(crate) struct ScriptedEmbedder {
        voiceprints: std::vec::IntoIter<[f32; 2]>,
    }

    impl Embedder for ScriptedEmbedder {
        fn embed(&mut self, _window: &[f32]) -> Result<Option<Vec<f32>>, SpeakerError> {
            Ok(self.voiceprints.next().map(Vec::from))
        }

        fn dimension(&self) -> usize {
            2
        }
    }

    /// A track whose net and embedder both read from a script: one chunk of
    /// frames and one voiceprint per entry, in lockstep.
    pub(crate) fn track(
        chunks: Vec<Vec<Activity>>,
        voiceprints: Vec<[f32; 2]>,
    ) -> PyannoteVoices<ScriptedSegmenter, ScriptedEmbedder> {
        track_of(HOP_SAMPLES as usize, chunks, voiceprints)
    }

    /// The same, under grouping rules other than the shipped ones.
    pub(crate) fn track_with(
        grouping: Grouping,
        chunks: Vec<Vec<Activity>>,
        voiceprints: Vec<[f32; 2]>,
    ) -> PyannoteVoices<ScriptedSegmenter, ScriptedEmbedder> {
        let mut voices = track_of(HOP_SAMPLES as usize, chunks, voiceprints);
        voices.grouping = grouping;
        voices
    }

    /// The same, over chunks `samples` wide.
    pub(crate) fn track_of(
        samples: usize,
        chunks: Vec<Vec<Activity>>,
        voiceprints: Vec<[f32; 2]>,
    ) -> PyannoteVoices<ScriptedSegmenter, ScriptedEmbedder> {
        PyannoteVoices::new(
            ScriptedSegmenter {
                chunks: chunks.into_iter(),
                samples,
            },
            Diarizer::new(
                ScriptedEmbedder {
                    voiceprints: voiceprints.into_iter(),
                },
                Vec::new(),
            ),
            Grouping::default(),
        )
    }

    /// What every track here is built with, so a changed default moves the
    /// tests with it rather than leaving them asserting an old number.
    pub(crate) fn min_voiceprints() -> usize {
        Grouping::default().least_voiceprints
    }

    /// Walks everything the recording holds, the way the track's own thread
    /// does. The tests are about what grouping decides, not about pacing.
    pub(crate) fn hear_all<S: Segmenter, E: Embedder>(
        voices: &mut PyannoteVoices<S, E>,
        recording: &RecordingReader,
    ) -> Revision {
        let mut revision = Revision::default();
        while let Some(chunk) = voices.hear(recording) {
            revision.merges.append(&mut voices.group(&chunk).merges);
        }
        revision
    }

    /// A chunk in which `local` speaks throughout and nobody else does.
    pub(crate) fn alone(local: usize) -> Vec<Activity> {
        vec![Activity::SILENT.speaking(local); MIN_ACTIVE_FRAMES * 2]
    }

    /// A finished recording of `chunks` whole chunks. Filled here because
    /// `held_to()` reports what a reader has taken, and in a session the walk's
    /// own `fill_through` pulls it forward.
    pub(crate) fn recording_of(chunks: usize) -> RecordingReader {
        let mut recording =
            RecordingReader::from_samples(&vec![0.1; chunks * HOP_SAMPLES as usize]);
        recording.fill().expect("the pushed recording is whole");
        recording
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn overlapping_chunks_judge_the_same_frame_more_than_once() {
        // Chunks twice the hop, so the back half of each is judged again by
        // the next. Every other test runs them edge to edge, which never
        // exercises the counting `speakers_in` reads.
        let width = HOP_SAMPLES as usize * 2;
        let per_chunk = width / SCRIPTED_SHIFT;
        let heard = min_voiceprints() + 1;
        let speaking = vec![Activity::SILENT.speaking(0); per_chunk];
        let mut voices = track_of(width, vec![speaking; heard], vec![[1.0, 0.0]; heard]);
        hear_all(&mut voices, &recording_of(heard));

        // Two judgements hearing one voice each is still one speaker.
        let overlap = per_chunk / 2;
        let named = voices.timeline.speakers_in(overlap, &voices.eligible());
        assert_eq!(named.len(), 1, "the overlap named more than one voice");
    }

    #[test]
    fn a_track_told_to_skip_ahead_never_asks_for_the_audio_behind_it() {
        let recording = recording_of(4);
        let mut voices = track(
            Vec::from([alone(0), alone(0)]),
            Vec::from([[1.0, 0.0], [1.0, 0.0]]),
        );

        voices.skip_to(HOP_SAMPLES * 2);
        hear_all(&mut voices, &recording);

        assert_eq!(
            voices.retained_from(),
            HOP_SAMPLES * 4,
            "the walk should resume past the skip, not replay it"
        );
    }

    #[test]
    fn skipping_backwards_does_nothing() {
        let mut voices = track(Vec::new(), Vec::new());
        voices.skip_to(HOP_SAMPLES * 3);

        voices.skip_to(HOP_SAMPLES);

        assert_eq!(voices.retained_from(), HOP_SAMPLES * 3);
    }

    #[test]
    fn two_voiceprints_alike_enough_are_one_voice() {
        let recording = recording_of(2);
        let mut voices = track(
            Vec::from([alone(0), alone(0)]),
            Vec::from([[1.0, 0.0], [1.0, 0.0]]),
        );
        assert!(!hear_all(&mut voices, &recording).revised());
        assert_eq!(voices.voices.len(), 1);
    }

    #[test]
    fn a_voiceprint_matching_nothing_opens_a_voice_and_revises_nothing() {
        let recording = recording_of(2);
        let mut voices = track(
            Vec::from([alone(0), alone(0)]),
            Vec::from([[1.0, 0.0], [0.0, 1.0]]),
        );
        // A voice nobody had attributed becoming attributed changes no
        // label already written, so there is nothing to rewrite.
        assert!(!hear_all(&mut voices, &recording).revised());
        assert_eq!(voices.voices.len(), 2);
        assert_eq!(voices.voices[0].name, "Speaker 1");
        assert_eq!(voices.voices[1].name, "Speaker 2");
    }

    #[test]
    fn voices_that_drift_together_merge_and_the_earlier_name_survives() {
        let recording = recording_of(3);
        // Apart on arrival; the third voiceprint drags the first around
        // until the two are one.
        let mut voices = track(
            Vec::from([alone(0), alone(0), alone(0)]),
            Vec::from([[1.0, 0.0], [0.17365, 0.98481], [0.5, 0.86603]]),
        );
        let revision = hear_all(&mut voices, &recording);
        assert!(revision.revised());
        assert_eq!(revision.merges[0].from, "Speaker 2");
        assert_eq!(revision.merges[0].into, "Speaker 1");
        assert_eq!(voices.voices.len(), 1);
        assert_eq!(voices.voices[0].name, "Speaker 1");
    }

    #[test]
    fn a_merged_number_is_retired_rather_than_handed_to_the_next_voice() {
        let recording = recording_of(6);
        // Three voices apart on arrival. The fourth and fifth drag the first
        // onto the second until they merge, which drops the second from the
        // list; the sixth is a voice nobody has heard.
        let mut voices = track(
            Vec::from([alone(0), alone(0), alone(0), alone(0), alone(0), alone(0)]),
            Vec::from([
                [1.0, 0.0],
                [-0.1736, 0.9848],
                [-0.766, -0.643],
                [0.643, 0.766],
                [0.342, 0.940],
                [0.9, -0.436],
            ]),
        );
        let revision = hear_all(&mut voices, &recording);
        assert_eq!(revision.merges[0].from, "Speaker 2");

        let live: Vec<&str> = voices
            .voices
            .iter()
            .map(|voice| voice.name.as_str())
            .collect();
        // Naming from the list's length would call the newcomer "Speaker 3",
        // which "Speaker 3" is still answering to.
        assert_eq!(live, Vec::from(["Speaker 1", "Speaker 3", "Speaker 4"]));
    }

    #[test]
    fn a_span_takes_the_voice_that_owned_the_most_of_it() {
        // Each voice speaks for long enough to clear the floor below which a
        // voice names nothing.
        let half = min_voiceprints();
        let recording = recording_of(half * 2);
        let mut voiceprints = vec![[1.0, 0.0]; half];
        voiceprints.extend(vec![[0.0, 1.0]; half]);
        let mut voices = track(vec![alone(0); half * 2], voiceprints);
        hear_all(&mut voices, &recording);

        let turn = HOP_SAMPLES * half as u64;
        let named = voices.attribute(&Vec::from([0..turn, turn..turn * 2]));
        assert_eq!(
            named,
            Vec::from([Some("Speaker 1".to_owned()), Some("Speaker 2".to_owned())])
        );
    }

    #[test]
    fn a_voice_heard_too_few_times_names_nothing() {
        let brief = min_voiceprints() - 1;
        let recording = recording_of(brief);
        let mut voices = track(vec![alone(0); brief], vec![[1.0, 0.0]; brief]);
        hear_all(&mut voices, &recording);

        // One bad mask opens a voice, and nothing merges a singleton away.
        // It stays in the list, and stays out of the transcript.
        assert_eq!(voices.voices.len(), 1);
        let whole = 0..HOP_SAMPLES * brief as u64;
        let named = voices.attribute(&[whole]);
        assert_eq!(named, Vec::from([None]));
        assert!(voices.turns().is_empty());
    }

    #[test]
    fn lowering_the_bar_lets_a_rare_voice_name_its_audio() {
        // The same run as above, under a grouping that asks for less. This
        // is the knob a sweep turns, so it has to reach the answer.
        let brief = min_voiceprints() - 1;
        let recording = recording_of(brief);
        let mut voices = track_with(
            Grouping {
                least_voiceprints: brief,
                ..Grouping::default()
            },
            vec![alone(0); brief],
            vec![[1.0, 0.0]; brief],
        );
        hear_all(&mut voices, &recording);

        let whole = 0..HOP_SAMPLES * brief as u64;
        let named = voices.attribute(&[whole]);
        assert_eq!(named, Vec::from([Some("Speaker 1".to_owned())]));
    }

    #[test]
    fn a_threshold_nothing_can_reach_gives_every_chunk_its_own_voice() {
        let heard = min_voiceprints();
        let recording = recording_of(heard);
        // Voiceprints alike enough that the default would fold them into one.
        let mut voices = track_with(
            Grouping {
                threshold: 1.1,
                ..Grouping::default()
            },
            vec![alone(0); heard],
            vec![[1.0, 0.0]; heard],
        );
        hear_all(&mut voices, &recording);

        assert_eq!(voices.voices.len(), heard, "no two of them could join");
        assert!(
            voices.heard_so_far().iter().all(|voice| !voice.eligible),
            "and none is heard often enough to be written"
        );
    }

    #[test]
    fn the_stretch_after_the_last_whole_chunk_is_still_read() {
        let whole = min_voiceprints();
        // A recording that stops a third of the way into one more chunk.
        let tail = HOP_SAMPLES as usize / 3;
        let mut recording =
            RecordingReader::from_samples(&vec![0.1; whole * HOP_SAMPLES as usize + tail]);
        recording.fill().expect("the pushed recording is whole");

        let mut voices = track(vec![alone(0); whole + 1], vec![[1.0, 0.0]; whole + 1]);
        hear_all(&mut voices, &recording);

        // Waiting for a whole chunk that will never arrive leaves the last
        // stretch unheard, and the paragraphs over it unnamed.
        assert!(
            voices.retained_from() >= recording.held_to(),
            "the track stopped at {} with {} recorded",
            voices.retained_from(),
            recording.held_to()
        );
    }

    #[test]
    fn a_span_past_the_tracks_reach_stays_unnamed() {
        let heard = min_voiceprints();
        let recording = recording_of(heard);
        let mut voices = track(vec![alone(0); heard], vec![[1.0, 0.0]; heard]);
        hear_all(&mut voices, &recording);
        // The track heard nothing there, so it says nothing rather than guess.
        let past = HOP_SAMPLES * 100..HOP_SAMPLES * 101;
        let named = voices.attribute(std::slice::from_ref(&past));
        assert_eq!(named, Vec::from([None]));
    }

    #[test]
    fn a_voice_too_brief_to_embed_is_left_alone() {
        let recording = recording_of(1);
        let brief = vec![Activity::SILENT.speaking(0); MIN_ACTIVE_FRAMES - 1];
        let mut voices = track(Vec::from([brief]), Vec::from([[1.0, 0.0]]));
        hear_all(&mut voices, &recording);
        assert!(voices.voices.is_empty());
    }

    fn frames(pattern: &[&[usize]]) -> Vec<Activity> {
        pattern
            .iter()
            .map(|speaking| {
                speaking
                    .iter()
                    .fold(Activity::SILENT, |active, &voice| active.speaking(voice))
            })
            .collect()
    }

    /// A grid of `shift`-wide frames whose receptive field is `size` wide.
    fn grid(shift: usize, size: usize) -> FrameGrid {
        FrameGrid::from(&ChunkShape {
            samples: 8,
            frame_shift: shift,
            frame_size: size,
            speakers: 3,
        })
    }

    #[test]
    fn only_a_voices_own_frames_reach_the_embedder() {
        let chunk: Vec<f32> = (0..8).map(|sample| sample as f32).collect();
        let heard = masked(&chunk, &frames(&[&[], &[0], &[1], &[0]]), 0, &grid(2, 2));
        assert_eq!(heard, Vec::from([2.0, 3.0, 6.0, 7.0]));
    }

    #[test]
    fn a_frame_reaches_past_its_own_shift_and_masks_from_the_middle() {
        let chunk: Vec<f32> = (0..8).map(|sample| sample as f32).collect();
        // Frames step by 2 but answer for 4 samples, so frame 1 is centred
        // one shift in: it owns samples 3 and 4, not 2 and 3.
        let heard = masked(&chunk, &frames(&[&[], &[0]]), 0, &grid(2, 4));
        assert_eq!(heard, Vec::from([3.0, 4.0]));
    }

    #[test]
    fn a_frame_nobody_speaks_in_masks_to_nothing() {
        let chunk = vec![1.0_f32; 8];
        assert!(masked(&chunk, &frames(&[&[], &[]]), 0, &grid(2, 2)).is_empty());
    }

    #[test]
    fn one_voice_is_as_alike_to_itself_as_a_cosine_gets() {
        let voice = [0.6, 0.8];
        assert!((alike(&voice, 1, &voice, 1) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn averaging_over_members_keeps_a_spread_voice_honest() {
        // Two members pointing 90 degrees apart average to a half-cosine
        // against either of them, not to the tight cosine their sum implies.
        let spread = [1.0, 1.0];
        let one = [1.0, 0.0];
        assert!((alike(&spread, 2, &one, 1) - 0.5).abs() < 1e-6);
    }
}
