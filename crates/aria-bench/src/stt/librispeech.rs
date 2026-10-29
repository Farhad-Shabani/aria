//! The LibriSpeech test-clean corpus the accuracy recipes measure, as 16 kHz
//! mono 16-bit WAVs plus a manifest of what each one says.
//!
//! LibriSpeech is clean, read audiobook prose under CC BY 4.0. It shows
//! whether a backend hears the words, not how it handles dictation.
//!
//! The committed list pins each WAV's sha256, so every machine measures the
//! same audio. Its rows interleave the 40 speakers, so a short corpus still
//! mixes them.

use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::corpus::{Built, CorpusError, write_manifest};
use super::fetch::{self, FetchError, create_dir, ensure, io_error, parse, require, run_tool};

const LIST: &str = include_str!("../../corpus/librispeech-test-clean.tsv");
const ARCHIVES_URL: &str = "https://www.openslr.org/resources/12";
const SAMPLE_RATE: &str = "16000";
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DECODE_TIMEOUT: Duration = Duration::from_secs(60);

/// Builds the first `wanted` listed utterances. A matching WAV on disk is
/// kept, so a cached corpus needs neither the archive nor ffmpeg.
pub fn build(output: &Path, wanted: NonZeroUsize) -> Result<Built, LibrispeechError> {
    // The manifest names each WAV by absolute path.
    let output = &std::path::absolute(output).map_err(io_error(output))?;
    let speech = output.join("librispeech");
    let mut downloaded = false;
    build_from(LIST, output, wanted, |id, wav| {
        if !downloaded {
            download(&speech, Subset::TestClean)?;
            downloaded = true;
        }
        let recording = recording_for(&speech, Subset::TestClean, id);
        if !recording.is_file() {
            return Err(LibrispeechError::MissingRecording(recording));
        }
        Ok(decode(&recording, wav)?)
    })
}

/// `decode` writes the WAV for an utterance id to the given path.
fn build_from(
    list: &str,
    output: &Path,
    wanted: NonZeroUsize,
    mut decode: impl FnMut(&str, &Path) -> Result<(), LibrispeechError>,
) -> Result<Built, LibrispeechError> {
    let rows = parse(list)?;
    let wanted = wanted.get();
    if wanted > rows.len() {
        return Err(LibrispeechError::TooFew {
            wanted,
            listed: rows.len(),
        });
    }
    let audio = output.join("wav");
    create_dir(&audio)?;

    let mut utterances = Vec::with_capacity(wanted);
    for row in &rows[..wanted] {
        let wav = audio.join(format!("{}.wav", row.id));
        ensure(row, &wav, |wav| decode(row.id, wav))?;
        utterances.push((wav, row.text));
    }
    Ok(write_manifest(
        &output.join("manifest.tsv"),
        utterances
            .iter()
            .map(|(wav, reference)| (wav.as_path(), *reference)),
    )?)
}

/// A LibriSpeech subset of clean read speech, CC BY 4.0. Each holds its own
/// speakers, so one can fit what another is measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Subset {
    DevClean,
    DevOther,
    TestClean,
}

impl Subset {
    /// How many speakers the subset holds.
    fn readers(self) -> usize {
        match self {
            Self::DevClean | Self::TestClean => 40,
            Self::DevOther => 33,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::DevClean => "dev-clean",
            Self::DevOther => "dev-other",
            Self::TestClean => "test-clean",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Gender {
    Female,
    Male,
}

/// One reader and what they read, in id order.
#[derive(Debug)]
pub(crate) struct Speaker {
    pub id: u32,
    pub gender: Gender,
    pub chapters: Vec<Chapter>,
}

#[derive(Debug)]
pub(crate) struct Chapter {
    /// Each utterance id and its transcript.
    pub utterances: Vec<(String, String)>,
}

/// Every speaker of `subset` in the extracted archive under `speech`, by id.
pub(crate) fn speakers(speech: &Path, subset: Subset) -> Result<Vec<Speaker>, LibrispeechError> {
    let root = speech.join("LibriSpeech");
    let listed = root.join("SPEAKERS.TXT");
    let genders = genders(
        &fs::read_to_string(&listed).map_err(io_error(&listed))?,
        subset,
    )?;
    let mut speakers = Vec::new();
    for (id, gender) in genders {
        let directory = root.join(subset.name()).join(id.to_string());
        let mut chapters = Vec::new();
        for chapter in numbered_dirs(&directory)? {
            let transcripts = directory
                .join(chapter.to_string())
                .join(format!("{id}-{chapter}.trans.txt"));
            let text = fs::read_to_string(&transcripts).map_err(io_error(&transcripts))?;
            chapters.push(Chapter {
                utterances: transcript(&text),
            });
        }
        speakers.push(Speaker {
            id,
            gender,
            chapters,
        });
    }
    Ok(speakers)
}

/// The readers of `subset` in `SPEAKERS.TXT`, by id.
fn genders(listed: &str, subset: Subset) -> Result<Vec<(u32, Gender)>, LibrispeechError> {
    let mut genders = Vec::new();
    for line in listed.lines().filter(|line| !line.starts_with(';')) {
        let fields: Vec<&str> = line.split('|').map(str::trim).collect();
        if fields.get(2) != Some(&subset.name()) {
            continue;
        }
        let unreadable = || LibrispeechError::Speakers(line.to_owned());
        let id = fields[0].parse().map_err(|_| unreadable())?;
        let gender = match fields[1] {
            "F" => Gender::Female,
            "M" => Gender::Male,
            _ => return Err(unreadable()),
        };
        genders.push((id, gender));
    }
    genders.sort_by_key(|(id, _)| *id);
    Ok(genders)
}

fn transcript(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(id, words)| (id.to_owned(), words.to_owned()))
        .collect()
}

fn numbered_dirs(directory: &Path) -> Result<Vec<u32>, FetchError> {
    let mut numbers: Vec<u32> = fs::read_dir(directory)
        .map_err(io_error(directory))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    numbers.sort_unstable();
    Ok(numbers)
}

/// Extracts into a staging directory that moves into place only once tar
/// succeeds, so a cut-short extraction never passes for a complete one.
/// One cache holds every subset; extracting replaces only `subset`.
pub(crate) fn download(speech: &Path, subset: Subset) -> Result<(), FetchError> {
    let extracted = speech.join("LibriSpeech");
    let readers = extracted.join(subset.name());
    if speakers_in(&readers) >= subset.readers() {
        return Ok(());
    }
    require("tar", "--version")?;
    require("ffmpeg", "-version")?;
    let staging = speech.join("extracting");
    remove_dir(&readers)?;
    remove_dir(&staging)?;
    create_dir(&staging)?;
    let archive = speech.join(format!("{}.tar.gz.part", subset.name()));
    fetch::download(
        &format!("{ARCHIVES_URL}/{}.tar.gz", subset.name()),
        &archive,
    )?;
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(&archive).arg("-C").arg(&staging);
    run_tool("tar", &mut tar, EXTRACT_TIMEOUT)?;
    create_dir(&extracted)?;
    let unpacked = staging.join("LibriSpeech");
    fs::rename(unpacked.join(subset.name()), &readers).map_err(io_error(&readers))?;
    // Every archive carries the list of every subset's speakers.
    let listed = extracted.join("SPEAKERS.TXT");
    fs::rename(unpacked.join("SPEAKERS.TXT"), &listed).map_err(io_error(&listed))?;
    remove_dir(&staging)?;
    fs::remove_file(&archive).map_err(io_error(&archive))
}

fn remove_dir(path: &Path) -> Result<(), FetchError> {
    match fs::remove_dir_all(path) {
        Err(source) if source.kind() != io::ErrorKind::NotFound => Err(io_error(path)(source)),
        _ => Ok(()),
    }
}

fn speakers_in(directory: &Path) -> usize {
    fs::read_dir(directory).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .count()
    })
}

/// LibriSpeech names a recording `<speaker>-<chapter>-<index>` and files it
/// under `<speaker>/<chapter>/`, so the id alone locates it.
pub(crate) fn recording_for(speech: &Path, subset: Subset, id: &str) -> PathBuf {
    let mut parts = id.splitn(3, '-');
    let speaker = parts.next().unwrap_or_default();
    let chapter = parts.next().unwrap_or_default();
    speech
        .join("LibriSpeech")
        .join(subset.name())
        .join(speaker)
        .join(chapter)
        .join(format!("{id}.flac"))
}

/// Writes the one WAV format capture accepts. Bitexact output omits the
/// encoder version, so every ffmpeg writes the same bytes.
pub(crate) fn decode(recording: &Path, wav: &Path) -> Result<(), FetchError> {
    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(recording)
        .args(["-fflags", "+bitexact", "-map_metadata", "-1"])
        .args(["-ac", "1", "-ar", SAMPLE_RATE, "-c:a", "pcm_s16le"])
        .arg(wav);
    run_tool("ffmpeg", &mut ffmpeg, DECODE_TIMEOUT)
}

#[derive(Debug, thiserror::Error)]
pub enum LibrispeechError {
    #[error("asked for {wanted} utterances; the list holds only {listed}")]
    TooFew { wanted: usize, listed: usize },

    #[error("the archive is missing {}", .0.display())]
    MissingRecording(PathBuf),

    #[error("SPEAKERS.TXT has an unreadable row: {0}")]
    Speakers(String),

    #[error(transparent)]
    Fetch(#[from] FetchError),

    #[error(transparent)]
    Corpus(#[from] CorpusError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::super::fetch::tests::sha256;
    use super::*;

    const PINNED: &[u8] = b"pinned audio";

    fn list() -> String {
        let sha256 = sha256(PINNED);
        format!(
            "1089-134686-0000\t{sha256}\tHE HOPED THERE WOULD BE STEW\n\
             1188-133604-0000\t{sha256}\tYOU WILL FIND ME\n"
        )
    }

    fn count(utterances: usize) -> NonZeroUsize {
        NonZeroUsize::new(utterances).unwrap()
    }

    fn writing(bytes: &'static [u8]) -> impl FnMut(&str, &Path) -> Result<(), LibrispeechError> {
        move |_, wav| {
            fs::write(wav, bytes).unwrap();
            Ok(())
        }
    }

    fn never(id: &str, _: &Path) -> Result<(), LibrispeechError> {
        panic!("{id} should not be decoded");
    }

    #[test]
    fn a_decode_that_matches_the_list_builds_the_manifest() {
        let output = tempfile::tempdir().unwrap();

        let built = build_from(&list(), output.path(), count(2), writing(PINNED)).unwrap();

        assert_eq!((built.utterances, built.words), (2, 10));
        let wav = output.path().join("wav");
        assert_eq!(
            fs::read_to_string(&built.manifest).unwrap(),
            format!(
                "{}\tHE HOPED THERE WOULD BE STEW\n{}\tYOU WILL FIND ME\n",
                wav.join("1089-134686-0000.wav").display(),
                wav.join("1188-133604-0000.wav").display()
            )
        );
    }

    #[test]
    fn a_matching_wav_is_kept_without_decoding() {
        let output = tempfile::tempdir().unwrap();
        build_from(&list(), output.path(), count(2), writing(PINNED)).unwrap();

        let built = build_from(&list(), output.path(), count(1), never).unwrap();

        assert_eq!(
            fs::read_to_string(&built.manifest).unwrap().lines().count(),
            1
        );
    }

    #[test]
    fn a_count_the_list_cannot_satisfy_fails_before_any_decode() {
        let output = tempfile::tempdir().unwrap();

        let built = build_from(&list(), output.path(), count(3), never);

        assert!(matches!(
            built,
            Err(LibrispeechError::TooFew {
                wanted: 3,
                listed: 2
            })
        ));
    }

    #[test]
    fn the_committed_list_spans_every_speaker_within_its_first_rows() {
        let rows = parse(LIST).unwrap();
        let readers = Subset::TestClean.readers();
        let speakers: BTreeSet<_> = rows[..readers]
            .iter()
            .map(|row| row.id.split('-').next().unwrap())
            .collect();

        assert_eq!(rows.len(), 100);
        assert_eq!(speakers.len(), readers);
    }

    #[test]
    fn only_test_clean_readers_are_listed_by_id() {
        let listed = ";ID  |SEX| SUBSET           |MINUTES| NAME\n\
                      237  | F | test-clean       | 8.02  | rachelellen\n\
                      14   | F | train-clean-360  | 25.03 | Kristin LeMoine\n\
                      61   | M | test-clean       | 8.08  | Paul-Gabriel Wiener\n";

        assert_eq!(
            genders(listed, Subset::TestClean).unwrap(),
            [(61, Gender::Male), (237, Gender::Female)]
        );
    }

    #[test]
    fn an_id_locates_its_recording_under_speaker_and_chapter() {
        assert_eq!(
            recording_for(Path::new("/speech"), Subset::TestClean, "1089-134686-0000"),
            Path::new("/speech/LibriSpeech/test-clean/1089/134686/1089-134686-0000.flac")
        );
    }
}
