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
const ARCHIVE_URL: &str = "https://www.openslr.org/resources/12/test-clean.tar.gz";
const SPEAKERS: usize = 40;
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
            download(&speech)?;
            downloaded = true;
        }
        let recording = recording_for(&speech, id);
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

/// Extracts into a staging directory that moves into place only once tar
/// succeeds, so a cut-short extraction never passes for a complete one.
fn download(speech: &Path) -> Result<(), FetchError> {
    let extracted = speech.join("LibriSpeech");
    if speakers_in(&extracted.join("test-clean")) >= SPEAKERS {
        return Ok(());
    }
    require("tar", "--version")?;
    require("ffmpeg", "-version")?;
    let staging = speech.join("extracting");
    remove_dir(&extracted)?;
    remove_dir(&staging)?;
    create_dir(&staging)?;
    let archive = speech.join("test-clean.tar.gz.part");
    fetch::download(ARCHIVE_URL, &archive)?;
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(&archive).arg("-C").arg(&staging);
    run_tool("tar", &mut tar, EXTRACT_TIMEOUT)?;
    fs::rename(staging.join("LibriSpeech"), &extracted).map_err(io_error(&extracted))?;
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
fn recording_for(speech: &Path, id: &str) -> PathBuf {
    let mut parts = id.splitn(3, '-');
    let speaker = parts.next().unwrap_or_default();
    let chapter = parts.next().unwrap_or_default();
    speech
        .join("LibriSpeech/test-clean")
        .join(speaker)
        .join(chapter)
        .join(format!("{id}.flac"))
}

/// Writes the one WAV format capture accepts. Bitexact output omits the
/// encoder version, so every ffmpeg writes the same bytes.
fn decode(recording: &Path, wav: &Path) -> Result<(), FetchError> {
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
        let speakers: BTreeSet<_> = rows[..SPEAKERS]
            .iter()
            .map(|row| row.id.split('-').next().unwrap())
            .collect();

        assert_eq!(rows.len(), 100);
        assert_eq!(speakers.len(), SPEAKERS);
    }

    #[test]
    fn an_id_locates_its_recording_under_speaker_and_chapter() {
        assert_eq!(
            recording_for(Path::new("/speech"), "1089-134686-0000"),
            Path::new("/speech/LibriSpeech/test-clean/1089/134686/1089-134686-0000.flac")
        );
    }
}
