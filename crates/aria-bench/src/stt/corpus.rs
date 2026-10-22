//! The reference recordings and what was actually said in them.

use std::fs;
use std::path::{Path, PathBuf};

use super::normalize;

/// One line per utterance: an absolute path to a canonical 16 kHz mono 16-bit
/// WAV, a tab, and the reference text.
pub(super) struct Corpus {
    pub(super) utterances: Vec<Utterance>,
}

impl Corpus {
    pub(super) fn load(manifest: &Path) -> Result<Self, CorpusError> {
        let text = fs::read_to_string(manifest).map_err(|source| CorpusError::Read {
            path: manifest.to_path_buf(),
            source,
        })?;

        let mut utterances = Vec::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            utterances.push(Utterance::parse(line, index + 1, manifest)?);
        }
        if utterances.is_empty() {
            return Err(CorpusError::Empty {
                path: manifest.to_path_buf(),
            });
        }
        Ok(Self { utterances })
    }
}

pub struct Built {
    pub utterances: usize,
    /// Counted with the scorer's own word split, so a report's header and
    /// its rates share a denominator.
    pub words: usize,
    pub manifest: PathBuf,
}

pub(super) fn write_manifest<'a>(
    path: &Path,
    utterances: impl IntoIterator<Item = (&'a Path, &'a str)>,
) -> Result<Built, CorpusError> {
    let mut lines = String::new();
    let mut built = Built {
        utterances: 0,
        words: 0,
        manifest: path.to_path_buf(),
    };
    for (audio, reference) in utterances {
        if !audio.is_absolute() {
            return Err(CorpusError::RelativeAudio {
                path: path.to_path_buf(),
                line: built.utterances + 1,
            });
        }
        lines.push_str(&format!("{}\t{reference}\n", audio.display()));
        built.utterances += 1;
        built.words += normalize::words(reference).len();
    }
    fs::write(path, lines).map_err(|source| CorpusError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(built)
}

pub(super) struct Utterance {
    /// Names this utterance in the result rows. Taken from the audio file name,
    /// which is how read-speech corpora identify their recordings.
    pub(super) id: String,
    pub(super) audio: PathBuf,
    pub(super) reference: String,
}

impl Utterance {
    fn parse(line: &str, number: usize, manifest: &Path) -> Result<Self, CorpusError> {
        let (audio, reference) = line
            .split_once('\t')
            .filter(|(_audio, reference)| !reference.trim().is_empty())
            .ok_or_else(|| CorpusError::Malformed {
                path: manifest.to_path_buf(),
                line: number,
            })?;
        let audio = PathBuf::from(audio);
        if !audio.is_absolute() {
            return Err(CorpusError::RelativeAudio {
                path: manifest.to_path_buf(),
                line: number,
            });
        }
        let id = audio
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| CorpusError::Unnamed {
                path: manifest.to_path_buf(),
                line: number,
            })?
            .to_owned();

        Ok(Self {
            id,
            audio,
            reference: reference.to_owned(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error("failed to read the corpus manifest {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to write the corpus manifest {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{} line {line} has no tab and reference text after the audio path", path.display())]
    Malformed { path: PathBuf, line: usize },

    #[error("{} line {line} has a relative audio path; it must be absolute", path.display())]
    RelativeAudio { path: PathBuf, line: usize },

    #[error("{} line {line} has no usable name for its audio file", path.display())]
    Unnamed { path: PathBuf, line: usize },

    #[error("{} holds no utterances; an empty corpus scores every backend perfect", path.display())]
    Empty { path: PathBuf },
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    fn manifest(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().expect("the test manifest should be created");
        write!(file, "{contents}").expect("the test manifest should be written");
        file
    }

    #[test]
    fn each_line_becomes_an_utterance_named_after_its_audio() {
        let manifest = manifest("/corpus/1089-134686-0000.wav\tTHE SOCKET CLOSES\n\n");

        let corpus = Corpus::load(manifest.path()).expect("the manifest should load");

        assert_eq!(corpus.utterances.len(), 1);
        assert_eq!(corpus.utterances[0].id, "1089-134686-0000");
        assert_eq!(corpus.utterances[0].reference, "THE SOCKET CLOSES");
    }

    #[test]
    fn a_line_without_a_tab_is_refused_by_number() {
        let manifest = manifest("/corpus/one.wav THE SOCKET CLOSES\n");

        let loaded = Corpus::load(manifest.path());

        assert!(matches!(
            loaded,
            Err(CorpusError::Malformed { line: 1, .. })
        ));
    }

    #[test]
    fn a_line_without_reference_text_is_refused() {
        let manifest = manifest("/corpus/one.wav\t   \n");

        let loaded = Corpus::load(manifest.path());

        assert!(matches!(
            loaded,
            Err(CorpusError::Malformed { line: 1, .. })
        ));
    }

    #[test]
    fn a_relative_audio_path_is_refused_by_number() {
        let manifest = manifest("corpus/one.wav\tTHE SOCKET CLOSES\n");

        let loaded = Corpus::load(manifest.path());

        assert!(matches!(
            loaded,
            Err(CorpusError::RelativeAudio { line: 1, .. })
        ));
    }

    #[test]
    fn a_written_manifest_loads_back_and_counts_scored_words() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("manifest.tsv");

        let built = write_manifest(
            &path,
            [
                (Path::new("/corpus/one.wav"), "THE SOCKET CLOSES"),
                (Path::new("/corpus/two.wav"), "it is open, again"),
            ],
        )
        .unwrap();

        assert_eq!((built.utterances, built.words), (2, 7));
        assert_eq!(Corpus::load(&path).unwrap().utterances.len(), 2);
        assert!(matches!(
            write_manifest(&path, [(Path::new("corpus/one.wav"), "X")]),
            Err(CorpusError::RelativeAudio { line: 1, .. })
        ));
    }

    #[test]
    fn an_empty_manifest_is_refused() {
        let manifest = manifest("\n  \n");

        let loaded = Corpus::load(manifest.path());

        assert!(matches!(loaded, Err(CorpusError::Empty { .. })));
    }
}
