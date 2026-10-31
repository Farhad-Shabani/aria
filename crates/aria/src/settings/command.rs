//! What `aria settings` does. The CLI writes the file itself, daemon or not,
//! through the one write path; a running daemon picks the change up by its
//! poll.

use std::path::{Path, PathBuf};

use toml_edit::Value;

use super::keys::{DELIVERY_MODES, Kind, OUTPUT_MODES};
use super::{
    Apply, Key, Patch, Refusal, SettingsError, SettingsFault, Written, default_path, document,
};
use crate::primitives::user_home::{self, UserHomeError};

/// One key as `aria settings show` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub key: Key,
    /// settings.toml holds the key.
    pub set: bool,
    /// The value in effect, as JSON. `Null` when absent and no single value
    /// stands for that, such as "the engine's own voice".
    pub value: serde_json::Value,
    /// The running daemon still uses its startup value.
    pub restart_required: bool,
}

impl Row {
    pub fn path(&self) -> &'static str {
        self.key.path()
    }

    pub fn apply(&self) -> Apply {
        self.key.apply()
    }

    pub fn description(&self) -> &'static str {
        self.key.description()
    }
}

/// What `set` or `reset` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Applied {
        /// This write changed a key that applies after a restart.
        restart_key_changed: bool,
    },
    Refused(SettingsFault),
}

#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("cannot locate the settings")]
    Home(#[source] UserHomeError),

    #[error(transparent)]
    Settings(#[from] SettingsError),

    #[error("cannot read the running daemon's settings status")]
    Status(#[source] crate::ipc::ClientError),
}

fn home() -> Result<PathBuf, CommandError> {
    user_home::resolve().map_err(CommandError::Home)
}

/// Every key, or the one `key` names.
pub fn show(key: Option<&str>) -> Result<Vec<Row>, CommandError> {
    let pending = restart_required_keys()?;
    show_with(&home()?, &pending, key)
}

/// What the running daemon waits on a restart for; nothing when no daemon
/// runs.
fn restart_required_keys() -> Result<Vec<String>, CommandError> {
    let Ok(paths) = crate::runtime::PreparedRuntimePaths::for_current_user() else {
        return Ok(Vec::new());
    };
    let Ok(session) = crate::daemon::DaemonSession::connect_at(&paths) else {
        return Ok(Vec::new());
    };
    let status = session.settings_status().map_err(CommandError::Status)?;
    Ok(status.restart_required)
}

fn show_with(home: &Path, pending: &[String], key: Option<&str>) -> Result<Vec<Row>, CommandError> {
    let settings = super::load(home)?;
    let values = settings.values();
    let keys = match key {
        Some(path) => {
            vec![Key::from_path(path).ok_or_else(|| SettingsError::UnknownKey(path.to_owned()))?]
        }
        None => Key::ALL.to_vec(),
    };
    Ok(keys
        .into_iter()
        .map(|key| {
            let written = document::value_at(&values, key);
            let set = !written.is_null();
            let value = if set { written } else { default_value(key) };
            Row {
                key,
                set,
                value,
                restart_required: pending.iter().any(|path| path == key.path()),
            }
        })
        .collect())
}

/// The key's default as JSON, read from its TOML literal.
fn default_value(key: Key) -> serde_json::Value {
    let Some(literal) = key.default() else {
        return serde_json::Value::Null;
    };
    toml_edit::de::from_str::<serde_json::Value>(&format!("value = {literal}"))
        .ok()
        .and_then(|mut table| table.get_mut("value").map(serde_json::Value::take))
        .unwrap_or(serde_json::Value::Null)
}

/// Writes key and value pairs in one change, so keys that only make sense
/// together land together. Values that are not strings read as TOML literals,
/// so `true`, `1.5` and `{ key_code = 49, shift = true }` keep their type.
pub fn set(pairs: &[String]) -> Result<Outcome, CommandError> {
    set_with(&home()?, pairs)
}

/// What a `set` or `reset` path names: a key, or a place inside a keyed
/// table. Never a whole table, which one command could wipe out.
struct Target {
    key: Key,
    below: Vec<String>,
}

impl Target {
    /// Reads the path as a TOML dotted key, so a quoted segment keeps its
    /// dots: `application_overrides."com.apple.Terminal".excluded`.
    fn parse(path: &str) -> Result<Self, Outcome> {
        let unknown = || Outcome::Refused(SettingsError::UnknownKey(path.to_owned()).fault());
        let segments: Vec<String> = toml_edit::Key::parse(path)
            .map_err(|_not_a_path| unknown())?
            .iter()
            .map(|segment| segment.get().to_owned())
            .collect();
        let (key, below) = Key::ALL
            .iter()
            .find_map(|&key| Some((key, below_key(key, &segments)?)))
            .ok_or_else(unknown)?;
        if key.kind().is_table() && below.is_empty() {
            return Err(refused(format!(
                "{key} is a table; name an entry inside it"
            )));
        }
        Ok(Self { key, below })
    }
}

/// The segments left after the key's path or an alias. Only a keyed table has
/// anything below it.
fn below_key(key: Key, segments: &[String]) -> Option<Vec<String>> {
    let names_key = |path: &&str| {
        let names = path.split('.');
        names.clone().count() <= segments.len() && names.zip(segments).all(|(a, b)| a == b)
    };
    let matched = std::iter::once(key.path())
        .chain(key.aliases().iter().copied())
        .find(names_key)?;
    let below = &segments[matched.split('.').count()..];
    (below.is_empty() || key.kind().is_table()).then(|| below.to_vec())
}

fn set_with(home: &Path, pairs: &[String]) -> Result<Outcome, CommandError> {
    if pairs.is_empty() || !pairs.len().is_multiple_of(2) {
        return Ok(refused(
            "give every key a value: KEY VALUE [KEY VALUE ...]".to_owned(),
        ));
    }
    let mut patch = Patch::new();
    for pair in pairs.chunks_exact(2) {
        if let Err(refusal) = add_to_patch(&mut patch, &pair[0], &pair[1]) {
            return Ok(refusal);
        }
    }
    write(home, &patch)
}

fn add_to_patch(patch: &mut Patch, path: &str, raw: &str) -> Result<(), Outcome> {
    let Target { key, below } = Target::parse(path)?;
    let value = literal(key, &below, path, raw).map_err(refused)?;
    patch.set_below(key, below, value);
    Ok(())
}

/// Removes the key's line, so its default applies.
pub fn reset(key: &str) -> Result<Outcome, CommandError> {
    reset_with(&home()?, key)
}

fn reset_with(home: &Path, path: &str) -> Result<Outcome, CommandError> {
    let Target { key, below } = match Target::parse(path) {
        Ok(target) => target,
        Err(refusal) => return Ok(refusal),
    };
    let mut patch = Patch::new();
    patch.reset_below(key, below);
    write(home, &patch)
}

fn write(home: &Path, patch: &Patch) -> Result<Outcome, CommandError> {
    let written = super::modify(&default_path(home), home, None, |_current| {
        Ok::<_, SettingsError>(patch.clone())
    })?;
    Ok(match written {
        Written::Applied(_) => Outcome::Applied {
            restart_key_changed: false,
        },
        Written::AppliedRestartKey(_) => Outcome::Applied {
            restart_key_changed: true,
        },
        Written::Refused(Refusal::FileInvalid(error) | Refusal::Invalid(error)) => {
            Outcome::Refused(error.fault())
        }
        Written::Refused(Refusal::ChangedRecently) => {
            refused("settings.toml kept changing; try again".to_owned())
        }
        // The CLI writes against the file as it stands, with no revision.
        Written::Refused(Refusal::Conflict { .. }) => {
            refused("settings.toml changed during the write; try again".to_owned())
        }
    })
}

fn refused(message: String) -> Outcome {
    Outcome::Refused(SettingsFault {
        message,
        position: None,
    })
}

/// A switch takes on, off, true or false and is stored as true or false.
/// Anything else reads as a TOML literal, and a word that is not one is
/// text.
fn literal(key: Key, below: &[String], path: &str, raw: &str) -> Result<Value, String> {
    if is_switch(key, below) {
        return match raw {
            "on" | "true" => Ok(Value::from(true)),
            "off" | "false" => Ok(Value::from(false)),
            _ => Err(format!("{path} takes on, off, true or false")),
        };
    }
    Ok(raw
        .parse::<Value>()
        .unwrap_or_else(|_not_toml| Value::from(raw)))
}

fn is_switch(key: Key, below: &[String]) -> bool {
    let below: Vec<&str> = below.iter().map(String::as_str).collect();
    match key.kind() {
        Kind::Bool => true,
        Kind::Prompts => matches!(
            below.as_slice(),
            [_, "enabled"] | [_, "chord", "control" | "option" | "shift" | "command"]
        ),
        Kind::Overrides => matches!(below.as_slice(), [_, "excluded"]),
        Kind::Integer
        | Kind::Number
        | Kind::Text
        | Kind::Path
        | Kind::PathList
        | Kind::Choice(_)
        | Kind::ShortcutEntry
        | Kind::Models => false,
    }
}

/// Makes sure the file exists, for an editor to open.
pub fn create() -> Result<PathBuf, CommandError> {
    let home = home()?;
    super::write::create(&default_path(&home))?;
    Ok(super::write::resolve_symlinks(&default_path(&home))?)
}

/// The fault in the file, or `None` when it is valid. Never writes.
pub fn validate() -> Result<Option<SettingsFault>, CommandError> {
    validate_with(&home()?)
}

fn validate_with(home: &Path) -> Result<Option<SettingsFault>, CommandError> {
    match super::load(home) {
        Ok(_settings) => Ok(None),
        Err(error) if error.is_file_fault() => Ok(Some(error.fault())),
        Err(error) => Err(error.into()),
    }
}

/// The file the settings live in, past any symlink.
pub fn path() -> Result<PathBuf, CommandError> {
    Ok(super::write::resolve_symlinks(&default_path(&home()?))?)
}

/// One line for commands to print while the file is invalid.
pub fn banner() -> Option<String> {
    let fault = validate().ok().flatten()?;
    let place = fault
        .position
        .map_or_else(String::new, |position| format!(" at {position}"));
    Some(format!(
        "warning: settings.toml is invalid{place}; Aria keeps the last valid settings. Run `aria settings check`."
    ))
}

/// A JSON Schema for settings.toml, for editors: one property per key,
/// nested by table, each with its kind, default and description.
pub fn schema() -> serde_json::Value {
    let mut root = serde_json::Map::new();
    for key in Key::ALL {
        let mut property = kind_schema(key.kind());
        if let Some(object) = property.as_object_mut() {
            object.insert("description".to_owned(), key.description().into());
            let default = default_value(*key);
            if !default.is_null() {
                object.insert("default".to_owned(), default);
            }
        }
        insert_property(&mut root, &key.segments().collect::<Vec<_>>(), property);
    }
    let mut schema = table_schema(root);
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "$schema".to_owned(),
            "http://json-schema.org/draft-07/schema#".into(),
        );
        object.insert("title".to_owned(), super::write::SCHEMA_TITLE.into());
    }
    schema
}

fn insert_property(
    table: &mut serde_json::Map<String, serde_json::Value>,
    segments: &[&str],
    property: serde_json::Value,
) {
    let Some((name, rest)) = segments.split_first() else {
        return;
    };
    if rest.is_empty() {
        table.insert((*name).to_owned(), property);
        return;
    }
    let child = table
        .entry((*name).to_owned())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(child) = child.as_object_mut() {
        insert_property(child, rest, property);
    }
}

/// Tables are built as maps of their properties, then wrapped here.
fn table_schema(properties: serde_json::Map<String, serde_json::Value>) -> serde_json::Value {
    let properties: serde_json::Map<String, serde_json::Value> = properties
        .into_iter()
        .map(|(name, value)| {
            let is_property = value.get("description").is_some();
            let value = match value {
                serde_json::Value::Object(table) if !is_property => table_schema(table),
                value => value,
            };
            (name, value)
        })
        .collect();
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": properties,
    })
}

fn chord_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["key_code"],
        "properties": {
            "key_code": { "type": "integer", "minimum": 0 },
            "control": { "type": "boolean", "default": false },
            "option": { "type": "boolean", "default": false },
            "shift": { "type": "boolean", "default": false },
            "command": { "type": "boolean", "default": false },
        },
    })
}

fn kind_schema(kind: Kind) -> serde_json::Value {
    match kind {
        Kind::Bool => serde_json::json!({ "type": "boolean" }),
        Kind::Integer => serde_json::json!({ "type": "integer" }),
        Kind::Number => serde_json::json!({ "type": "number" }),
        Kind::Text | Kind::Path => serde_json::json!({ "type": "string" }),
        Kind::PathList => serde_json::json!({ "type": "array", "items": { "type": "string" } }),
        Kind::Choice(words) => serde_json::json!({ "type": "string", "enum": words }),
        Kind::ShortcutEntry => serde_json::json!({
            "oneOf": [
                { "type": "string", "enum": ["off", "shift", "command"] },
                chord_schema(),
            ],
        }),
        Kind::Models => serde_json::json!({
            "type": "object",
            "additionalProperties": { "type": "string" },
        }),
        Kind::Prompts => serde_json::json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "kind", "file"],
                "properties": {
                    "name": { "type": "string" },
                    "kind": { "type": "string", "enum": ["rewrite", "compose"] },
                    "file": { "type": "string" },
                    "enabled": { "type": "boolean", "default": true },
                    "chord": chord_schema(),
                },
            },
        }),
        Kind::Overrides => serde_json::json!({
            "type": "object",
            "additionalProperties": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "output_mode": {
                        "type": "string",
                        "enum": OUTPUT_MODES,
                    },
                    "delivery_mode": { "type": "string", "enum": DELIVERY_MODES },
                    "excluded": { "type": "boolean", "default": false },
                },
            },
        }),
    }
}

/// Every default, as one file. It must load, and must read back as the
/// same values code uses when a key is absent.
#[cfg(test)]
fn defaults_text() -> String {
    let mut document = toml_edit::DocumentMut::new();
    for key in Key::ALL {
        let Some(literal) = key.default() else {
            continue;
        };
        let value: Value = literal.parse().expect("a default is a TOML literal");
        let segments: Vec<&str> = key.segments().collect();
        let table = document::table_at(document.as_table_mut(), segments.iter().copied())
            .expect("a default's table");
        table.insert(segments[segments.len() - 1], toml_edit::Item::Value(value));
    }
    document.to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::config::TtsConfig;
    use crate::config::{ChatConfig, DictationConfig, ListenConfig, MemoryConfig, SpeakerConfig};
    use crate::settings::Loaded;

    fn home_with(text: &str) -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("a temp home");
        fs::create_dir_all(home.path().join(".aria")).expect("the settings directory");
        fs::write(default_path(home.path()), text).expect("written");
        home
    }

    fn set(home: &Path, key: &str, value: &str) -> Outcome {
        set_with(home, &[key.to_owned(), value.to_owned()]).expect("runs")
    }

    fn file(home: &Path) -> String {
        fs::read_to_string(default_path(home)).expect("read")
    }

    #[test]
    fn a_set_is_applied_and_keeps_every_comment() {
        let home = home_with("# mine\n[memory]\n# remember?\nenabled = false # for now\n");

        let outcome = set(home.path(), "memory.enabled", "on");

        assert_eq!(
            outcome,
            Outcome::Applied {
                restart_key_changed: false
            }
        );
        assert_eq!(
            file(home.path()),
            "# mine\n[memory]\n# remember?\nenabled = true # for now\n"
        );
    }

    #[test]
    fn a_restart_key_says_so() {
        let home = home_with("");
        assert_eq!(
            set(home.path(), "phone.port", "9999"),
            Outcome::Applied {
                restart_key_changed: true
            }
        );
        assert!(file(home.path()).contains("port = 9999"));
    }

    #[test]
    fn values_keep_their_type_and_words_are_text() {
        let home = home_with("");
        set(home.path(), "tts.rate", "1.5");
        set(home.path(), "tts.backend", "kokoro");
        set(
            home.path(),
            "shortcuts.ask",
            "{ key_code = 49, shift = true }",
        );
        set(home.path(), "wake.enabled", "off");
        let text = file(home.path());
        assert!(text.contains("rate = 1.5"), "{text}");
        assert!(text.contains("backend = \"kokoro\""), "{text}");
        assert!(
            text.contains("ask = { key_code = 49, shift = true }"),
            "{text}"
        );
        assert!(text.contains("enabled = false"), "{text}");
    }

    #[test]
    fn a_bad_value_is_refused_with_its_place_and_nothing_written() {
        let home = home_with("[tts]\nrate = 1.0\n");

        let Outcome::Refused(fault) = set(home.path(), "tts.rate", "9.0") else {
            panic!("a wild rate is refused");
        };

        assert!(fault.message.contains("tts.rate"), "{}", fault.message);
        assert!(fault.position.is_some());
        assert_eq!(file(home.path()), "[tts]\nrate = 1.0\n");
        assert!(matches!(
            set(home.path(), "memory.enabled", "maybe"),
            Outcome::Refused(_)
        ));
    }

    #[test]
    fn an_invalid_file_refuses_the_write_with_the_files_place() {
        let home = home_with("[memory]\nenabled = maybe\n");

        let Outcome::Refused(fault) = set(home.path(), "tts.rate", "1.5") else {
            panic!("an invalid file is never written over");
        };

        assert_eq!(
            fault.position.map(|position| position.line),
            Some(2),
            "{}",
            fault.message
        );
    }

    #[test]
    fn keys_that_need_each_other_land_in_one_set() {
        let home = home_with("");
        let pairs = [
            "stt.backend",
            "server",
            "stt.server",
            "https://example.test",
        ]
        .map(str::to_owned);

        let outcome = set_with(home.path(), &pairs).expect("runs");

        assert!(!matches!(outcome, Outcome::Refused { .. }), "{outcome:?}");
        assert!(file(home.path()).contains("backend = \"server\""));
    }

    #[test]
    fn one_refused_pair_writes_nothing() {
        let home = home_with("");
        let pairs = ["memory.enabled", "on", "nothing.here", "1"].map(str::to_owned);

        let outcome = set_with(home.path(), &pairs).expect("runs");

        assert!(matches!(outcome, Outcome::Refused { .. }), "{outcome:?}");
        assert_eq!(file(home.path()), "");
    }

    #[test]
    fn a_whole_table_and_an_unknown_key_are_refused() {
        let home = home_with("");
        for (key, value) in [
            ("prompts", "{}"),
            ("assistant.models", "{}"),
            ("chat.nothing", "1"),
        ] {
            assert!(
                matches!(set(home.path(), key, value), Outcome::Refused(_)),
                "{key}"
            );
        }
        assert_eq!(file(home.path()), "");
    }

    #[test]
    fn a_reset_refuses_a_whole_table_and_keeps_it() {
        let text = "[assistant.models]\ncodex = \"mine\"\n";
        let home = home_with(text);

        let outcome = reset_with(home.path(), "assistant.models").expect("runs");

        assert!(matches!(outcome, Outcome::Refused(_)));
        assert_eq!(file(home.path()), text);
    }

    #[test]
    fn a_model_entry_is_set_and_reset_by_path() {
        let home = home_with("# mine\n[assistant.models]\nclaude-code = \"opus\" # keep\n");

        let outcome = set(home.path(), "assistant.models.codex", "gpt");

        assert!(matches!(outcome, Outcome::Applied { .. }));
        assert_eq!(
            file(home.path()),
            "# mine\n[assistant.models]\nclaude-code = \"opus\" # keep\ncodex = \"gpt\"\n"
        );
        reset_with(home.path(), "assistant.models.codex").expect("runs");
        assert_eq!(
            file(home.path()),
            "# mine\n[assistant.models]\nclaude-code = \"opus\" # keep\n"
        );
    }

    #[test]
    fn a_quoted_segment_keeps_the_dots_of_a_bundle_id() {
        let home = home_with("");

        let outcome = set(
            home.path(),
            "application_overrides.\"com.apple.Terminal\".excluded",
            "on",
        );

        assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome:?}");
        assert_eq!(
            file(home.path()),
            "[application_overrides.\"com.apple.Terminal\"]\nexcluded = true\n"
        );
        reset_with(
            home.path(),
            "application_overrides.\"com.apple.Terminal\".excluded",
        )
        .expect("runs");
        assert!(!file(home.path()).contains("excluded"));
    }

    #[test]
    fn a_prompt_field_takes_a_switch_and_keeps_its_place() {
        let prompts = |enabled: &str| {
            format!(
                "[prompts.first]\nname = \"First\"\nkind = \"compose\"\nfile = \"/a\"\nenabled = {enabled}\n\n\
                 [prompts.second]\nname = \"Second\"\nkind = \"compose\"\nfile = \"/b\"\n"
            )
        };
        let home = home_with(&prompts("true"));

        let outcome = set(home.path(), "prompts.first.enabled", "off");

        assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome:?}");
        assert_eq!(file(home.path()), prompts("false"));
        assert!(matches!(
            set(home.path(), "prompts.first.enabled", "maybe"),
            Outcome::Refused(_)
        ));
    }

    #[test]
    fn a_write_that_breaks_an_entry_is_refused() {
        let home = home_with("");
        for (path, value) in [
            ("assistant.models.gemini", "x"),
            ("assistant.models.codex.deeper", "x"),
            ("prompts.lonely.enabled", "on"),
            ("memory.enabled.deeper", "on"),
            ("application_overrides.\"unclosed", "on"),
        ] {
            assert!(
                matches!(set(home.path(), path, value), Outcome::Refused(_)),
                "{path}"
            );
        }
        assert_eq!(file(home.path()), "");
    }

    #[test]
    fn cleanup_turns_off_with_its_model_table_in_place() {
        let table = "[dictation.cleanup_llm]\nendpoint = \"http://127.0.0.1:1/v1/chat/completions\"\nmodel = \"m\"\n";
        let home = home_with(&format!("[dictation]\ncleanup = true\n\n{table}"));

        let outcome = set(home.path(), "dictation.cleanup", "off");

        assert!(matches!(outcome, Outcome::Applied { .. }), "{outcome:?}");
        assert!(file(home.path()).contains(table));
    }

    #[test]
    fn a_reset_removes_the_line() {
        let home = home_with("[memory]\nenabled = true\nextract_model = \"opus\"\n");

        reset_with(home.path(), "memory.enabled").expect("runs");

        assert_eq!(file(home.path()), "[memory]\nextract_model = \"opus\"\n");
    }

    #[test]
    fn show_tells_a_written_value_from_a_default() {
        let home = home_with("[memory]\nenabled = true\n");

        let rows = show_with(home.path(), &["phone.port".to_owned()], None).expect("runs");
        let row = |key: Key| rows.iter().find(|row| row.key == key).expect("a row");

        assert_eq!(rows.len(), Key::ALL.len());
        assert!(row(Key::MemoryEnabled).set);
        assert_eq!(row(Key::MemoryEnabled).value, serde_json::json!(true));
        assert!(!row(Key::PhonePort).set);
        assert_eq!(row(Key::PhonePort).value, serde_json::json!(9850));
        assert!(row(Key::PhonePort).restart_required);
        assert_eq!(row(Key::TtsKokoroVoice).value, serde_json::Value::Null);
    }

    /// The check and the banner read a file that isn't text.
    #[test]
    fn validate_reports_a_file_that_is_not_text() {
        let home = home_with("");
        fs::write(default_path(home.path()), [0xff, 0xfe]).unwrap();

        let fault = validate_with(home.path()).expect("runs").expect("a fault");

        assert_eq!(fault.position, None);
    }

    #[test]
    fn validate_reports_the_place_without_writing() {
        let home = home_with("[transcription\n");
        let fault = validate_with(home.path()).expect("runs").expect("a fault");
        assert_eq!(fault.position.map(|position| position.line), Some(1));
        assert_eq!(validate_with(home_with("").path()).expect("runs"), None);
    }

    #[test]
    fn the_defaults_load_and_match_what_code_does_without_them() {
        let home = Path::new("/home/user");
        let defaults = Loaded::parse(&defaults_text(), home)
            .expect("every default is valid")
            .settings;
        let empty = crate::settings::SettingsFile::default();
        let home_dir = home.to_path_buf();

        let mut settings = crate::settings::Settings::from_file(&defaults).expect("valid");
        assert_eq!(
            settings.assistant_id.take(),
            Some(crate::settings::Settings::default().resolve_assistant_id())
        );
        assert_eq!(crate::settings::Settings::default(), settings);
        assert_eq!(
            DictationConfig::load(&defaults, &home_dir).unwrap(),
            DictationConfig::load(&empty, &home_dir).unwrap()
        );
        assert_eq!(
            TtsConfig::load(&defaults.tts, &home_dir).unwrap(),
            TtsConfig::load(&empty.tts, &home_dir).unwrap()
        );
        assert_eq!(
            ChatConfig::load(&defaults).unwrap(),
            ChatConfig::load(&empty).unwrap()
        );
        assert_eq!(
            MemoryConfig::load(&defaults.memory, &home_dir),
            MemoryConfig::load(&empty.memory, &home_dir)
        );
        assert_eq!(
            SpeakerConfig::load(&defaults.speaker, &home_dir),
            SpeakerConfig::load(&empty.speaker, &home_dir)
        );
        assert_eq!(
            ListenConfig::load(&defaults, &home_dir).unwrap(),
            ListenConfig::load(&empty, &home_dir).unwrap()
        );
    }

    #[test]
    fn the_schema_has_every_key_once_with_its_description() {
        let schema = schema();
        for key in Key::ALL {
            let mut node = &schema;
            for segment in key.segments() {
                node = &node["properties"][segment];
            }
            assert_eq!(node["description"], key.description(), "{key}");
        }
        assert_eq!(
            schema["properties"]["tts"]["properties"]["backend"]["enum"],
            serde_json::json!(["system", "kokoro", "qwen"])
        );
    }
}
