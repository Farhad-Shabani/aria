//! Models that come from the network.

use std::fs;
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
    /// Home-relative destinations.
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
        "models/whisper/ggml-large-v3-turbo-q8_0.bin",
        Source::Url(download::WHISPER_URL),
    )],
};

pub(super) const KOKORO: Asset = Asset {
    name: "kokoro",
    title: "Kokoro TTS",
    bytes: 325_532_387 + 28_214_398,
    default: true,
    files: &[
        (
            "models/kokoro/kokoro.onnx",
            Source::Url(download::KOKORO_MODEL_URL),
        ),
        (
            "models/kokoro/voices.bin",
            Source::Url(download::KOKORO_VOICES_URL),
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
            ".wespeaker/en/model.onnx",
            Source::Url(download::SPEAKER_URL),
        )],
    },
    Asset {
        name: "segmentation",
        title: "Speaker segmentation",
        bytes: 6_958_444,
        default: true,
        files: &[(
            "models/pyannote/segmentation-3.0.onnx",
            Source::Member {
                url: download::SEGMENTATION_URL,
                member: download::SEGMENTATION_MEMBER,
            },
        )],
    },
];

pub(super) fn present(home: &Path, asset: &Asset) -> bool {
    asset
        .files
        .iter()
        .all(|(relative, _)| home.join(relative).exists())
}

/// Fetches every file of an asset. `received` gets the byte count on disk
/// so far, counted across the files, and never runs backwards; an error
/// from it ends the transfer.
pub(super) fn obtain(
    home: &Path,
    asset: &Asset,
    meter: bool,
    received: &mut dyn FnMut(u64) -> Result<(), SetupError>,
) -> Result<(), SetupError> {
    let mut finished = 0;
    for (relative, source) in asset.files {
        let dest = home.join(relative);
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    fn all() -> Vec<&'static Asset> {
        let mut assets = vec![&WHISPER, &KOKORO];
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
                    "{relative} must be home-relative"
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
