//! Support for tests of the features that read the settings.

use std::path::PathBuf;

use toml_edit::Value;

use super::{Key, Loaded, SettingsFile};

pub fn test_home() -> PathBuf {
    PathBuf::from("/home/user")
}

/// The typed values of `text`, read as if home were `test_home()`.
pub fn file(text: &str) -> SettingsFile {
    Loaded::parse(text, &test_home())
        .expect("the file is valid")
        .settings
}

/// Every default, as one file. It must load, and must read back as the
/// same values code uses when a key is absent.
pub fn defaults_text() -> String {
    let mut document = toml_edit::DocumentMut::new();
    for key in Key::ALL {
        let Some(literal) = key.default() else {
            continue;
        };
        let value: Value = literal.parse().expect("a default is a TOML literal");
        let segments: Vec<&str> = key.segments().collect();
        let table = super::document::table_at(document.as_table_mut(), segments.iter().copied())
            .expect("a default's table");
        table.insert(segments[segments.len() - 1], toml_edit::Item::Value(value));
    }
    document.to_string()
}
