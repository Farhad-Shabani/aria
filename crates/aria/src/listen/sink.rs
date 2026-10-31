//! The transcript file a listen session writes as it hears.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::audio::canonical_duration;
use time::OffsetDateTime;
use time::macros::format_description;

use crate::dictation::{TranscriptSink, Utterance};
use crate::speaker::Label;

/// A pause this long always starts a new paragraph.
pub const GAP_BREAK: Duration = Duration::from_secs(3);
/// After a sentence-final segment, even a short pause starts one — it often
/// lands near a speaker change.
const SENTENCE_GAP: Duration = Duration::from_secs(1);
/// Gapless audio (music, back-to-back talk) still breaks eventually.
const MAX_PARAGRAPH_CHARS: usize = 600;

/// One transcript file per session: timestamped paragraphs, broken on pauses
/// and speaker changes. A paragraph is flushed whole when it closes, so a
/// crash loses at most the open paragraph. Live labels are tentative;
/// [`TranscriptFile::rewrite`] replaces them with settled ones, as often as
/// a session needs.
pub(crate) struct TranscriptFile {
    file: File,
    path: PathBuf,
    /// Bytes already in the file at open — `create_at` appends, and the
    /// rewrite must never touch them.
    preamble: u64,
    paragraph: String,
    /// Seconds into the recording at the open paragraph's first and latest
    /// segments.
    paragraph_at: u64,
    paragraph_ended: u64,
    label: Option<Label>,
    /// The open paragraph's audio so far.
    paragraph_span: Option<Range<u64>>,
    /// Sum of the open paragraph's segment embeddings.
    paragraph_embedding: Vec<f32>,
    last_segment: Instant,
    separate: bool,
    history: Vec<Paragraph>,
}

struct Paragraph {
    at: u64,
    ended: u64,
    /// Where its audio sat in the stream. The voice track answers in these,
    /// not in seconds, so a rewrite has something exact to ask about. `None`
    /// when the engine timed nothing in it.
    span: Option<Range<u64>>,
    /// Unit-norm. `None` when no segment in it carried an embedding.
    embedding: Option<Vec<f32>>,
    text: String,
}

impl TranscriptFile {
    /// A timestamped file in the transcripts directory.
    pub(crate) fn create(directory: &Path) -> io::Result<(Self, PathBuf)> {
        let stamp = OffsetDateTime::now_utc()
            .format(format_description!(
                "[year]-[month]-[day]T[hour]-[minute]-[second]Z"
            ))
            .map_err(io::Error::other)?;
        Self::create_at(directory.join(format!("{stamp}.txt")))
    }

    /// The caller's exact file, appended to when it already exists.
    pub(crate) fn create_at(path: PathBuf) -> io::Result<(Self, PathBuf)> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = File::options().create(true).append(true).open(&path)?;
        let preamble = file.metadata()?.len();
        let now = Instant::now();
        Ok((
            Self {
                file,
                path: path.clone(),
                preamble,
                paragraph: String::new(),
                paragraph_at: 0,
                paragraph_ended: 0,
                paragraph_span: None,
                paragraph_embedding: Vec::new(),
                label: None,
                last_segment: now,
                separate: false,
                history: Vec::new(),
            },
            path,
        ))
    }

    /// Where each written paragraph's audio sat, in stream positions. What a
    /// voice track needs to say who owned them.
    ///
    /// `None` where nothing in the paragraph was timed. There is no audio to
    /// point at, and pointing at the wrong audio would still get an answer.
    fn paragraph_spans(&self) -> Vec<Option<Range<u64>>> {
        self.history
            .iter()
            .map(|paragraph| paragraph.span.clone())
            .collect()
    }

    /// Rewrites every paragraph written so far under `labels`, leaving the
    /// preamble bytes untouched. Rename-atomic: the streamed file stands
    /// until the replacement is whole. Safe to run again — a session that
    /// keeps correcting itself calls this once per correction.
    /// `label` is asked after the open paragraph closes, so the spans it
    /// answers about include it. It may answer for only the first
    /// paragraphs; the rest lose their labels rather than borrow a
    /// neighbour's.
    pub(crate) fn rewrite(
        &mut self,
        label: impl FnOnce(&[Option<Range<u64>>]) -> Vec<Option<Label>>,
    ) -> io::Result<()> {
        self.flush_paragraph()?;
        if self.history.is_empty() {
            return Ok(());
        }
        let mut labels = label(&self.paragraph_spans());
        crate::speaker::settle(&mut labels, |index| {
            self.history.get(index + 1).is_some_and(|next| {
                next.at.saturating_sub(self.history[index].ended) < GAP_BREAK.as_secs()
            })
        });
        labels.resize(self.history.len(), None);

        // Live label jitter splits paragraphs mid-utterance; equal final
        // labels with no real pause between them read back as one.
        struct Block {
            at: u64,
            ended: u64,
            label: Option<Label>,
            text: String,
        }
        let mut blocks: Vec<Block> = Vec::new();
        for (paragraph, label) in self.history.iter().zip(labels) {
            match blocks.last_mut() {
                Some(last)
                    if last.label == label && paragraph.at.saturating_sub(last.ended) <= 1 =>
                {
                    last.text.push(' ');
                    last.text.push_str(&paragraph.text);
                    last.ended = paragraph.ended;
                }
                _ => blocks.push(Block {
                    at: paragraph.at,
                    ended: paragraph.ended,
                    label,
                    text: paragraph.text.clone(),
                }),
            }
        }

        let existing = File::open(&self.path)?;
        // A temp file opens private to its owner. The transcript was not, and
        // a rewrite is not the moment to change who can read a session.
        let permissions = existing.metadata()?.permissions();
        let mut preamble = Vec::new();
        existing.take(self.preamble).read_to_end(&mut preamble)?;
        let parent = self.path.parent().unwrap_or(Path::new("."));
        let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
        replacement.as_file().set_permissions(permissions)?;
        replacement.write_all(&preamble)?;
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                replacement.write_all(b"\n")?;
            }
            replacement
                .write_all(render(block.at, block.label.as_ref(), &block.text).as_bytes())?;
        }
        replacement.flush()?;
        replacement
            .persist(&self.path)
            .map_err(|error| error.error)?;
        // persist() unlinked the inode this handle held; without reopening,
        // every later paragraph writes into a file no path reaches.
        self.file = File::options().append(true).open(&self.path)?;
        Ok(())
    }

    /// [`Self::rewrite`] under the labels `relabel` gives each paragraph's
    /// embedding.
    pub(crate) fn relabel(
        &mut self,
        relabel: impl FnOnce(&[Option<Vec<f32>>]) -> Vec<Option<Label>>,
    ) -> io::Result<()> {
        self.flush_paragraph()?;
        let embeddings: Vec<Option<Vec<f32>>> = self
            .history
            .iter()
            .map(|paragraph| paragraph.embedding.clone())
            .collect();
        self.rewrite(|_| relabel(&embeddings))
    }

    /// Adds a segment and the embedding of its audio, which
    /// [`Self::relabel`] reads.
    pub(crate) fn voiced_segment(
        &mut self,
        utterance: Utterance<'_>,
        embedding: Option<&[f32]>,
    ) -> io::Result<()> {
        self.segment(utterance)?;
        if let Some(embedding) = embedding {
            if self.paragraph_embedding.is_empty() {
                self.paragraph_embedding.resize(embedding.len(), 0.0);
            }
            for (sum, value) in self.paragraph_embedding.iter_mut().zip(embedding) {
                *sum += value;
            }
        }
        Ok(())
    }

    fn flush_paragraph(&mut self) -> io::Result<()> {
        if self.paragraph.is_empty() {
            return Ok(());
        }
        if self.separate {
            self.file.write_all(b"\n")?;
        }
        self.file.write_all(
            render(self.paragraph_at, self.label.as_ref(), &self.paragraph).as_bytes(),
        )?;
        self.file.flush()?;
        self.label = None;
        self.history.push(Paragraph {
            at: self.paragraph_at,
            ended: self.paragraph_ended,
            span: self.paragraph_span.take(),
            embedding: crate::speaker::normalized(std::mem::take(&mut self.paragraph_embedding)),
            text: std::mem::take(&mut self.paragraph),
        });
        self.separate = true;
        Ok(())
    }
}

/// The transcript file is the delivery, not a record of one: a paragraph is
/// on disk the moment it closes. So `completed` has nothing left to write —
/// the whole transcript it would hand over is already there.
impl TranscriptSink for TranscriptFile {
    type Error = std::io::Error;

    fn segment(&mut self, utterance: Utterance<'_>) -> io::Result<()> {
        let Utterance {
            text,
            speaker: label,
            at: heard_at,
            span,
            defers_ending: _,
        } = utterance;
        // One clock for the file, the follower and the speaker timeline: the
        // recording's own. A wall clock would drift from the audio the
        // speaker timeline is read on.
        let at = canonical_duration(heard_at).as_secs();
        let now = Instant::now();
        let speaker_changed = !self.paragraph.is_empty() && self.label.as_ref() != label;
        if speaker_changed || breaks_before(now.duration_since(self.last_segment), &self.paragraph)
        {
            self.flush_paragraph()?;
        }
        if self.paragraph.is_empty() {
            self.paragraph_at = at;
            self.paragraph_span = None;
            self.label = label.cloned();
        } else {
            self.paragraph.push(' ');
        }
        self.paragraph.push_str(text.trim());
        // Where the paragraph's audio ends, not where its last segment began.
        // Both the settle rule and the coalescing in `rewrite` measure the gap
        // to the next paragraph from here, and a start reads as a longer pause
        // than there was.
        self.paragraph_ended = match &span {
            Some(span) => canonical_duration(span.end).as_secs(),
            None => at,
        }
        .max(at);
        // Only timed segments widen the span, so a paragraph covers the audio
        // somebody measured and nothing else. One that gets no timed segment
        // keeps `None`, and the track is never asked about it: a guessed span
        // would hand it the wrong audio, and it would answer confidently.
        if let Some(span) = &span {
            self.paragraph_span = Some(match self.paragraph_span.take() {
                Some(open) => open.start..span.end.max(open.end),
                None => span.clone(),
            });
        }
        self.last_segment = now;
        Ok(())
    }

    fn completed(&mut self, _whole: Utterance<'_>) -> io::Result<()> {
        Ok(())
    }
}

/// The tail paragraph reaches the file however the session ends.
impl Drop for TranscriptFile {
    fn drop(&mut self) {
        if let Err(error) = self.flush_paragraph() {
            tracing::warn!(%error, "failed to flush the final transcript paragraph");
        }
    }
}

pub fn stamp(at_seconds: u64) -> String {
    format!(
        "[{:02}:{:02}:{:02}]",
        at_seconds / 3600,
        at_seconds % 3600 / 60,
        at_seconds % 60
    )
}

fn render(at: u64, label: Option<&Label>, text: &str) -> String {
    let stamp = stamp(at);
    match label {
        Some(label) => format!("{stamp} {label}: {text}\n"),
        None => format!("{stamp} {text}\n"),
    }
}

/// Whether the pause before an arriving segment closes the open paragraph.
/// Nothing finalizes during silence, so the wall-clock gap between segment
/// arrivals is the silence signal — no VAD needed.
fn breaks_before(gap: Duration, paragraph: &str) -> bool {
    if paragraph.is_empty() {
        return false;
    }
    if paragraph.len() >= MAX_PARAGRAPH_CHARS || gap >= GAP_BREAK {
        return true;
    }
    gap >= SENTENCE_GAP && ends_sentence(paragraph)
}

fn ends_sentence(paragraph: &str) -> bool {
    matches!(paragraph.trim_end().chars().last(), Some('.' | '!' | '?'))
}

#[cfg(test)]
mod tests {
    fn heard<'a>(speaker: Option<&'a Label>, text: &'a str) -> Utterance<'a> {
        Utterance {
            text,
            speaker,
            at: 0,
            span: None,
            defers_ending: false,
        }
    }

    /// The same utterance, arriving `at` seconds into the recording.
    fn heard_at<'a>(at: u64, speaker: Option<&'a Label>, text: &'a str) -> Utterance<'a> {
        let start = at * u64::from(CANONICAL_SAMPLE_RATE);
        Utterance {
            at: start,
            span: Some(start..start + u64::from(CANONICAL_SAMPLE_RATE)),
            ..heard(speaker, text)
        }
    }

    /// An utterance running from `at` for `seconds`.
    fn spanning<'a>(
        at: u64,
        seconds: u64,
        speaker: Option<&'a Label>,
        text: &'a str,
    ) -> Utterance<'a> {
        let rate = u64::from(CANONICAL_SAMPLE_RATE);
        Utterance {
            at: at * rate,
            span: Some(at * rate..(at + seconds) * rate),
            ..heard(speaker, text)
        }
    }

    use super::*;
    use crate::audio::CANONICAL_SAMPLE_RATE;
    use crate::speaker::SpeakerName;

    fn enrolled(name: &str) -> Label {
        Label::Enrolled(SpeakerName::new(name).expect("a usable name"))
    }

    fn open_in_tempdir() -> (tempfile::TempDir, TranscriptFile, PathBuf) {
        let directory = tempfile::tempdir().expect("test directory is created");
        let (sink, path) =
            TranscriptFile::create(directory.path()).expect("the transcript file is created");
        (directory, sink, path)
    }

    #[test]
    fn a_long_paragraph_rejoins_the_one_that_follows_it_without_a_pause() {
        let (_directory, mut sink, path) = open_in_tempdir();

        // Live labels flapped mid-utterance, which split one turn in
        // two. The rewrite agrees they are one voice.
        sink.segment(spanning(
            10,
            4,
            Some(&Label::Anonymous(1)),
            "the first half",
        ))
        .expect("the write succeeds");
        sink.segment(spanning(
            14,
            2,
            Some(&Label::Anonymous(2)),
            "and the second",
        ))
        .expect("the write succeeds");
        sink.rewrite(|_| Vec::from([Some(Label::Anonymous(1)), Some(Label::Anonymous(1))]))
            .expect("the rewrite succeeds");
        drop(sink);

        let written = std::fs::read_to_string(path).expect("the transcript file is readable");
        assert_eq!(
            written.trim(),
            "[00:00:10] Speaker 1: the first half and the second",
            "measuring the gap from the paragraph's start leaves these apart"
        );
    }

    #[test]
    fn quick_segments_merge_into_one_timestamped_paragraph() {
        let (_directory, mut sink, path) = open_in_tempdir();

        sink.segment(heard(None, "hello there"))
            .expect("the write succeeds");
        sink.segment(heard(None, "general kenobi"))
            .expect("the write succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(written, "[00:00:00] hello there general kenobi\n");
        assert!(
            path.file_name()
                .and_then(|name| name.to_str())
                .expect("the file has a name")
                .ends_with("Z.txt")
        );
    }

    #[test]
    fn a_speaker_change_breaks_the_paragraph_and_labels_both() {
        let (_directory, mut sink, path) = open_in_tempdir();

        sink.segment(heard(Some(&Label::Anonymous(1)), "hello"))
            .expect("the write succeeds");
        sink.segment(heard(Some(&Label::Anonymous(2)), "hi back"))
            .expect("the write succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(
            written,
            "[00:00:00] Speaker 1: hello\n\n[00:00:00] Speaker 2: hi back\n"
        );
    }

    #[test]
    fn a_paragraph_is_stamped_by_the_recording_not_the_wall() {
        // The follower reads these same seconds off the event stream, so a
        // clock of its own here is a clock the two disagree on.
        let (_directory, mut sink, path) = open_in_tempdir();

        sink.segment(heard_at(0, Some(&Label::Anonymous(1)), "hello"))
            .expect("the write succeeds");
        sink.segment(heard_at(3_671, Some(&Label::Anonymous(2)), "much later"))
            .expect("the write succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(
            written,
            "[00:00:00] Speaker 1: hello\n\n[01:01:11] Speaker 2: much later\n"
        );
    }

    /// An utterance from an engine that times nothing: the decoder still
    /// knows where it had reached, and only that reaches the stamp.
    fn untimed<'a>(at: u64, speaker: Option<&'a Label>, text: &'a str) -> Utterance<'a> {
        Utterance {
            at: at * u64::from(CANONICAL_SAMPLE_RATE),
            span: None,
            ..heard(speaker, text)
        }
    }

    #[test]
    fn an_engine_that_times_no_words_still_stamps_by_the_recording() {
        // Moonshine is a listen backend that times no words.
        let (_directory, mut sink, path) = open_in_tempdir();

        sink.segment(untimed(0, Some(&Label::Anonymous(1)), "hello"))
            .expect("the write succeeds");
        sink.segment(untimed(3_671, Some(&Label::Anonymous(2)), "much later"))
            .expect("the write succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(
            written,
            "[00:00:00] Speaker 1: hello\n\n[01:01:11] Speaker 2: much later\n"
        );
    }

    #[test]
    fn a_paragraph_nobody_timed_is_not_offered_to_the_voice_track() {
        let (_directory, mut sink, _path) = open_in_tempdir();

        // Two speakers, so they are two paragraphs: the gap rule reads the
        // wall clock, and nothing elapses inside a test.
        sink.segment(untimed(0, Some(&Label::Anonymous(1)), "nobody timed this"))
            .expect("the write succeeds");
        sink.segment(spanning(
            30,
            2,
            Some(&Label::Anonymous(2)),
            "this one is timed",
        ))
        .expect("the write succeeds");
        sink.rewrite(|spans| {
            assert_eq!(spans.len(), 2);
            assert_eq!(spans[0], None, "an untimed paragraph points at no audio");
            let rate = u64::from(CANONICAL_SAMPLE_RATE);
            assert_eq!(spans[1], Some(30 * rate..32 * rate));
            Vec::from([None, None])
        })
        .expect("the rewrite succeeds");
    }

    #[test]
    fn a_paragraph_takes_its_span_from_the_first_segment_anybody_timed() {
        let (_directory, mut sink, _path) = open_in_tempdir();
        let rate = u64::from(CANONICAL_SAMPLE_RATE);

        // One paragraph: no pause between them, and no speaker change.
        sink.segment(untimed(10, Some(&Label::Anonymous(1)), "untimed opening"))
            .expect("the write succeeds");
        sink.segment(spanning(10, 2, Some(&Label::Anonymous(1)), "timed rest"))
            .expect("the write succeeds");
        sink.rewrite(|spans| {
            assert_eq!(
                spans,
                [Some(10 * rate..12 * rate)],
                "the span covers the audio somebody measured, not the whole paragraph"
            );
            Vec::from([None])
        })
        .expect("the rewrite succeeds");
    }

    #[test]
    fn a_paragraph_the_track_did_not_reach_stays_unnamed() {
        let (_directory, mut sink, path) = open_in_tempdir();

        sink.segment(spanning(0, 2, Some(&Label::Anonymous(1)), "reached"))
            .expect("the write succeeds");
        sink.segment(spanning(2, 2, Some(&Label::Anonymous(2)), "past the track"))
            .expect("the write succeeds");
        sink.rewrite(|_| Vec::from([Some(Label::Anonymous(1))]))
            .expect("the rewrite succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(
            written,
            "[00:00:00] Speaker 1: reached\n\n[00:00:02] past the track\n"
        );
    }

    #[test]
    fn a_rewrite_mid_session_leaves_later_paragraphs_writable() {
        // The rewrite replaces the file by rename, which unlinks the inode
        // the sink was holding. A session that corrects itself while it runs
        // keeps writing afterwards, so the handle has to follow the path.
        let (_directory, mut sink, path) = open_in_tempdir();

        sink.segment(heard(Some(&Label::Anonymous(2)), "hello"))
            .expect("the write succeeds");
        sink.rewrite(|_| Vec::from([Some(Label::Anonymous(1))]))
            .expect("the rewrite succeeds");
        sink.segment(heard(Some(&Label::Anonymous(1)), "still here"))
            .expect("the write succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(
            written,
            "[00:00:00] Speaker 1: hello\n\n[00:00:00] Speaker 1: still here\n"
        );
    }

    #[test]
    fn a_rewrite_relabels_but_preserves_the_preamble() {
        let directory = tempfile::tempdir().expect("test directory is created");
        let path = directory.path().join("transcript.txt");
        fs::write(&path, "earlier session\n").expect("the preamble is written");
        let (mut sink, path) =
            TranscriptFile::create_at(path).expect("the transcript file is created");

        sink.segment(heard(Some(&Label::Anonymous(1)), "hello"))
            .expect("the write succeeds");
        sink.segment(heard(Some(&Label::Anonymous(2)), "hi back"))
            .expect("the write succeeds");
        sink.rewrite(|spans| {
            assert_eq!(spans.len(), 2);
            vec![Some(enrolled("alex")), Some(Label::Anonymous(1))]
        })
        .expect("the rewrite succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(
            written,
            "earlier session\n[00:00:00] alex: hello\n\n[00:00:00] Speaker 1: hi back\n"
        );
    }

    #[test]
    fn a_relabel_reads_each_paragraphs_voice_from_every_segment_in_it() {
        let (_directory, mut sink, path) = open_in_tempdir();
        let speaker = Label::Anonymous(1);

        sink.voiced_segment(heard_at(0, Some(&speaker), "hello"), Some(&[0.6, 0.8]))
            .expect("the write succeeds");
        sink.voiced_segment(heard_at(1, Some(&speaker), "there"), Some(&[0.6, -0.8]))
            .expect("the write succeeds");
        sink.voiced_segment(
            heard_at(9, Some(&Label::Anonymous(2)), "hi back"),
            Some(&[0.0, 1.0]),
        )
        .expect("the write succeeds");
        sink.relabel(|embeddings| {
            assert_eq!(embeddings, [Some(vec![1.0, 0.0]), Some(vec![0.0, 1.0])]);
            vec![Some(enrolled("alex")), Some(Label::Anonymous(1))]
        })
        .expect("the relabel succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(
            written,
            "[00:00:00] alex: hello there\n\n[00:00:09] Speaker 1: hi back\n"
        );
    }

    #[test]
    fn a_rewrite_folds_an_unnamed_fragment_into_the_utterance_it_opens() {
        let (_directory, mut sink, path) = open_in_tempdir();

        sink.segment(heard(None, "Well I"))
            .expect("the write succeeds");
        sink.segment(heard(Some(&Label::Anonymous(2)), "disagree."))
            .expect("the write succeeds");
        sink.rewrite(|_| vec![None, Some(Label::Anonymous(2))])
            .expect("the rewrite succeeds");
        drop(sink);

        let written = fs::read_to_string(&path).expect("the transcript file is readable");
        assert_eq!(written, "[00:00:00] Speaker 2: Well I disagree.\n");
    }

    #[test]
    fn paragraphs_break_on_pauses_sentence_ends_and_the_size_cap() {
        let cases = [
            ("", Duration::from_secs(60), false),
            ("mid sentence words", Duration::from_secs(1), false),
            ("mid sentence words", GAP_BREAK, true),
            ("a finished sentence.", SENTENCE_GAP, true),
            ("a finished sentence.", Duration::from_millis(200), false),
            ("shouted!", SENTENCE_GAP, true),
        ];
        for (paragraph, gap, expected) in cases {
            assert_eq!(
                breaks_before(gap, paragraph),
                expected,
                "{paragraph:?} after {gap:?}"
            );
        }
        assert!(breaks_before(
            Duration::ZERO,
            &"x".repeat(MAX_PARAGRAPH_CHARS)
        ));
    }
}
