use std::fs;

pub(super) fn home(settings: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let path = aria_settings::default_path(home.path());
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, settings).unwrap();
    home
}
