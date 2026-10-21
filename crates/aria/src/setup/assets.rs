//! Models that come from the network.

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use aria_kit::backend::{ModelDownload, Unpack};

use super::SetupError;
use super::download;

const MODEL_DIR_MODE: u32 = 0o755;

pub(super) struct Asset {
    pub(super) name: &'static str,
    pub(super) title: &'static str,
    /// Whether it is fetched when nobody answers.
    pub(super) default: bool,
    /// Each engine's `MODEL`, paired with the folder under the models
    /// directory its files land in.
    pub(super) folders: &'static [(&'static str, &'static [ModelDownload])],
}

impl Asset {
    /// Each file, with its destination relative to the models directory.
    pub(super) fn files(&self) -> impl Iterator<Item = (PathBuf, &'static ModelDownload)> {
        self.folders.iter().flat_map(|(folder, model)| {
            model
                .iter()
                .map(move |file| (Path::new(folder).join(file.file_name), file))
        })
    }

    /// What the transfer moves: every file, or the archive one is cut from.
    pub(super) fn total_bytes(&self) -> u64 {
        self.files().map(|(_, file)| file.bytes).sum()
    }
}

pub(super) const WHISPER: Asset = Asset {
    name: "whisper",
    title: "Whisper large-v3-turbo",
    default: true,
    folders: &[("whisper", aria_stt::whisper::MODEL)],
};

pub(super) const WHISPER_CANDLE: Asset = Asset {
    name: "whisper-candle",
    title: "Whisper tiny.en (candle)",
    default: true,
    folders: &[("whisper-candle/tiny.en", aria_stt::whisper_candle::MODEL)],
};

pub(super) const PARAKEET: Asset = Asset {
    name: "parakeet",
    title: "Parakeet TDT 0.6B",
    default: true,
    folders: &[("parakeet/parakeet-tdt-0.6b-v3", aria_stt::parakeet::MODEL)],
};

pub(super) const MOONSHINE: Asset = Asset {
    name: "moonshine",
    title: "Moonshine base",
    default: true,
    folders: &[("moonshine/base", aria_stt::moonshine::MODEL)],
};

pub(super) const NEMOTRON: Asset = Asset {
    name: "nemotron",
    title: "Nemotron streaming 0.6B",
    default: true,
    folders: &[(
        "nemotron/nemotron-speech-streaming-en-0.6b",
        aria_stt::nemotron::MODEL,
    )],
};

pub(super) const KOKORO: Asset = Asset {
    name: "kokoro",
    title: "Kokoro TTS",
    default: true,
    folders: &[("kokoro", aria_tts::kokoro::MODEL)],
};

pub(super) const QWEN: Asset = Asset {
    name: "qwen",
    title: "Qwen3 TTS",
    default: true,
    folders: &[
        (
            "qwen/qwen3-tts-0.6b-customvoice",
            aria_tts::qwen::model_files::MODEL,
        ),
        (
            "qwen/qwen3-tts-tokenizer",
            aria_tts::qwen::model_files::SPEECH_TOKENIZER,
        ),
    ],
};

/// The embedding model an enrollment needs. Named because setup only offers
/// to record a voice once this one is on disk.
pub(super) const SPEAKER: &str = "speaker";

/// Wanted by a feature rather than by a chosen engine: speaker
/// verification and the diarization `aria listen` does.
pub(super) const EXTRAS: [Asset; 2] = [
    Asset {
        name: SPEAKER,
        title: "Speaker verification",
        default: false,
        folders: &[("wespeaker/en", aria_speaker::wespeaker::MODEL)],
    },
    Asset {
        name: "segmentation",
        title: "Speaker segmentation",
        default: true,
        folders: &[("pyannote", aria_segmentation::pyannote::MODEL)],
    },
];

pub(super) fn present(models: &Path, asset: &Asset) -> bool {
    asset
        .files()
        .all(|(relative, _)| models.join(relative).exists())
}

/// Whether `path` is one of the asset's files, or a directory holding one.
pub(super) fn holds(models: &Path, asset: &Asset, path: &Path) -> bool {
    asset
        .files()
        .any(|(relative, _)| models.join(relative).starts_with(path))
}

/// Whether any of the asset's files, finished or `.part`, is on disk.
pub(super) fn partly_present(models: &Path, asset: &Asset) -> bool {
    asset.files().any(|(relative, _)| {
        let file = models.join(relative);
        file.exists() || download::partial(&file).exists()
    })
}

/// Fetches every file of an asset. `received` gets the byte count on disk
/// so far, counted across the files, and never runs backwards; an error
/// from it ends the transfer.
pub(super) fn obtain(
    models: &Path,
    asset: &Asset,
    meter: bool,
    received: &mut dyn FnMut(u64) -> Result<(), SetupError>,
) -> Result<(), SetupError> {
    let mut finished = 0;
    for (relative, file) in asset.files() {
        let dest = models.join(relative);
        if let Some(parent) = dest.parent() {
            // Not group-writable, whatever the umask: anyone who can write
            // the folder can swap a part for a link after its hash check.
            fs::DirBuilder::new()
                .recursive(true)
                .mode(MODEL_DIR_MODE)
                .create(parent)
                .map_err(|source| SetupError::Io {
                    action: "create",
                    path: parent.to_path_buf(),
                    source,
                })?;
        }
        let mut progress = |bytes| received(finished + bytes);
        match file.unpack {
            Unpack::Plain => download::fetch(file, &dest, meter, &mut progress)?,
            Unpack::TarBz2Member(member) => {
                download::fetch_member(file, member, &dest, meter, &mut progress)?;
            }
        }
        finished += fs::metadata(&dest)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
    }
    Ok(())
}

/// Deletes the asset's files and their `.part` files, then any folders left
/// empty, up to the models directory. Returns the bytes freed.
pub(super) fn remove(models: &Path, asset: &Asset) -> Result<u64, SetupError> {
    let mut freed = 0;
    for (relative, _) in asset.files() {
        let file = models.join(relative);
        freed += remove_file(&file)?;
        freed += remove_file(&download::partial(&file))?;
        let parents = file.ancestors().skip(1).take_while(|dir| *dir != models);
        for dir in parents {
            match fs::remove_dir(dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                // Still holds the user's files, or is a link: leave it.
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotADirectory
                    ) =>
                {
                    break;
                }
                Err(source) => {
                    return Err(SetupError::Io {
                        action: "remove",
                        path: dir.to_path_buf(),
                        source,
                    });
                }
            }
        }
    }
    Ok(freed)
}

/// Deletes one file. Returns its size, or 0 when it was absent.
fn remove_file(path: &Path) -> Result<u64, SetupError> {
    let io_error = |source| SetupError::Io {
        action: "remove",
        path: path.to_path_buf(),
        source,
    };
    let bytes = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(source) => return Err(io_error(source)),
    };
    fs::remove_file(path).map_err(io_error)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn all() -> Vec<&'static Asset> {
        let mut assets = vec![
            &WHISPER,
            &WHISPER_CANDLE,
            &PARAKEET,
            &MOONSHINE,
            &NEMOTRON,
            &KOKORO,
            &QWEN,
        ];
        assets.extend(EXTRAS.iter());
        assets
    }

    #[test]
    fn every_asset_lands_in_its_own_place() {
        let mut seen = HashSet::new();
        for asset in all() {
            assert!(asset.files().next().is_some(), "{}", asset.name);
            assert!(asset.total_bytes() > 0, "{}", asset.name);
            for (relative, _) in asset.files() {
                let shown = relative.display().to_string();
                assert!(
                    relative.is_relative(),
                    "{shown} must be relative to the models directory"
                );
                assert!(seen.insert(relative), "{shown} is claimed twice");
            }
        }
    }

    #[test]
    fn every_file_names_a_sha256_in_lowercase_hex() {
        for (relative, file) in all().into_iter().flat_map(|asset| asset.files()) {
            assert!(
                file.sha256.len() == 64
                    && file
                        .sha256
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
                "{} has no usable SHA-256",
                relative.display()
            );
        }
    }

    #[test]
    fn an_asset_is_present_only_with_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let files: Vec<_> = KOKORO.files().collect();
        let [(model, _), (voices, _)] = files.as_slice() else {
            panic!("kokoro ships a model and its voices");
        };
        fs::create_dir_all(dir.path().join(model).parent().unwrap()).unwrap();

        fs::write(dir.path().join(model), b"").unwrap();
        assert!(!present(dir.path(), &KOKORO));

        fs::write(dir.path().join(voices), b"").unwrap();
        assert!(present(dir.path(), &KOKORO));
    }
}
