//! Lists the models setup downloads, and removes unused ones. A model
//! settings.toml loads is never removed.

use std::path::Path;

use crate::config::{self, LoadedModel};
use crate::primitives::user_home;
use crate::settings::SettingsFile;

use super::assets::{self, Asset};
use super::{SetupError, model_states, settings_file};

/// One model setup can download.
pub struct Model {
    pub name: &'static str,
    pub bytes: u64,
    pub on_disk: OnDisk,
    /// The settings table that loads it.
    pub used_by: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDisk {
    Complete,
    /// Some of its files, or an unfinished download.
    Partial,
    Absent,
}

impl OnDisk {
    pub(super) fn of(models: &Path, asset: &Asset) -> Self {
        if assets::present(models, asset) {
            Self::Complete
        } else if assets::partly_present(models, asset) {
            Self::Partial
        } else {
            Self::Absent
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "installed",
            Self::Partial => "partial",
            Self::Absent => "-",
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
    let file = settings_file::read(&home)?;
    removed(&home, &file, name)
}

fn listed(home: &Path, file: &SettingsFile) -> Result<Vec<Model>, SetupError> {
    let models_dir = config::models_dir(home);
    let loaded = config::loaded_models(file, home).map_err(SetupError::Config)?;
    Ok(model_states(home, file)?
        .into_iter()
        .map(|model| Model {
            name: model.name,
            bytes: model.bytes,
            on_disk: model.on_disk,
            used_by: user(&loaded, &models_dir, model.asset),
        })
        .collect())
}

fn removed(home: &Path, file: &SettingsFile, name: &str) -> Result<u64, SetupError> {
    let model = model_states(home, file)?
        .into_iter()
        .find(|model| model.name == name)
        .ok_or_else(|| SetupError::UnknownModel {
            name: name.to_owned(),
        })?;
    let models_dir = config::models_dir(home);
    let loaded = config::loaded_models(file, home).map_err(SetupError::Config)?;
    if let Some(section) = user(&loaded, &models_dir, model.asset) {
        return Err(SetupError::ModelInUse {
            name: model.name,
            section,
        });
    }
    let freed = assets::remove(&models_dir, model.asset)?;
    if freed == 0 {
        return Err(SetupError::ModelNotInstalled { name: model.name });
    }
    Ok(freed)
}

/// The section that loads one of the asset's files, if any does.
fn user(loaded: &[LoadedModel], models_dir: &Path, asset: &assets::Asset) -> Option<&'static str> {
    loaded
        .iter()
        .find(|model| assets::holds(models_dir, asset, &model.path))
        .map(|model| model.section)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

    /// The typed values of `text`, before validation, so a test can reach a
    /// table that does not resolve.
    fn config(text: &str) -> SettingsFile {
        toml_edit::de::from_str(text).unwrap()
    }

    fn model(home: &Path, file: &SettingsFile, name: &str) -> Model {
        listed(home, file)
            .unwrap()
            .into_iter()
            .find(|model| model.name == name)
            .unwrap()
    }

    fn used_by(file: &SettingsFile, name: &str) -> Option<&'static str> {
        model(Path::new("/home/someone"), file, name).used_by
    }

    /// A temporary home and the models directory inside it.
    fn home() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let models = config::models_dir(home.path());
        (home, models)
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"model").unwrap();
    }

    #[test]
    fn an_empty_config_uses_the_default_models() {
        let file = config("");

        assert_eq!(used_by(&file, config::backend::DEFAULT), Some("stt"));
        assert_eq!(used_by(&file, "kokoro"), None);
        assert_eq!(used_by(&file, "speaker"), Some("speaker"));
        assert_eq!(used_by(&file, "segmentation"), Some("speaker"));
    }

    #[test]
    fn each_section_uses_the_model_of_its_engine() {
        let file = config(
            "[stt]\nbackend = \"parakeet\"\n\
             [listen]\nbackend = \"moonshine\"\n\
             [tts]\nbackend = \"qwen\"\n",
        );

        assert_eq!(used_by(&file, "parakeet"), Some("stt"));
        assert_eq!(used_by(&file, "moonshine"), Some("listen"));
        assert_eq!(used_by(&file, "qwen"), Some("tts"));
        assert_eq!(used_by(&file, "whisper"), None);
    }

    #[test]
    fn a_model_kept_elsewhere_leaves_the_download_unused() {
        let file = config("[stt]\nbackend = \"parakeet\"\nmodel = \"/elsewhere/tdt\"\n");

        assert_eq!(used_by(&file, "parakeet"), None);
    }

    #[test]
    fn an_unfinished_download_lists_as_partial() {
        let (home, models) = home();
        touch(&models.join("moonshine/base/tokenizer.json.part"));
        let file = config("");

        assert_eq!(
            model(home.path(), &file, "moonshine").on_disk,
            OnDisk::Partial
        );
        assert_eq!(
            model(home.path(), &file, "parakeet").on_disk,
            OnDisk::Absent
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

        let freed = removed(home.path(), &config(""), "moonshine").unwrap();

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

        removed(home.path(), &config(""), "moonshine").unwrap();

        assert!(!model.join("tokenizer.json").exists());
        assert!(model.join("notes.txt").exists());
    }

    /// Nothing is removed while a table that does not resolve might use it.
    #[test]
    fn remove_waits_for_a_config_that_resolves() {
        let (home, models) = home();
        let parakeet = models.join("parakeet/parakeet-tdt-0.6b-v3/vocab.txt");
        touch(&parakeet);
        let file = config("[stt]\nbackend = \"parakeet-mlx\"\n");

        let refused = removed(home.path(), &file, "parakeet").unwrap_err();

        assert!(matches!(refused, SetupError::Config(_)));
        assert!(parakeet.exists());
    }

    #[test]
    fn remove_refuses_what_it_cannot_take() {
        let (home, models) = home();
        let parakeet = models.join("parakeet/parakeet-tdt-0.6b-v3/vocab.txt");
        touch(&parakeet);
        let file = config("[stt]\nbackend = \"parakeet\"\n");
        let refused = |name| removed(home.path(), &file, name).unwrap_err();

        assert!(matches!(
            refused("parakeet"),
            SetupError::ModelInUse { section: "stt", .. }
        ));
        assert!(matches!(
            refused("speaker"),
            SetupError::ModelInUse {
                section: "speaker",
                ..
            }
        ));
        assert!(matches!(
            refused("moonshine"),
            SetupError::ModelNotInstalled { .. }
        ));
        assert!(matches!(refused("gpt"), SetupError::UnknownModel { .. }));
        assert!(parakeet.exists());
    }
}
