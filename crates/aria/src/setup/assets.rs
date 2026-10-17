//! Models that come from the network.

use std::fs;
use std::io;
use std::path::Path;

use super::SetupError;
use super::download;

pub(super) struct Asset {
    pub(super) name: &'static str,
    pub(super) title: &'static str,
    /// What the transfer moves: every file, or the archive one is cut from.
    pub(super) bytes: u64,
    /// Whether it is fetched when nobody answers.
    pub(super) default: bool,
    /// Destinations relative to the models directory.
    pub(super) files: &'static [(&'static str, Source)],
}

pub(super) enum Source {
    Url(&'static str),
    /// One member of a `.tar.bz2` archive.
    Member {
        url: &'static str,
        member: &'static str,
    },
}

pub(super) const WHISPER: Asset = Asset {
    name: "whisper",
    title: "Whisper large-v3-turbo",
    bytes: 874_188_075,
    default: true,
    files: &[(
        "whisper/ggml-large-v3-turbo-q8_0.bin",
        Source::Url(download::WHISPER_URL),
    )],
};

pub(super) const WHISPER_CANDLE: Asset = Asset {
    name: "whisper-candle",
    title: "Whisper tiny.en (candle)",
    bytes: 151_060_136 + 2_405_679 + 1_937,
    default: true,
    files: &[
        (
            "whisper-candle/tiny.en/model.safetensors",
            Source::Url(download::WHISPER_CANDLE_MODEL_URL),
        ),
        (
            "whisper-candle/tiny.en/tokenizer.json",
            Source::Url(download::WHISPER_CANDLE_TOKENIZER_URL),
        ),
        (
            "whisper-candle/tiny.en/config.json",
            Source::Url(download::WHISPER_CANDLE_CONFIG_URL),
        ),
    ],
};

pub(super) const PARAKEET: Asset = Asset {
    name: "parakeet",
    title: "Parakeet TDT 0.6B",
    bytes: 41_770_866 + 2_435_420_160 + 72_520_893 + 93_939,
    default: true,
    files: &[
        (
            "parakeet/parakeet-tdt-0.6b-v3/encoder-model.onnx",
            Source::Url(download::PARAKEET_ENCODER_URL),
        ),
        (
            "parakeet/parakeet-tdt-0.6b-v3/encoder-model.onnx.data",
            Source::Url(download::PARAKEET_ENCODER_DATA_URL),
        ),
        (
            "parakeet/parakeet-tdt-0.6b-v3/decoder_joint-model.onnx",
            Source::Url(download::PARAKEET_DECODER_URL),
        ),
        (
            "parakeet/parakeet-tdt-0.6b-v3/vocab.txt",
            Source::Url(download::PARAKEET_VOCAB_URL),
        ),
    ],
};

pub(super) const MOONSHINE: Asset = Asset {
    name: "moonshine",
    title: "Moonshine base",
    bytes: 20_513_063 + 42_498_870 + 3_761_754,
    default: true,
    files: &[
        (
            "moonshine/base/encoder_model.onnx",
            Source::Url(download::MOONSHINE_ENCODER_URL),
        ),
        (
            "moonshine/base/decoder_model_merged.onnx",
            Source::Url(download::MOONSHINE_DECODER_URL),
        ),
        (
            "moonshine/base/tokenizer.json",
            Source::Url(download::MOONSHINE_TOKENIZER_URL),
        ),
    ],
};

pub(super) const NEMOTRON: Asset = Asset {
    name: "nemotron",
    title: "Nemotron streaming 0.6B",
    bytes: 42_159_995 + 2_436_567_040 + 35_779_240 + 251_056,
    default: true,
    files: &[
        (
            "nemotron/nemotron-speech-streaming-en-0.6b/encoder.onnx",
            Source::Url(download::NEMOTRON_ENCODER_URL),
        ),
        (
            "nemotron/nemotron-speech-streaming-en-0.6b/encoder.onnx.data",
            Source::Url(download::NEMOTRON_ENCODER_DATA_URL),
        ),
        (
            "nemotron/nemotron-speech-streaming-en-0.6b/decoder_joint.onnx",
            Source::Url(download::NEMOTRON_DECODER_URL),
        ),
        (
            "nemotron/nemotron-speech-streaming-en-0.6b/tokenizer.model",
            Source::Url(download::NEMOTRON_TOKENIZER_URL),
        ),
    ],
};

pub(super) const KOKORO: Asset = Asset {
    name: "kokoro",
    title: "Kokoro TTS",
    bytes: 325_532_387 + 28_214_398,
    default: true,
    files: &[
        (
            "kokoro/kokoro.onnx",
            Source::Url(download::KOKORO_MODEL_URL),
        ),
        (
            "kokoro/voices.bin",
            Source::Url(download::KOKORO_VOICES_URL),
        ),
    ],
};

pub(super) const QWEN: Asset = Asset {
    name: "qwen",
    title: "Qwen3 TTS",
    bytes: 1_811_626_576 + 4_908 + 2_776_833 + 1_671_839 + 7_344 + 682_293_092 + 2_336,
    default: true,
    files: &[
        (
            "qwen/qwen3-tts-0.6b-customvoice/model.safetensors",
            Source::Url(download::QWEN_MODEL_URL),
        ),
        (
            "qwen/qwen3-tts-0.6b-customvoice/config.json",
            Source::Url(download::QWEN_CONFIG_URL),
        ),
        (
            "qwen/qwen3-tts-0.6b-customvoice/vocab.json",
            Source::Url(download::QWEN_VOCAB_URL),
        ),
        (
            "qwen/qwen3-tts-0.6b-customvoice/merges.txt",
            Source::Url(download::QWEN_MERGES_URL),
        ),
        (
            "qwen/qwen3-tts-0.6b-customvoice/tokenizer_config.json",
            Source::Url(download::QWEN_TOKENIZER_CONFIG_URL),
        ),
        (
            "qwen/qwen3-tts-tokenizer/model.safetensors",
            Source::Url(download::QWEN_CODEC_MODEL_URL),
        ),
        (
            "qwen/qwen3-tts-tokenizer/config.json",
            Source::Url(download::QWEN_CODEC_CONFIG_URL),
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
        bytes: 26_530_309,
        default: false,
        files: &[(
            "wespeaker/en/model.onnx",
            Source::Url(download::SPEAKER_URL),
        )],
    },
    Asset {
        name: "segmentation",
        title: "Speaker segmentation",
        bytes: 6_958_444,
        default: true,
        files: &[(
            "pyannote/segmentation-3.0.onnx",
            Source::Member {
                url: download::SEGMENTATION_URL,
                member: download::SEGMENTATION_MEMBER,
            },
        )],
    },
];

pub(super) fn present(models: &Path, asset: &Asset) -> bool {
    asset
        .files
        .iter()
        .all(|(relative, _)| models.join(relative).exists())
}

/// Whether `path` is one of the asset's files, or a directory holding one.
pub(super) fn holds(models: &Path, asset: &Asset, path: &Path) -> bool {
    asset
        .files
        .iter()
        .any(|(relative, _)| models.join(relative).starts_with(path))
}

/// Whether any of the asset's files, finished or `.part`, is on disk.
pub(super) fn partly_present(models: &Path, asset: &Asset) -> bool {
    asset.files.iter().any(|(relative, _)| {
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
    for (relative, source) in asset.files {
        let dest = models.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|source| SetupError::Io {
                action: "create",
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut progress = |bytes| received(finished + bytes);
        match source {
            Source::Url(url) => download::fetch(url, &dest, meter, &mut progress)?,
            Source::Member { url, member } => {
                download::fetch_member(url, member, &dest, meter, &mut progress)?;
            }
        }
        finished += fs::metadata(&dest).map(|file| file.len()).unwrap_or(0);
    }
    Ok(())
}

/// Deletes the asset's files and their `.part` files, then any folders left
/// empty, up to the models directory. Returns the bytes freed.
pub(super) fn remove(models: &Path, asset: &Asset) -> Result<u64, SetupError> {
    let mut freed = 0;
    for (relative, _) in asset.files {
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
            assert!(!asset.files.is_empty(), "{}", asset.name);
            assert!(asset.bytes > 0, "{}", asset.name);
            for (relative, _) in asset.files {
                assert!(seen.insert(*relative), "{relative} is claimed twice");
                assert!(
                    !relative.starts_with('/'),
                    "{relative} must be relative to the models directory"
                );
            }
        }
    }

    #[test]
    fn an_asset_is_present_only_with_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let [(model, _), (voices, _)] = KOKORO.files else {
            panic!("kokoro ships a model and its voices");
        };
        fs::create_dir_all(dir.path().join(model).parent().unwrap()).unwrap();

        fs::write(dir.path().join(model), b"").unwrap();
        assert!(!present(dir.path(), &KOKORO));

        fs::write(dir.path().join(voices), b"").unwrap();
        assert!(present(dir.path(), &KOKORO));
    }
}
