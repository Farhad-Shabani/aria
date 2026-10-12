//! Validates model paths, file types, ownership, and permissions before loading.

use std::path::PathBuf;

use crate::primitives::file_inspection::InspectedFile;

use super::ModelError;

pub fn validate_model_file(path: PathBuf) -> Result<PathBuf, ModelError> {
    InspectedFile::inspect(path)
        .and_then(InspectedFile::require_regular_file)
        .and_then(InspectedFile::require_trusted)
        .map_err(ModelError::Path)
}

pub fn validate_model_directory(
    path: PathBuf,
    required: &[&'static str],
) -> Result<PathBuf, ModelError> {
    let directory = InspectedFile::inspect(path)
        .and_then(InspectedFile::require_directory)
        .and_then(InspectedFile::require_trusted)
        .map_err(ModelError::Path)?;

    let entries = std::fs::read_dir(&directory).map_err(|source| ModelError::ReadDirectory {
        path: directory.clone(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| ModelError::ReadDirectory {
            path: directory.clone(),
            source,
        })?;
        let candidate = InspectedFile::inspect(entry.path()).map_err(ModelError::Path)?;
        // Check regular files beside the weights; subdirectories are not traversed.
        if candidate.is_regular_file() {
            candidate.require_trusted().map_err(ModelError::Path)?;
        }
    }
    if let Some(&file) = required.iter().find(|file| !directory.join(file).is_file()) {
        return Err(ModelError::Missing { directory, file });
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use crate::primitives::file_inspection::InspectionError;

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    const SECURE_MODE: u32 = 0o644;
    const GROUP_WRITABLE_MODE: u32 = 0o664;
    const GROUP_WRITABLE_DIRECTORY_MODE: u32 = 0o775;

    fn model_directory() -> TempDir {
        let directory = TempDir::new().expect("test directory should be created");
        write_model_file(directory.path(), "vocab.txt", SECURE_MODE);
        directory
    }

    fn write_model_file(directory: &Path, name: &str, mode: u32) {
        let path = directory.join(name);
        fs::write(&path, []).expect("test model file should be written");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode))
            .expect("test model file permissions should be set");
    }

    #[test]
    fn a_directory_of_trusted_files_is_accepted() {
        let directory = model_directory();

        let validated = validate_model_directory(directory.path().to_path_buf(), &[])
            .expect("a trusted model directory should be accepted");

        assert_eq!(
            validated,
            fs::canonicalize(directory.path()).expect("the directory should canonicalize")
        );
    }

    #[test]
    fn a_nested_directory_is_left_alone() {
        let directory = model_directory();
        let nested = directory.path().join(".cache");
        fs::create_dir(&nested).expect("nested directory should be created");
        fs::set_permissions(
            &nested,
            fs::Permissions::from_mode(GROUP_WRITABLE_DIRECTORY_MODE),
        )
        .expect("nested directory permissions should be set");

        assert!(validate_model_directory(directory.path().to_path_buf(), &[]).is_ok());
    }

    #[test]
    fn a_directory_is_not_a_model_file() {
        let directory = TempDir::new().expect("test directory should be created");

        let error = validate_model_file(directory.path().to_path_buf())
            .expect_err("a directory is not a model file");

        assert!(matches!(
            error,
            ModelError::Path(InspectionError::NotRegularFile { .. })
        ));
    }

    #[test]
    fn a_missing_required_file_is_named() {
        let directory = model_directory();

        let error = validate_model_directory(
            directory.path().to_path_buf(),
            &["vocab.txt", "encoder-model.onnx"],
        )
        .expect_err("a directory without a required file must be rejected");

        assert!(matches!(
            error,
            ModelError::Missing {
                file: "encoder-model.onnx",
                ..
            }
        ));
    }

    #[test]
    fn a_group_writable_entry_is_rejected() {
        let directory = model_directory();
        write_model_file(directory.path(), "encoder-model.onnx", GROUP_WRITABLE_MODE);

        let error = validate_model_directory(directory.path().to_path_buf(), &[])
            .expect_err("a group-writable model file must be rejected");

        assert!(matches!(
            error,
            ModelError::Path(InspectionError::WritableByOthers { actual_mode, .. })
                if actual_mode == GROUP_WRITABLE_MODE
        ));
    }

    #[test]
    fn a_group_writable_model_file_is_rejected() {
        let directory = TempDir::new().expect("test directory should be created");
        write_model_file(directory.path(), "model.bin", GROUP_WRITABLE_MODE);

        let error = validate_model_file(directory.path().join("model.bin"))
            .expect_err("a group-writable model file must be rejected");

        assert!(matches!(
            error,
            ModelError::Path(InspectionError::WritableByOthers { actual_mode, .. })
                if actual_mode == GROUP_WRITABLE_MODE
        ));
    }

    #[test]
    fn a_group_writable_model_directory_is_rejected() {
        let directory = model_directory();
        fs::set_permissions(
            directory.path(),
            fs::Permissions::from_mode(GROUP_WRITABLE_DIRECTORY_MODE),
        )
        .expect("test directory permissions should be set");

        let error = validate_model_directory(directory.path().to_path_buf(), &[])
            .expect_err("a group-writable model directory must be rejected");

        assert!(matches!(
            error,
            ModelError::Path(InspectionError::WritableByOthers { actual_mode, .. })
                if actual_mode == GROUP_WRITABLE_DIRECTORY_MODE
        ));
    }

    #[test]
    fn a_regular_file_is_not_a_model_directory() {
        let directory = TempDir::new().expect("test directory should be created");
        write_model_file(directory.path(), "model.bin", SECURE_MODE);

        let error = validate_model_directory(directory.path().join("model.bin"), &[])
            .expect_err("a regular file must be rejected");

        assert!(matches!(
            error,
            ModelError::Path(InspectionError::NotDirectory { .. })
        ));
    }
}
