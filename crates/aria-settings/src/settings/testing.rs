//! Support for tests of the features that read the settings.

use toml_edit::Value;

use super::Key;

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
