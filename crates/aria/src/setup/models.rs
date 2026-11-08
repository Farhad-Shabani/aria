//! Lists the models setup downloads, and removes unused ones. A model
//! settings.toml loads is never removed.

use std::path::Path;

use crate::primitives::user_home;
use aria_settings::CheckedSettings;
use aria_voice::config::{LoadedModel, Section};

use super::assets::{self, Asset};
use super::{SetupError, model_states, settings_file};

/// One model setup can download.
pub struct Model {
    pub name: &'static str,
    pub bytes: u64,
    pub files: ModelFiles,
    pub used_by: Vec<Section>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFiles {
    Complete,
    /// Some of its files, or an unfinished download.
    Partial,
    Absent,
    /// Settings load the engine's files from elsewhere, so setup fetches
    /// nothing, whatever the models folder holds.
    External,
}

impl ModelFiles {
    pub(super) fn of(models: &Path, asset: &Asset) -> Self {
        if assets::present(models, asset) {
            Self::Complete
        } else if assets::partly_present(models, asset) {
            Self::Partial
        } else {
            Self::Absent
        }
    }

    pub fn needs_download(self) -> bool {
        matches!(self, Self::Partial | Self::Absent)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "installed",
            Self::Partial => "partial",
            Self::Absent => "-",
            Self::External => "external",
        }
    }
}

pub fn list() -> Result<Vec<Model>, SetupError> {
    let home = user_home::resolve().map_err(SetupError::Home)?;
    let file = settings_file::read(&home)?;
    listed(&home, &file)
}

/// Deletes one model's files and returns the bytes freed.
pub fn remove(name: &str) -> Result<u64, SetupError> {
    let home = user_home::resolve().map_err(SetupError::Home)?;
    remove_at(&home, name)
}

/// Under setup's lock, so a run fetching a model for settings it hasn't
/// written yet can't lose the files to a removal that saw them unused.
fn remove_at(home: &Path, name: &str) -> Result<u64, SetupError> {
    let _lock = super::lock_setup(&aria_settings::default_path(home))?;
    let file = settings_file::read(home)?;
    removed(home, &file, name)
}

fn listed(home: &Path, file: &CheckedSettings) -> Result<Vec<Model>, SetupError> {
    Ok(model_states(home, file)?
        .into_iter()
        .map(|model| Model {
            name: model.name,
            bytes: model.bytes,
            files: model.files,
            used_by: model.used_by,
        })
        .collect())
}

fn removed(home: &Path, file: &CheckedSettings, name: &str) -> Result<u64, SetupError> {
    let model = model_states(home, file)?
        .into_iter()
        .find(|model| model.name == name)
        .ok_or_else(|| SetupError::UnknownModel {
            name: name.to_owned(),
        })?;
    if !model.used_by.is_empty() {
        return Err(SetupError::ModelInUse {
            name: model.name,
            sections: model.used_by,
        });
    }
    let models_dir = aria_settings::config::models_dir(home);
    let freed = assets::remove(&models_dir, model.asset)?;
    if freed == 0 {
        return Err(SetupError::ModelNotInstalled { name: model.name });
    }
    Ok(freed)
}

/// The section that loads one of the asset's files, if any does.
pub(super) fn users(
    loaded: &[LoadedModel],
    models_dir: &Path,
    asset: &assets::Asset,
) -> Vec<Section> {
    let mut sections = Vec::new();
    for model in loaded {
        if assets::holds(models_dir, asset, &model.path) && !sections.contains(&model.section) {
            sections.push(model.section);
        }
    }
    sections
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use aria_settings::testing::file;

    fn model(home: &Path, file: &CheckedSettings, name: &str) -> Model {
        listed(home, file)
            .unwrap()
            .into_iter()
            .find(|model| model.name == name)
            .unwrap()
    }

    fn used_by(file: &CheckedSettings, name: &str) -> Vec<Section> {
        model(Path::new("/home/someone"), file, name).used_by
    }

    /// A temporary home and the models directory inside it.
    fn home() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let models = aria_settings::config::models_dir(home.path());
        (home, models)
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"model").unwrap();
    }

    #[test]
    fn an_empty_config_uses_the_default_models() {
        let file = file("");

        assert_eq!(
            used_by(&file, aria_settings::config::backend::DEFAULT),
            [Section::Stt, Section::SystemTranscription],
            "System transcription runs dictation's engine when it streams"
        );
        assert!(used_by(&file, "kokoro").is_empty());
        assert_eq!(used_by(&file, "speaker"), [Section::Speaker]);
        assert_eq!(used_by(&file, "segmentation"), [Section::Speaker]);
    }

    #[test]
    fn each_section_uses_the_model_of_its_engine() {
        let file = file(
            "[stt]\nbackend = \"parakeet\"\n\
             [transcription]\nbackend = \"moonshine\"\n\
             [tts]\nbackend = \"qwen\"\n",
        );

        assert_eq!(used_by(&file, "parakeet"), [Section::Stt]);
        assert_eq!(used_by(&file, "moonshine"), [Section::SystemTranscription]);
        assert_eq!(used_by(&file, "qwen"), [Section::Tts]);
        assert!(used_by(&file, "whisper").is_empty());
    }

    #[test]
    fn a_model_kept_elsewhere_leaves_the_download_unused() {
        let file = file("[stt]\nbackend = \"parakeet\"\nmodel = \"/elsewhere/tdt\"\n");

        assert!(used_by(&file, "parakeet").is_empty());
    }

    #[test]
    fn a_model_kept_elsewhere_is_external_even_with_its_files_here() {
        let (home, models) = home();
        for (relative, _) in assets::PARAKEET.files() {
            touch(&models.join(relative));
        }
        let file = file("[stt]\nbackend = \"parakeet\"\nmodel = \"/elsewhere/tdt\"\n");

        assert_eq!(
            model(home.path(), &file, "parakeet").files,
            ModelFiles::External
        );
    }

    #[test]
    fn an_unfinished_download_lists_as_partial() {
        let (home, models) = home();
        touch(&models.join("moonshine/base/tokenizer.json.part"));
        let file = file("");

        assert_eq!(
            model(home.path(), &file, "moonshine").files,
            ModelFiles::Partial
        );
        assert_eq!(
            model(home.path(), &file, "parakeet").files,
            ModelFiles::Absent
        );
    }

    #[test]
    fn remove_deletes_the_model_and_leaves_other_files() {
        let (home, models) = home();
        let model = models.join("moonshine/base");
        for file in [
            "encoder_model.onnx",
            "decoder_model_merged.onnx",
            "tokenizer.json.part",
        ] {
            touch(&model.join(file));
        }
        touch(&models.join("parakeet/mine.onnx"));

        let freed = removed(home.path(), &file(""), "moonshine").unwrap();

        assert_eq!(freed, 3 * b"model".len() as u64);
        assert!(!models.join("moonshine").exists());
        assert!(models.join("parakeet/mine.onnx").exists());
    }

    #[test]
    fn remove_keeps_a_directory_holding_a_file_of_the_user() {
        let (home, models) = home();
        let model = models.join("moonshine/base");
        touch(&model.join("tokenizer.json"));
        touch(&model.join("notes.txt"));

        removed(home.path(), &file(""), "moonshine").unwrap();

        assert!(!model.join("tokenizer.json").exists());
        assert!(model.join("notes.txt").exists());
    }

    #[test]
    fn remove_waits_for_no_setup_run() {
        let (home, _) = home();
        let _running = super::super::lock_setup(&aria_settings::default_path(home.path())).unwrap();

        assert!(matches!(
            remove_at(home.path(), "moonshine"),
            Err(SetupError::Busy)
        ));
    }

    #[test]
    fn remove_refuses_what_it_cannot_take() {
        let (home, models) = home();
        let parakeet = models.join("parakeet/parakeet-tdt-0.6b-v3/vocab.txt");
        touch(&parakeet);
        let file = file("[stt]\nbackend = \"parakeet\"\n");
        let refused = |name| removed(home.path(), &file, name).unwrap_err();

        assert!(matches!(
            refused("parakeet"),
            SetupError::ModelInUse { sections, .. } if sections == [Section::Stt, Section::SystemTranscription]
        ));
        assert!(matches!(
            refused("speaker"),
            SetupError::ModelInUse { sections, .. } if sections == [Section::Speaker]
        ));
        assert!(matches!(
            refused("moonshine"),
            SetupError::ModelNotInstalled { .. }
        ));
        assert!(matches!(refused("gpt"), SetupError::UnknownModel { .. }));
        assert!(parakeet.exists());
    }
}
