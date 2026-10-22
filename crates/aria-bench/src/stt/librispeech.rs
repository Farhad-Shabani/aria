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
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aria::primitives::cancel::CancelToken;
use aria::primitives::process::{Bounds, Capture, RunError, run};
use aria_harness::artifact::{ArtifactError, fingerprint};
use signal_hook::consts::SIGINT;

const LIST: &str = include_str!("../../corpus/librispeech-test-clean.tsv");
const ARCHIVE_URL: &str = "https://www.openslr.org/resources/12/test-clean.tar.gz";
const SPEAKERS: usize = 40;
const SAMPLE_RATE: &str = "16000";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DECODE_TIMEOUT: Duration = Duration::from_secs(60);
const DIAGNOSTIC_BYTES: usize = 64 * 1024;

pub struct Built {
    pub utterances: usize,
    pub words: usize,
    pub manifest: PathBuf,
}

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
        decode(&recording, wav)
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

    let mut manifest = String::new();
    let mut words = 0;
    for row in &rows[..wanted] {
        let wav = audio.join(format!("{}.wav", row.id));
        if !pinned(&wav, row.sha256)? {
            decode(row.id, &wav)?;
            if !pinned(&wav, row.sha256)? {
                return Err(LibrispeechError::Unpinned(row.id.to_owned()));
            }
        }
        manifest.push_str(&format!("{}\t{}\n", wav.display(), row.reference));
        words += row.reference.split_whitespace().count();
    }

    let path = output.join("manifest.tsv");
    fs::write(&path, manifest).map_err(io_error(&path))?;
    Ok(Built {
        utterances: wanted,
        words,
        manifest: path,
    })
}

struct Row<'a> {
    id: &'a str,
    sha256: &'a str,
    reference: &'a str,
}

fn parse(list: &str) -> Result<Vec<Row<'_>>, LibrispeechError> {
    list.lines()
        .enumerate()
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            match (fields.next(), fields.next(), fields.next(), fields.next()) {
                (Some(id), Some(sha256), Some(reference), None) if !reference.is_empty() => {
                    Ok(Row {
                        id,
                        sha256,
                        reference,
                    })
                }
                _ => Err(LibrispeechError::MalformedList { line: index + 1 }),
            }
        })
        .collect()
}

fn pinned(wav: &Path, sha256: &str) -> Result<bool, LibrispeechError> {
    if !wav.is_file() {
        return Ok(false);
    }
    Ok(fingerprint(wav)? == sha256)
}

/// Extracts into a staging directory that moves into place only once tar
/// succeeds, so a cut-short extraction never passes for a complete one.
fn download(speech: &Path) -> Result<(), LibrispeechError> {
    let extracted = speech.join("LibriSpeech");
    if speakers_in(&extracted.join("test-clean")) >= SPEAKERS {
        return Ok(());
    }
    let staging = speech.join("extracting");
    remove_dir(&extracted)?;
    remove_dir(&staging)?;
    create_dir(&staging)?;
    let archive = speech.join("test-clean.tar.gz.part");
    tracing::info!(
        url = ARCHIVE_URL,
        "downloading LibriSpeech test-clean, 346 MB"
    );
    let mut curl = Command::new("curl");
    curl.args(["--silent", "--show-error", "--fail", "--location"])
        .args(["--retry", "3", "--output"])
        .arg(&archive)
        .arg(ARCHIVE_URL);
    run_tool("curl", &mut curl, DOWNLOAD_TIMEOUT)?;
    let mut tar = Command::new("tar");
    tar.arg("-xzf").arg(&archive).arg("-C").arg(&staging);
    run_tool("tar", &mut tar, EXTRACT_TIMEOUT)?;
    fs::rename(staging.join("LibriSpeech"), &extracted).map_err(io_error(&extracted))?;
    remove_dir(&staging)?;
    fs::remove_file(&archive).map_err(io_error(&archive))
}

fn remove_dir(path: &Path) -> Result<(), LibrispeechError> {
    match fs::remove_dir_all(path) {
        Err(source) if source.kind() != io::ErrorKind::NotFound => Err(io_error(path)(source)),
        _ => Ok(()),
    }
}

fn create_dir(path: &Path) -> Result<(), LibrispeechError> {
    fs::create_dir_all(path).map_err(io_error(path))
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> LibrispeechError + '_ {
    move |source| LibrispeechError::Io {
        path: path.to_path_buf(),
        source,
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
fn decode(recording: &Path, wav: &Path) -> Result<(), LibrispeechError> {
    let mut ffmpeg = Command::new("ffmpeg");
    ffmpeg
        .args(["-nostdin", "-loglevel", "error", "-y", "-i"])
        .arg(recording)
        .args(["-fflags", "+bitexact", "-map_metadata", "-1"])
        .args(["-ac", "1", "-ar", SAMPLE_RATE, "-c:a", "pcm_s16le"])
        .arg(wav);
    run_tool("ffmpeg", &mut ffmpeg, DECODE_TIMEOUT)
}

fn run_tool(
    tool: &'static str,
    command: &mut Command,
    timeout: Duration,
) -> Result<(), LibrispeechError> {
    let bounds = Bounds {
        timeout,
        stdout: Capture::KeepTail(DIAGNOSTIC_BYTES),
        stderr: Capture::KeepTail(DIAGNOSTIC_BYTES),
    };
    let interrupt = Interrupt::arm()?;
    let Output { status, stderr, .. } = match run(command, &[], bounds, &interrupt.token()) {
        Ok(output) => output,
        Err(RunError::Cancelled) => return Err(LibrispeechError::Interrupted),
        Err(RunError::Spawn(source)) if source.kind() == io::ErrorKind::NotFound => {
            return Err(LibrispeechError::Missing(tool));
        }
        Err(source) => return Err(LibrispeechError::Run { tool, source }),
    };
    drop(interrupt);
    if status.success() {
        return Ok(());
    }
    Err(LibrispeechError::Failed {
        tool,
        stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
    })
}

/// Ctrl-C stops the running tool too. Each tool runs in its own process
/// group, which the terminal's interrupt never reaches. signal-hook cannot
/// restore SIGINT's default action, so `terminate` picks which handler acts.
/// Outside a tool run, Ctrl-C still ends the program.
struct Interrupt {
    flags: &'static Flags,
}

struct Flags {
    /// True whenever no tool is running, so the default action applies.
    terminate: Arc<AtomicBool>,
    raised: Arc<AtomicBool>,
}

static FLAGS: OnceLock<Flags> = OnceLock::new();

impl Interrupt {
    fn arm() -> Result<Self, LibrispeechError> {
        let flags = match FLAGS.get() {
            Some(flags) => flags,
            None => {
                let flags = Flags {
                    terminate: Arc::new(AtomicBool::new(true)),
                    raised: Arc::new(AtomicBool::new(false)),
                };
                // Registered first so it acts first: a raised `terminate`
                // ends the process before `raised` is read.
                signal_hook::flag::register_conditional_default(
                    SIGINT,
                    Arc::clone(&flags.terminate),
                )
                .map_err(LibrispeechError::Signal)?;
                signal_hook::flag::register(SIGINT, Arc::clone(&flags.raised))
                    .map_err(LibrispeechError::Signal)?;
                FLAGS.get_or_init(|| flags)
            }
        };
        flags.raised.store(false, Ordering::SeqCst);
        flags.terminate.store(false, Ordering::SeqCst);
        Ok(Self { flags })
    }

    fn token(&self) -> CancelToken {
        let raised = Arc::clone(&self.flags.raised);
        CancelToken::from_fn(move || raised.load(Ordering::SeqCst))
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.flags.terminate.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LibrispeechError {
    #[error("line {line} of the corpus list needs an id, a sha256, and a reference")]
    MalformedList { line: usize },

    #[error("asked for {wanted} utterances; the list holds only {listed}")]
    TooFew { wanted: usize, listed: usize },

    #[error("needs {0}; install it with `brew install {0}`")]
    Missing(&'static str),

    #[error("{tool} did not finish")]
    Run {
        tool: &'static str,
        #[source]
        source: RunError,
    },

    #[error("{tool} failed: {stderr}")]
    Failed { tool: &'static str, stderr: String },

    #[error("the archive is missing {}", .0.display())]
    MissingRecording(PathBuf),

    #[error("{0} decoded to audio the list does not pin")]
    Unpinned(String),

    #[error("interrupted")]
    Interrupted,

    #[error("failed to handle Ctrl-C")]
    Signal(#[source] io::Error),

    #[error("failed to update {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    Hash(#[from] ArtifactError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const PINNED: &[u8] = b"pinned audio";

    fn list() -> String {
        let pinned = tempfile::NamedTempFile::new().unwrap();
        fs::write(pinned.path(), PINNED).unwrap();
        let sha256 = fingerprint(pinned.path()).unwrap();
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
    fn a_wav_that_differs_is_decoded_again() {
        let output = tempfile::tempdir().unwrap();
        build_from(&list(), output.path(), count(2), writing(PINNED)).unwrap();
        let stale = output.path().join("wav/1188-133604-0000.wav");
        fs::write(&stale, "stale").unwrap();

        build_from(&list(), output.path(), count(2), writing(PINNED)).unwrap();

        assert_eq!(fs::read(&stale).unwrap(), PINNED);
    }

    #[test]
    fn a_decode_the_list_does_not_pin_fails_and_names_the_utterance() {
        let output = tempfile::tempdir().unwrap();

        let built = build_from(&list(), output.path(), count(2), writing(b"other audio"));

        assert!(matches!(
            built,
            Err(LibrispeechError::Unpinned(id)) if id == "1089-134686-0000"
        ));
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
