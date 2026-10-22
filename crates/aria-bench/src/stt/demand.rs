//! Room noise for `aria-bench noisy`: one channel of each listed DEMAND
//! environment, 16 kHz mono, as `<output>/<ENVIRONMENT>.wav`.
//!
//! DEMAND is real recordings of rooms, streets, and transport under
//! CC BY-SA 3.0: <https://zenodo.org/records/1227121>. The committed list
//! pins each WAV's sha256, so every machine mixes the same noise.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::fetch::{self, FetchError, create_dir, ensure, io_error, parse, require, run_tool};

const LIST: &str = include_str!("../../corpus/demand-noise.tsv");
const RECORD_URL: &str = "https://zenodo.org/records/1227121/files";
const CHANNEL: &str = "ch01.wav";
const EXTRACT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub struct Fetched {
    pub recordings: usize,
    pub directory: PathBuf,
}

/// A matching WAV on disk is kept, so a cached set needs neither the
/// network nor unzip.
pub fn fetch(output: &Path) -> Result<Fetched, FetchError> {
    let output = &std::path::absolute(output).map_err(io_error(output))?;
    fetch_from(LIST, output, download)
}

/// `download` writes the WAV for an environment to the given path.
fn fetch_from(
    list: &str,
    output: &Path,
    mut download: impl FnMut(&str, &Path) -> Result<(), FetchError>,
) -> Result<Fetched, FetchError> {
    let rows = parse(list)?;
    create_dir(output)?;
    for row in &rows {
        let wav = output.join(format!("{}.wav", row.id));
        ensure(row, &wav, |wav| download(row.id, wav))?;
    }
    Ok(Fetched {
        recordings: rows.len(),
        directory: output.to_path_buf(),
    })
}

/// Staged beside `wav`, so the rename never crosses a filesystem.
fn download(environment: &str, wav: &Path) -> Result<(), FetchError> {
    require("unzip", "-v")?;
    let directory = wav.parent().unwrap_or(Path::new("."));
    let staging = tempfile::tempdir_in(directory).map_err(io_error(directory))?;
    let archive = staging.path().join("noise.zip");
    fetch::download(&format!("{RECORD_URL}/{environment}_16k.zip"), &archive)?;
    let mut unzip = Command::new("unzip");
    unzip
        .arg("-q")
        .arg(&archive)
        .arg(format!("{environment}/{CHANNEL}"))
        .arg("-d")
        .arg(staging.path());
    run_tool("unzip", &mut unzip, EXTRACT_TIMEOUT)?;
    let extracted = staging.path().join(environment).join(CHANNEL);
    fs::rename(&extracted, wav).map_err(io_error(wav))
}

#[cfg(test)]
mod tests {
    use super::super::fetch::tests::sha256;
    use super::*;

    const PINNED: &[u8] = b"pinned noise";

    fn list() -> String {
        let sha256 = sha256(PINNED);
        format!("DKITCHEN\t{sha256}\tkitchen\nNPARK\t{sha256}\tpark\n")
    }

    fn writing(bytes: &'static [u8]) -> impl FnMut(&str, &Path) -> Result<(), FetchError> {
        move |_, wav| {
            fs::write(wav, bytes).unwrap();
            Ok(())
        }
    }

    fn never(environment: &str, _: &Path) -> Result<(), FetchError> {
        panic!("{environment} should not be downloaded");
    }

    #[test]
    fn a_download_that_matches_the_list_lands_as_the_environment_wav() {
        let output = tempfile::tempdir().unwrap();

        let fetched = fetch_from(&list(), output.path(), writing(PINNED)).unwrap();

        assert_eq!(fetched.recordings, 2);
        assert_eq!(fs::read(output.path().join("NPARK.wav")).unwrap(), PINNED);
    }

    #[test]
    fn a_matching_wav_is_kept_without_downloading() {
        let output = tempfile::tempdir().unwrap();
        fetch_from(&list(), output.path(), writing(PINNED)).unwrap();

        fetch_from(&list(), output.path(), never).unwrap();
    }

    #[test]
    fn a_download_the_list_does_not_pin_fails_and_names_the_environment() {
        let output = tempfile::tempdir().unwrap();

        let fetched = fetch_from(&list(), output.path(), writing(b"other noise"));

        assert!(matches!(fetched, Err(FetchError::Unpinned(name)) if name == "DKITCHEN"));
    }

    #[test]
    fn the_committed_list_pins_six_environments() {
        let rows = parse(LIST).unwrap();

        assert_eq!(rows.len(), 6);
        for row in rows {
            assert_eq!(row.sha256.len(), 64, "{}", row.id);
        }
    }
}
