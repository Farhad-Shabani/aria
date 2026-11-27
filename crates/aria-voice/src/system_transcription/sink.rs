//! The transcript file a system transcription writes as it hears.

use std::fs::File;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aria_kit::audio::canonical_duration;
use time::OffsetDateTime;

use crate::dictation::{TranscriptSink, Utterance};
use crate::history::{CreateError, Store};
use crate::speaker::Label;
use crate::transcript_file::{self, Creation};

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
pub struct TranscriptFile {
    file: File,
    path: PathBuf,
    owner: Owner,
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

/// Whose path a session's file sits at, which decides whether a rewrite may
/// replace the file.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// Aria made the file in its own folder, so its path is the session's.
    Aria,
    /// The caller named the file, and any program may append to it while the
    /// session runs. Aria only appends to it and never rewrites it.
    Caller,
}

impl TranscriptFile {
    /// A new file in `directory`, under a name no session had before.
    pub(crate) fn create(directory: &Path) -> io::Result<(Self, PathBuf)> {
        for name in transcript_file::names_at(OffsetDateTime::now_utc()) {
            let path = directory.join(name?);
            match transcript_file::create(&path, Creation::New) {
                Ok(file) => return Ok((Self::opened(file, path.clone(), Owner::Aria), path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(transcript_file::taken())
    }

    /// A new file in `directory` that `history` names from its start, under
    /// a name nothing has on disk or in history. Returns the entry's id.
    pub(crate) fn create_recorded(
        directory: &Path,
        history: &Store,
    ) -> Result<(Self, PathBuf, u64), CreateError> {
        let (file, path, id) = history.create_transcript(directory)?;
        Ok((Self::opened(file, path.clone(), Owner::Aria), path, id))
    }

    /// The caller's exact file, appended to when it already exists. A file or
    /// folder this creates is private to its owner. An existing file keeps
    /// its mode.
    pub(crate) fn append_to(path: PathBuf) -> io::Result<(Self, PathBuf)> {
        let file = transcript_file::create(&path, Creation::Append)?;
        Ok((Self::opened(file, path.clone(), Owner::Caller), path))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn opened(file: File, path: PathBuf, owner: Owner) -> Self {
        let now = Instant::now();
        Self {
            file,
            path,
            owner,
            paragraph: String::new(),
            paragraph_at: 0,
            paragraph_ended: 0,
            paragraph_span: None,
            paragraph_embedding: Vec::new(),
            label: None,
            last_segment: now,
            separate: false,
            history: Vec::new(),
        }
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

    /// Rewrites every paragraph written so far under the labels `label`
    /// gives. Rename-atomic: the streamed file stands until the replacement
    /// is whole. Safe to run again. A session that keeps correcting itself
    /// calls this once per correction. A caller's file keeps the labels
    /// given while listening.
    /// `label` is asked after the open paragraph closes, so the spans it
    /// answers about include it. It may answer for only the first
    /// paragraphs; the rest lose their labels rather than borrow a
    /// neighbour's.
    pub(crate) fn rewrite(
        &mut self,
        label: impl FnOnce(&[Option<Range<u64>>]) -> Vec<Option<Label>>,
    ) -> io::Result<()> {
        self.flush_paragraph()?;
        if self.history.is_empty() || self.owner == Owner::Caller {
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

        let mut contents = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            if index > 0 {
                contents.push(b'\n');
            }
            contents
                .extend_from_slice(render(block.at, block.label.as_ref(), &block.text).as_bytes());
        }
        self.file = transcript_file::rewrite(&self.path, &self.file, &contents)?;
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
            spacing: _,
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
            spacing: Default::default(),
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
    use crate::speaker::SpeakerName;
    use aria_kit::audio::CANONICAL_SAMPLE_RATE;
    use aria_platform::sqlite::AtRest;
    use std::fs;
    use std::sync::Arc;

    fn enrolled(name: &str) -> Label {
        Label::Enrolled(SpeakerName::new(name).expect("a usable name"))
    }

    /// A session history names, with one segment heard and not yet written.
    fn recorded_in_tempdir() -> (tempfile::TempDir, Arc<Store>, TranscriptFile, PathBuf, u64) {
        let directory = tempfile::tempdir().unwrap();
        let history = Store::open(&directory.path().join("history"), &AtRest::for_tests()).unwrap();
        let (mut sink, path, id) =
            TranscriptFile::create_recorded(&directory.path().join("transcripts"), &history)
                .unwrap();
        sink.segment(spanning(0, 2, Some(&Label::Anonymous(1)), "heard"))
            .unwrap();
        (directory, history, sink, path, id)
    }

    #[test]
    fn a_relabeled_transcript_goes_with_its_deleted_entry() {
        let (_directory, history, mut sink, path, id) = recorded_in_tempdir();
        sink.rewrite(|_| vec![Some(Label::Anonymous(2))]).unwrap();
        drop(sink);
        history.finish_transcript(id, 1_000, None).unwrap();

        history.delete(id).unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn a_line_another_writer_appends_after_a_rewrite_stays() {
        let (_directory, _history, mut sink, path, _id) = recorded_in_tempdir();
        sink.segment(heard(None, "first")).unwrap();
        sink.flush_paragraph().unwrap();
        sink.rewrite(|_| vec![Some(Label::Anonymous(2))]).unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\nsomeone else's line")
            .unwrap();

        sink.segment(heard(None, "second")).unwrap();
        sink.flush_paragraph().unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("someone else's line"), "{text}");
        assert!(text.trim_end().ends_with("second"), "{text}");
    }

    #[test]
    fn a_write_only_output_file_opens_and_gets_the_session_after_its_text() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("meeting.txt");
        fs::write(&path, "earlier\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();

        let (mut sink, _) = TranscriptFile::append_to(path.clone()).unwrap();
        sink.segment(heard(Some(&Label::Anonymous(1)), "heard"))
            .unwrap();
        sink.flush_paragraph().unwrap();
        drop(sink);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "earlier\n[00:00:00] Speaker 1: heard\n"
        );
    }

    #[test]
    fn a_note_appended_to_the_output_file_survives_speaker_correction() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("meeting.txt");
        let (mut sink, _) = TranscriptFile::append_to(path.clone()).unwrap();
        sink.segment(heard(Some(&Label::Anonymous(1)), "heard"))
            .unwrap();
        sink.flush_paragraph().unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"\na note\n")
            .unwrap();

        sink.rewrite(|_| vec![Some(Label::Anonymous(2))]).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[00:00:00] Speaker 1: heard\n\na note\n"
        );
    }

    #[test]
    fn a_rewrite_leaves_a_file_an_editor_saved_at_the_output_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("meeting.txt");
        let (mut sink, _) = TranscriptFile::append_to(path.clone()).unwrap();
        sink.segment(heard(Some(&Label::Anonymous(1)), "heard"))
            .unwrap();
        sink.flush_paragraph().unwrap();
        let saved = directory.path().join("meeting.txt.saving");
        fs::write(&saved, "the person's edits\n").unwrap();
        fs::rename(&saved, &path).unwrap();

        sink.rewrite(|_| vec![Some(Label::Anonymous(2))]).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "the person's edits\n");
    }

    fn open_in_tempdir() -> (tempfile::TempDir, TranscriptFile, PathBuf) {
        let directory = tempfile::tempdir().expect("test directory is created");
        let (sink, path) = TranscriptFile::create(&directory.path().join("transcripts"))
            .expect("the transcript file is created");
        (directory, sink, path)
    }

    #[test]
    fn a_planted_link_or_second_name_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        fs::write(&target, "").unwrap();

        let link = directory.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(TranscriptFile::append_to(link).is_err());

        let second = directory.path().join("second.txt");
        fs::hard_link(&target, &second).unwrap();
        assert!(TranscriptFile::append_to(second).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "");

        let fifo = directory.path().join("fifo.txt");
        let mut mkfifo = std::process::Command::new("mkfifo");
        mkfifo.arg(&fifo);
        let outcome = aria_primitives::process::ChildProcess::spawn(&mut mkfifo)
            .unwrap()
            .wait_until(Instant::now() + Duration::from_secs(5), || false)
            .unwrap();
        assert!(matches!(
            outcome,
            aria_primitives::process::WaitOutcome::Exited(status) if status.success()
        ));
        assert!(TranscriptFile::append_to(fifo).is_err());
    }

    #[test]
    fn sessions_started_in_the_same_second_get_files_of_their_own() {
        let directory = tempfile::tempdir().unwrap();
        let (_first, first) = TranscriptFile::create(directory.path()).unwrap();
        let (_second, second) = TranscriptFile::create(directory.path()).unwrap();

        assert_ne!(first, second);
    }

    #[test]
    fn a_new_transcript_and_its_folder_are_private_and_an_existing_file_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("transcripts");
        let (_sink, path) = TranscriptFile::create(&folder).unwrap();
        assert_eq!(mode(&folder), 0o700);
        assert_eq!(mode(&path), 0o600);

        let shared = directory.path().join("shared.txt");
        fs::write(&shared, "").unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o644)).unwrap();
        drop(TranscriptFile::append_to(shared.clone()).unwrap());
        assert_eq!(mode(&shared), 0o644);
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
        let name = path.file_name().and_then(|name| name.to_str());
        assert!(name.and_then(transcript_file::session_start).is_some());
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

        sink.segment(spanning(0, 1, Some(&Label::Anonymous(1)), "hello"))
            .expect("the write succeeds");
        sink.segment(spanning(3_671, 1, Some(&Label::Anonymous(2)), "much later"))
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
        // Moonshine is a recognition backend that times no words.
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
    fn a_relabel_reads_each_paragraphs_voice_from_every_segment_in_it() {
        let (_directory, mut sink, path) = open_in_tempdir();
        let speaker = Label::Anonymous(1);

        sink.voiced_segment(spanning(0, 1, Some(&speaker), "hello"), Some(&[0.6, 0.8]))
            .expect("the write succeeds");
        sink.voiced_segment(spanning(1, 1, Some(&speaker), "there"), Some(&[0.6, -0.8]))
            .expect("the write succeeds");
        sink.voiced_segment(
            spanning(9, 1, Some(&Label::Anonymous(2)), "hi back"),
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
