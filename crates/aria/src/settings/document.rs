//! Reading and parsing the file: the size cap, the torn-read guard, the typed
//! `SettingsFile` shape, alias moves, `~` expansion, validation and the revision.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use aria_kit::synthesis::Voice;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item, Table, TableLike};

use super::keys::{LISTEN_BACKENDS, STT_BACKENDS, WAKE_BACKENDS};
use super::{InvalidValue, Key, MAX_SETTINGS_BYTES, ParseError, SettingsError};
use crate::chat::PromptKind;
use crate::config::{
    DEFAULT_OVERALL_TIMEOUT, DEFAULT_RECORDING_TIMEOUT, ServerEndpoint, backend, tts_backend,
    wake_backend,
};
use crate::delivery::OutputMode;
use crate::dictation::{DeliveryMode, MAX_DICTATION_TIMEOUT};
use crate::hotkey::AriaKey;
use crate::hotkey::Combo;
use crate::settings::{ApplicationOverrides, Chord, Modifiers, Prompts, Settings, Shortcut};

/// What a reader saw of the file's identity. Two equal stamps around a read
/// say the bytes between them were one version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    pub(super) len: u64,
    pub(super) modified: SystemTime,
    pub(super) inode: u64,
}

impl Stamp {
    pub(super) fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            inode: metadata.ino(),
        }
    }

    pub(super) fn of(path: &Path) -> Result<Option<Self>, SettingsError> {
        match fs::metadata(path) {
            Ok(metadata) => Ok(Some(Self::from_metadata(&metadata))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SettingsError::Io {
                action: "inspect",
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

pub(super) struct Text {
    pub(super) raw: String,
    pub(super) stamp: Stamp,
}

/// The file's text, or `None` when there is no file. An editor writing in
/// place can leave a prefix that parses, so the stamp is taken before and
/// after, and a mismatch is retried once.
pub(super) fn read_text(path: &Path) -> Result<Option<Text>, SettingsError> {
    read_text_with(path, Stamp::of)
}

fn read_text_with(
    path: &Path,
    mut stamp: impl FnMut(&Path) -> Result<Option<Stamp>, SettingsError>,
) -> Result<Option<Text>, SettingsError> {
    for _attempt in 0..2 {
        let Some(before) = stamp(path)? else {
            return Ok(None);
        };
        let raw = read_capped(path)?;
        if stamp(path)? == Some(before) {
            return Ok(Some(Text { raw, stamp: before }));
        }
    }
    Err(SettingsError::TornRead {
        path: path.to_path_buf(),
    })
}

fn read_capped(path: &Path) -> Result<String, SettingsError> {
    let io_error = |action| {
        move |source| SettingsError::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    };
    // Opening a FIFO would block until something writes to it.
    if !fs::metadata(path).map_err(io_error("inspect"))?.is_file() {
        return Err(SettingsError::NotAFile {
            path: path.to_path_buf(),
        });
    }
    let file = File::open(path).map_err(io_error("open"))?;
    let mut bytes = Vec::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error("read"))?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(SettingsError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    String::from_utf8(bytes).map_err(|_error| SettingsError::InvalidUtf8 {
        path: path.to_path_buf(),
    })
}

/// A hash of the parsed values. A comment edit changes nothing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Revision(u64);

impl Revision {
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

/// A revision a client read and sends back with its patch.
impl From<u64> for Revision {
    fn from(hash: u64) -> Self {
        Self(hash)
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:016x}", self.0)
    }
}

/// One valid file: the editable text, the values it holds, and their hash.
#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    pub(super) document: DocumentMut,
    pub(crate) settings: SettingsFile,
    pub(crate) revision: Revision,
    /// The file's stamp as the write path saw it, under the lock. `None`
    /// until the write path sets it.
    pub(super) stamp: Option<Stamp>,
}

impl Loaded {
    /// Parses, moves aliases to their current names, expands `~`, and
    /// validates. A missing file reads as an empty one.
    pub(crate) fn parse(text: &str, home: &Path) -> Result<Self, SettingsError> {
        let aliases: Vec<(Key, &str)> = Key::ALL
            .iter()
            .flat_map(|key| key.aliases().iter().map(|alias| (*key, *alias)))
            .collect();
        Self::parse_with(text, home, &aliases)
    }

    fn parse_with(text: &str, home: &Path, aliases: &[(Key, &str)]) -> Result<Self, SettingsError> {
        let mut document = DocumentMut::from_str(text)
            .map_err(|error| parse_error(error.message(), error.span(), text))?;
        let mut settings = if move_aliases(&mut document, aliases)? {
            let moved = document.to_string();
            toml_edit::de::from_str::<SettingsFile>(&moved).map_err(|error| {
                let span = error.span().and_then(|span| line_in(&moved, span, text));
                parse_error(&with_version_hint(error.message()), span, text)
            })?
        } else {
            toml_edit::de::from_str::<SettingsFile>(text).map_err(|error| {
                parse_error(&with_version_hint(error.message()), error.span(), text)
            })?
        };
        settings
            .expand_paths(home)
            .and_then(|()| settings.validate())
            .map_err(|error| locate(error, text))?;
        let revision = settings.revision();
        Ok(Self {
            document,
            settings,
            revision,
            stamp: None,
        })
    }
}

/// The line of `text` that the span's line in `moved` came from. A line that
/// appears twice has no one answer, so it reports no place at all.
fn line_in(
    moved: &str,
    span: std::ops::Range<usize>,
    text: &str,
) -> Option<std::ops::Range<usize>> {
    let start = moved[..span.start].rfind('\n').map_or(0, |at| at + 1);
    let line = moved[start..].lines().next()?.trim();
    let mut offset = 0;
    let mut found = None;
    for candidate in text.split_inclusive('\n') {
        if candidate.trim() == line {
            if found.is_some() {
                return None;
            }
            found = Some(offset..offset + candidate.trim_end().len());
        }
        offset += candidate.len();
    }
    found
}

/// A key this build doesn't know may come from a newer Aria, which a downgrade
/// or an older CLI meets as a file it can't load.
fn with_version_hint(message: &str) -> String {
    if !message.starts_with("unknown field") {
        return message.to_owned();
    }
    let version = env!("CARGO_PKG_VERSION");
    format!("{message}; a newer Aria may have written it, and this is {version}")
}

fn parse_error(message: &str, span: Option<std::ops::Range<usize>>, text: &str) -> SettingsError {
    SettingsError::Parse(ParseError::new(message, span, text))
}

/// Moves every value found under an alias to the key's current path, keeping
/// the comment above it. The next write then persists the rename.
fn move_aliases(
    document: &mut DocumentMut,
    aliases: &[(Key, &str)],
) -> Result<bool, SettingsError> {
    let mut moved = false;
    for (key, alias) in aliases {
        if lookup(document.as_table(), alias).is_none() {
            continue;
        }
        if lookup(document.as_table(), key.path()).is_some() {
            return Err(SettingsError::KeyAndAlias {
                key: *key,
                alias: (*alias).to_owned(),
            });
        }
        let Some((old_key, item)) = take(document.as_table_mut(), alias) else {
            continue;
        };
        let Some(table) = table_at(document.as_table_mut(), key.segments()) else {
            continue;
        };
        let name = key.path().rsplit('.').next().unwrap_or_default();
        table.insert(name, item);
        if let Some(mut new_key) = table.key_mut(name) {
            *new_key.leaf_decor_mut() = old_key.leaf_decor().clone();
        }
        moved = true;
    }
    Ok(moved)
}

/// Removes the value at `path` with its key, then the tables above it that
/// held nothing else. A table with comments above it stays, for their sake.
fn take(root: &mut Table, path: &str) -> Option<(toml_edit::Key, Item)> {
    let (parents, name) = path.rsplit_once('.').unwrap_or(("", path));
    let table: &mut dyn TableLike = if parents.is_empty() {
        root
    } else {
        existing_table_at(root, parents.split('.'))?
    };
    let old_key = table.get_key_value(name)?.0.clone();
    let item = table.remove(name)?;
    let mut parents: Vec<&str> = parents
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect();
    while let Some(name) = parents.pop() {
        let Some(table) = existing_table_at(root, parents.iter().copied()) else {
            break;
        };
        if !table.get(name).is_some_and(is_empty_without_comments) {
            break;
        }
        table.remove(name);
    }
    Some((old_key, item))
}

fn is_empty_without_comments(item: &Item) -> bool {
    let has_comments = item
        .as_table()
        .and_then(|table| table.decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .is_some_and(|prefix| prefix.contains('#'));
    !has_comments && item.as_table_like().is_some_and(TableLike::is_empty)
}

pub(super) fn lookup<'a>(root: &'a Table, path: &str) -> Option<&'a Item> {
    let mut item: &Item = root.get(path.split('.').next()?)?;
    for segment in path.split('.').skip(1) {
        item = item.as_table_like()?.get(segment)?;
    }
    Some(item)
}

pub(super) fn existing_table_at<'a, 's>(
    root: &'a mut Table,
    segments: impl Iterator<Item = &'s str>,
) -> Option<&'a mut dyn TableLike> {
    let mut table: &mut dyn TableLike = root;
    for segment in segments {
        table = table.get_mut(segment)?.as_table_like_mut()?;
    }
    Some(table)
}

/// The table that holds the key's last segment, created on the way when
/// missing. `None` when something on the path isn't a table.
pub(super) fn table_at<'a, 's>(
    root: &'a mut Table,
    segments: impl Iterator<Item = &'s str>,
) -> Option<&'a mut dyn TableLike> {
    let segments: Vec<&str> = segments.collect();
    let (_name, parents) = segments.split_last()?;
    let mut table: &mut dyn TableLike = root;
    for segment in parents {
        table = table
            .entry(segment)
            .or_insert_with(|| {
                // A table that only holds tables needs no header of its own.
                let mut created = Table::new();
                created.set_implicit(true);
                Item::Table(created)
            })
            .as_table_like_mut()?;
    }
    Some(table)
}

fn is_default<T: Default + PartialEq>(section: &T) -> bool {
    *section == T::default()
}

/// The file's shape. Every leaf is optional; absent means default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct SettingsFile {
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) dictation: Dictation,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) history: History,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) listen: Listen,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) assistant: Assistant,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) memory: Memory,
    #[serde(skip_serializing_if = "Keyed::is_empty")]
    pub(crate) prompts: Keyed<PromptEntry>,
    #[serde(skip_serializing_if = "Keyed::is_empty")]
    pub(crate) application_overrides: Keyed<ApplicationOverrideEntry>,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) shortcuts: Shortcuts,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) wake: Wake,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) phone: Phone,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) delivery: Delivery,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) stt: Stt,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) tts: Tts,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) speaker: Speaker,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) capture: Capture,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Dictation {
    pub(crate) output_mode: Option<OutputModeName>,
    pub(crate) delivery_mode: Option<DeliveryModeName>,
    pub(crate) silence_duration_ms: Option<u64>,
    pub(crate) auto_stop_ms: Option<u64>,
    pub(crate) max_recording_secs: Option<f64>,
    pub(crate) timeout_secs: Option<f64>,
    pub(crate) mute_sound: Option<bool>,
    pub(crate) cleanup: Option<bool>,
    pub(crate) backtrack: Option<bool>,
    pub(crate) own_voice_only: Option<bool>,
    pub(crate) cleanup_llm: Option<CleanupLlm>,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) vocabulary: Vocabulary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CleanupLlm {
    pub(crate) endpoint: String,
    pub(crate) model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Vocabulary {
    pub(crate) enabled: Option<bool>,
    pub(crate) dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct History {
    pub(crate) enabled: Option<bool>,
    pub(crate) keep_audio: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Listen {
    pub(crate) backend: Option<String>,
    pub(crate) model: Option<PathBuf>,
    pub(crate) device: Option<DeviceName>,
    pub(crate) transcripts_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Assistant {
    pub(crate) id: Option<String>,
    pub(crate) workdir: Option<PathBuf>,
    pub(crate) project_roots: Option<Vec<PathBuf>>,
    pub(crate) timeout_secs: Option<f64>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) models: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Memory {
    pub(crate) enabled: Option<bool>,
    pub(crate) extract_model: Option<String>,
    pub(crate) max_facts_per_session: Option<u32>,
    pub(crate) dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptEntry {
    pub(crate) name: String,
    pub(crate) kind: PromptKindName,
    pub(crate) file: PathBuf,
    #[serde(default = "enabled")]
    pub(crate) enabled: bool,
    pub(crate) chord: Option<ChordEntry>,
}

fn enabled() -> bool {
    true
}

/// A table keyed by id, so a write names one entry by a path that stays put
/// while others are added or removed. Entries keep file order, which is the
/// prompt list's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Keyed<T>(pub(crate) Vec<(String, T)>);

impl<T> Default for Keyed<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> Keyed<T> {
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, &T)> {
        self.0.iter().map(|(id, entry)| (id.as_str(), entry))
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut T> {
        self.0.iter_mut().map(|(_id, entry)| entry)
    }
}

impl<T> FromIterator<(String, T)> for Keyed<T> {
    fn from_iter<I: IntoIterator<Item = (String, T)>>(entries: I) -> Self {
        Self(entries.into_iter().collect())
    }
}

impl<T: Serialize> Serialize for Keyed<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Keyed<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyedVisitor<T>(std::marker::PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for KeyedVisitor<T> {
            type Value = Keyed<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a table of entries keyed by id")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Keyed<T>, A::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(Keyed(entries))
            }
        }

        deserializer.deserialize_map(KeyedVisitor(std::marker::PhantomData))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplicationOverrideEntry {
    pub(crate) output_mode: Option<OutputModeName>,
    pub(crate) delivery_mode: Option<DeliveryModeName>,
    #[serde(default)]
    pub(crate) excluded: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Shortcuts {
    pub(crate) aria_key: Option<AriaKeyName>,
    pub(crate) ask: Option<ShortcutEntry>,
    pub(crate) prompt_list: Option<ShortcutEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Wake {
    pub(crate) enabled: Option<bool>,
    pub(crate) backend: Option<String>,
    pub(crate) model: Option<PathBuf>,
    pub(crate) threshold: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Phone {
    pub(crate) enabled: Option<bool>,
    pub(crate) port: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Delivery {
    pub(crate) timeout_secs: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Stt {
    pub(crate) backend: Option<String>,
    pub(crate) language: Option<String>,
    /// A path for every local engine but parakeet-mlx, whose model is a
    /// Hugging Face repo ID.
    pub(crate) model: Option<String>,
    pub(crate) device: Option<DeviceName>,
    pub(crate) interpreter: Option<PathBuf>,
    pub(crate) server: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Tts {
    pub(crate) backend: Option<String>,
    pub(crate) rate: Option<f64>,
    pub(crate) fallback: Option<TtsFallbackName>,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) system: TtsSystem,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) kokoro: TtsKokoro,
    #[serde(skip_serializing_if = "is_default")]
    pub(crate) qwen: TtsQwen,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TtsSystem {
    pub(crate) voice: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TtsKokoro {
    pub(crate) model: Option<PathBuf>,
    pub(crate) voices: Option<PathBuf>,
    pub(crate) voice: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct TtsQwen {
    pub(crate) model: Option<PathBuf>,
    pub(crate) tokenizer: Option<PathBuf>,
    pub(crate) voice: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Speaker {
    pub(crate) model: Option<PathBuf>,
    pub(crate) segmentation_model: Option<PathBuf>,
    pub(crate) profiles_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Capture {
    pub(crate) backend: Option<CaptureBackendName>,
    pub(crate) path: Option<PathBuf>,
    pub(crate) pace: Option<PaceName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputModeName {
    Insert,
    InsertAndSubmit,
    InsertOrCopy,
    Copy,
}

impl From<OutputMode> for OutputModeName {
    fn from(mode: OutputMode) -> Self {
        match mode {
            OutputMode::Insert => Self::Insert,
            OutputMode::InsertAndSubmit => Self::InsertAndSubmit,
            OutputMode::InsertOrCopy => Self::InsertOrCopy,
            OutputMode::Copy => Self::Copy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeliveryModeName {
    Streaming,
    Complete,
}

impl From<OutputModeName> for OutputMode {
    fn from(mode: OutputModeName) -> Self {
        match mode {
            OutputModeName::Insert => Self::Insert,
            OutputModeName::InsertAndSubmit => Self::InsertAndSubmit,
            OutputModeName::InsertOrCopy => Self::InsertOrCopy,
            OutputModeName::Copy => Self::Copy,
        }
    }
}

impl From<DeliveryModeName> for DeliveryMode {
    fn from(mode: DeliveryModeName) -> Self {
        match mode {
            DeliveryModeName::Streaming => Self::Streaming,
            DeliveryModeName::Complete => Self::Complete,
        }
    }
}

impl From<DeliveryMode> for DeliveryModeName {
    fn from(mode: DeliveryMode) -> Self {
        match mode {
            DeliveryMode::Streaming => Self::Streaming,
            DeliveryMode::Complete => Self::Complete,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PromptKindName {
    Rewrite,
    Compose,
}

impl From<PromptKind> for PromptKindName {
    fn from(kind: PromptKind) -> Self {
        match kind {
            PromptKind::Rewrite => Self::Rewrite,
            PromptKind::Compose => Self::Compose,
        }
    }
}

impl From<PromptKindName> for PromptKind {
    fn from(kind: PromptKindName) -> Self {
        match kind {
            PromptKindName::Rewrite => Self::Rewrite,
            PromptKindName::Compose => Self::Compose,
        }
    }
}

/// The Aria key, or `Off`, which runs no listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AriaKeyName {
    RightOption,
    Fn,
    Off,
}

/// `None` is off.
impl From<Option<AriaKey>> for AriaKeyName {
    fn from(key: Option<AriaKey>) -> Self {
        match key {
            Some(AriaKey::RightOption) => Self::RightOption,
            Some(AriaKey::Fn) => Self::Fn,
            None => Self::Off,
        }
    }
}

/// What starts the ask or the prompt list: a word, or a chord table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShortcutEntry {
    Off,
    Shift,
    Command,
    Chord(ChordEntry),
}

const SHORTCUT_WORDS: &[&str] = &["off", "shift", "command"];

impl Serialize for ShortcutEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Off => serializer.serialize_str(SHORTCUT_WORDS[0]),
            Self::Shift => serializer.serialize_str(SHORTCUT_WORDS[1]),
            Self::Command => serializer.serialize_str(SHORTCUT_WORDS[2]),
            Self::Chord(chord) => chord.serialize(serializer),
        }
    }
}

/// Picks the form by its shape, so a fault names the chord's field rather
/// than saying no form matched.
impl<'de> Deserialize<'de> for ShortcutEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ShortcutVisitor;

        impl<'de> Visitor<'de> for ShortcutVisitor {
            type Value = ShortcutEntry;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("off, shift, command, or a chord table")
            }

            fn visit_str<E: de::Error>(self, word: &str) -> Result<ShortcutEntry, E> {
                match word {
                    "off" => Ok(ShortcutEntry::Off),
                    "shift" => Ok(ShortcutEntry::Shift),
                    "command" => Ok(ShortcutEntry::Command),
                    other => Err(E::unknown_variant(other, SHORTCUT_WORDS)),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<ShortcutEntry, A::Error> {
                ChordEntry::deserialize(de::value::MapAccessDeserializer::new(map))
                    .map(ShortcutEntry::Chord)
            }
        }

        deserializer.deserialize_any(ShortcutVisitor)
    }
}

/// A key code with modifiers. An omitted modifier is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChordEntry {
    pub(crate) key_code: u16,
    #[serde(default)]
    pub(crate) control: bool,
    #[serde(default)]
    pub(crate) option: bool,
    #[serde(default)]
    pub(crate) shift: bool,
    #[serde(default)]
    pub(crate) command: bool,
}

impl ChordEntry {
    pub(crate) fn chord(self) -> Result<Chord, InvalidValue> {
        Chord::new(
            self.key_code,
            Modifiers {
                control: self.control,
                option: self.option,
                shift: self.shift,
                command: self.command,
            },
        )
        .map_err(InvalidValue::Chord)
    }
}

impl ShortcutEntry {
    /// `None` is off.
    pub(crate) fn shortcut(self) -> Result<Option<Shortcut>, InvalidValue> {
        match self {
            Self::Off => Ok(None),
            Self::Shift => Ok(Some(Shortcut::Combo(Combo::Shift))),
            Self::Command => Ok(Some(Shortcut::Combo(Combo::Command))),
            Self::Chord(chord) => chord.chord().map(|chord| Some(Shortcut::Chord(chord))),
        }
    }
}

impl From<Option<Shortcut>> for ShortcutEntry {
    fn from(shortcut: Option<Shortcut>) -> Self {
        match shortcut {
            None => Self::Off,
            Some(Shortcut::Combo(Combo::Shift)) => Self::Shift,
            Some(Shortcut::Combo(Combo::Command)) => Self::Command,
            Some(Shortcut::Chord(chord)) => Self::Chord(chord.into()),
        }
    }
}

impl From<&Prompts> for Keyed<PromptEntry> {
    fn from(prompts: &Prompts) -> Self {
        prompts
            .entries()
            .iter()
            .map(|prompt| {
                let entry = PromptEntry {
                    name: prompt.name().to_owned(),
                    kind: prompt.kind().into(),
                    file: prompt.file().to_path_buf(),
                    enabled: prompt.enabled(),
                    chord: prompt.chord().map(ChordEntry::from),
                };
                (prompt.id().as_str().to_owned(), entry)
            })
            .collect()
    }
}

impl From<&ApplicationOverrides> for Keyed<ApplicationOverrideEntry> {
    fn from(overrides: &ApplicationOverrides) -> Self {
        overrides
            .iter()
            .map(|(id, entry)| {
                let crate::settings::ApplicationOverride {
                    excluded,
                    overrides:
                        crate::settings::DictationOverrides {
                            output_mode,
                            delivery_mode,
                        },
                } = *entry;
                let entry = ApplicationOverrideEntry {
                    output_mode: output_mode.map(Into::into),
                    delivery_mode: delivery_mode.map(Into::into),
                    excluded,
                };
                (id.as_str().to_owned(), entry)
            })
            .collect()
    }
}

impl From<Chord> for ChordEntry {
    fn from(chord: Chord) -> Self {
        let modifiers = chord.modifiers();
        Self {
            key_code: chord.key_code(),
            control: modifiers.control,
            option: modifiers.option,
            shift: modifiers.shift,
            command: modifiers.command,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TtsFallbackName {
    None,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeviceName {
    Cpu,
    Gpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaptureBackendName {
    Microphone,
    WavFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PaceName {
    Immediate,
    Realtime,
}

impl SettingsFile {
    /// Expands `~` in every path. Destructured so a new path field is
    /// classified here before it compiles.
    fn expand_paths(&mut self, home: &Path) -> Result<(), SettingsError> {
        let Self {
            dictation,
            history: _,
            listen,
            assistant,
            memory,
            prompts,
            application_overrides: _,
            shortcuts: _,
            wake,
            phone: _,
            delivery: _,
            stt,
            tts,
            speaker,
            capture,
        } = self;
        let expand = |key: Key, path: &mut PathBuf| -> Result<(), SettingsError> {
            *path = expand_home(path, home).map_err(|source| invalid(key, source))?;
            Ok(())
        };
        for (key, path) in [
            (
                Key::DictationVocabularyDir,
                dictation.vocabulary.dir.as_mut(),
            ),
            (Key::ListenModel, listen.model.as_mut()),
            (Key::ListenTranscriptsDir, listen.transcripts_dir.as_mut()),
            (Key::AssistantWorkdir, assistant.workdir.as_mut()),
            (Key::MemoryDir, memory.dir.as_mut()),
            (Key::WakeModel, wake.model.as_mut()),
            (Key::SttInterpreter, stt.interpreter.as_mut()),
            (Key::TtsKokoroModel, tts.kokoro.model.as_mut()),
            (Key::TtsKokoroVoices, tts.kokoro.voices.as_mut()),
            (Key::TtsQwenModel, tts.qwen.model.as_mut()),
            (Key::TtsQwenTokenizer, tts.qwen.tokenizer.as_mut()),
            (Key::SpeakerModel, speaker.model.as_mut()),
            (
                Key::SpeakerSegmentationModel,
                speaker.segmentation_model.as_mut(),
            ),
            (Key::SpeakerProfilesDir, speaker.profiles_dir.as_mut()),
            (Key::CapturePath, capture.path.as_mut()),
        ] {
            if let Some(path) = path {
                expand(key, path)?;
            }
        }
        for root in assistant.project_roots.iter_mut().flatten() {
            expand(Key::AssistantProjectRoots, root)?;
        }
        for prompt in prompts.values_mut() {
            expand(Key::Prompts, &mut prompt.file)?;
        }
        if let Some(model) = &stt.model
            && model.starts_with('~')
        {
            let expanded = expand_home(Path::new(model), home)
                .map_err(|source| invalid(Key::SttModel, source))?;
            stt.model = Some(expanded.to_string_lossy().into_owned());
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), SettingsError> {
        let settings = Settings::from_file(self)?;
        self.validate_dictation()?;
        self.validate_timeouts()?;
        self.validate_listen()?;
        self.validate_assistant()?;
        self.validate_memory()?;
        validate_shortcuts(&settings)?;
        self.validate_wake()?;
        self.validate_stt()?;
        self.validate_tts()?;
        self.validate_paths()?;
        self.validate_capture()?;
        Ok(())
    }

    fn validate_dictation(&self) -> Result<(), SettingsError> {
        let dictation = &self.dictation;
        if let Some(llm) = &dictation.cleanup_llm {
            crate::dictation::cleanup::validate_config(&llm.endpoint, &llm.model).map_err(
                |source| {
                    invalid(
                        Key::DictationCleanupLlmEndpoint,
                        InvalidValue::Cleanup(source),
                    )
                },
            )?;
        }
        Ok(())
    }

    fn validate_timeouts(&self) -> Result<(), SettingsError> {
        let written_recording = self
            .dictation
            .max_recording_secs
            .map(|seconds| duration(seconds, Key::DictationMaxRecordingSecs))
            .transpose()?;
        if let Some(seconds) = self.delivery.timeout_secs {
            duration(seconds, Key::DeliveryTimeoutSecs)?;
        }
        let written_overall = self
            .dictation
            .timeout_secs
            .map(|seconds| duration(seconds, Key::DictationTimeoutSecs))
            .transpose()?;
        if let Some(overall) = written_overall
            && overall > MAX_DICTATION_TIMEOUT
        {
            return Err(invalid(
                Key::DictationTimeoutSecs,
                InvalidValue::TooLong {
                    actual: overall,
                    maximum: MAX_DICTATION_TIMEOUT,
                },
            ));
        }
        let recording = written_recording.unwrap_or(DEFAULT_RECORDING_TIMEOUT);
        let overall = written_overall.unwrap_or(DEFAULT_OVERALL_TIMEOUT);
        if recording < overall {
            return Ok(());
        }
        // The fault is reported at a key the file holds, so it has a place.
        Err(if written_recording.is_some() {
            invalid(
                Key::DictationMaxRecordingSecs,
                InvalidValue::RecordingOutlastsOverall { recording, overall },
            )
        } else {
            invalid(
                Key::DictationTimeoutSecs,
                InvalidValue::OverallWithinRecording { recording, overall },
            )
        })
    }

    fn validate_listen(&self) -> Result<(), SettingsError> {
        let listen = &self.listen;
        if let Some(backend_name) = &listen.backend {
            supported(backend_name, LISTEN_BACKENDS, Key::ListenBackend)?;
            if listen.device.is_some() && backend_name != backend::PARAKEET {
                return Err(invalid(
                    Key::ListenDevice,
                    InvalidValue::NotApplicable {
                        backend: backend_name.clone(),
                    },
                ));
            }
        }
        Ok(())
    }

    fn validate_assistant(&self) -> Result<(), SettingsError> {
        let assistant = &self.assistant;
        for model in assistant.models.values() {
            if !crate::chat::is_valid_model_id(model) {
                return Err(invalid(Key::AssistantModels, InvalidValue::ModelId));
            }
        }
        if let Some(workdir) = &assistant.workdir {
            absolute_path(workdir, Key::AssistantWorkdir)?;
        }
        if let Some(seconds) = assistant.timeout_secs {
            let timeout = duration(seconds, Key::AssistantTimeoutSecs)?;
            if timeout > crate::chat::MAX_TIMEOUT {
                return Err(invalid(
                    Key::AssistantTimeoutSecs,
                    InvalidValue::TooLong {
                        actual: timeout,
                        maximum: crate::chat::MAX_TIMEOUT,
                    },
                ));
            }
        }
        Ok(())
    }

    fn validate_memory(&self) -> Result<(), SettingsError> {
        if let Some(model) = &self.memory.extract_model {
            non_empty(model, Key::MemoryExtractModel)?;
        }
        Ok(())
    }

    fn validate_wake(&self) -> Result<(), SettingsError> {
        let wake = &self.wake;
        if let Some(backend) = &wake.backend {
            supported(backend, WAKE_BACKENDS, Key::WakeBackend)?;
        }
        if let Some(threshold) = wake.threshold
            && (!threshold.is_finite() || threshold <= 0.0 || threshold > 1.0)
        {
            return Err(invalid(
                Key::WakeThreshold,
                InvalidValue::Threshold(threshold),
            ));
        }
        if wake.enabled == Some(true) {
            if wake.backend.as_deref() == Some(wake_backend::LIVEKIT) && wake.model.is_none() {
                return Err(invalid(
                    Key::WakeModel,
                    InvalidValue::Required {
                        backend: wake_backend::LIVEKIT.to_owned(),
                    },
                ));
            }
            if matches!(self.capture.backend, Some(CaptureBackendName::WavFile)) {
                return Err(invalid(Key::WakeEnabled, InvalidValue::NeedsMicrophone));
            }
        }
        Ok(())
    }

    fn validate_stt(&self) -> Result<(), SettingsError> {
        let stt = &self.stt;
        let backend_name = stt.backend.as_deref().unwrap_or(backend::DEFAULT);
        supported(backend_name, STT_BACKENDS, Key::SttBackend)?;
        let not_applicable = |key: Key, backend: &str| {
            invalid(
                key,
                InvalidValue::NotApplicable {
                    backend: backend.to_owned(),
                },
            )
        };
        if backend_name == backend::SERVER {
            if stt.model.is_some() {
                return Err(not_applicable(Key::SttModel, backend::SERVER));
            }
            if stt.device.is_some() {
                return Err(not_applicable(Key::SttDevice, backend::SERVER));
            }
            if stt.interpreter.is_some() {
                return Err(not_applicable(Key::SttInterpreter, backend::SERVER));
            }
            let server = stt.server.as_deref().ok_or_else(|| {
                invalid(
                    Key::SttServer,
                    InvalidValue::Required {
                        backend: backend::SERVER.to_owned(),
                    },
                )
            })?;
            ServerEndpoint::parse(server)
                .ok_or_else(|| invalid(Key::SttServer, InvalidValue::ServerEndpoint))?;
        } else if stt.server.is_some() {
            return Err(not_applicable(Key::SttServer, backend_name));
        }
        if let Some(model) = &stt.model {
            non_empty(model, Key::SttModel)?;
            // parakeet-mlx takes a Hugging Face repo ID, not a path.
            if backend_name != backend::PARAKEET_MLX {
                absolute_path(Path::new(model), Key::SttModel)?;
            }
        }
        if stt.interpreter.is_some() && backend_name != backend::PARAKEET_MLX {
            return Err(not_applicable(Key::SttInterpreter, backend_name));
        }
        if backend_name == backend::PARAKEET_MLX && stt.interpreter.is_none() {
            return Err(invalid(
                Key::SttInterpreter,
                InvalidValue::Required {
                    backend: backend::PARAKEET_MLX.to_owned(),
                },
            ));
        }
        if stt.device.is_some() && backend_name != backend::PARAKEET {
            return Err(not_applicable(Key::SttDevice, backend_name));
        }
        Ok(())
    }

    fn validate_tts(&self) -> Result<(), SettingsError> {
        let tts = &self.tts;
        let backend = tts.backend.as_deref().unwrap_or(tts_backend::DEFAULT);
        supported(
            backend,
            &[tts_backend::KOKORO, tts_backend::QWEN, tts_backend::SYSTEM],
            Key::TtsBackend,
        )?;
        if tts.fallback.is_some() && backend == tts_backend::SYSTEM {
            return Err(invalid(
                Key::TtsFallback,
                InvalidValue::NotApplicable {
                    backend: tts_backend::SYSTEM.to_owned(),
                },
            ));
        }
        for (key, voice) in [
            (Key::TtsSystemVoice, &tts.system.voice),
            (Key::TtsKokoroVoice, &tts.kokoro.voice),
            (Key::TtsQwenVoice, &tts.qwen.voice),
        ] {
            if let Some(voice) = voice {
                Voice::new(voice.clone())
                    .map_err(|source| invalid(key, InvalidValue::Voice(source)))?;
            }
        }
        Ok(())
    }

    fn validate_paths(&self) -> Result<(), SettingsError> {
        for (key, path) in [
            (Key::ListenModel, &self.listen.model),
            (Key::ListenTranscriptsDir, &self.listen.transcripts_dir),
            (Key::MemoryDir, &self.memory.dir),
            (Key::WakeModel, &self.wake.model),
            (Key::SttInterpreter, &self.stt.interpreter),
            (Key::TtsKokoroModel, &self.tts.kokoro.model),
            (Key::TtsKokoroVoices, &self.tts.kokoro.voices),
            (Key::TtsQwenModel, &self.tts.qwen.model),
            (Key::TtsQwenTokenizer, &self.tts.qwen.tokenizer),
            (Key::SpeakerModel, &self.speaker.model),
            (
                Key::SpeakerSegmentationModel,
                &self.speaker.segmentation_model,
            ),
            (Key::SpeakerProfilesDir, &self.speaker.profiles_dir),
            (Key::CapturePath, &self.capture.path),
        ] {
            if let Some(path) = path {
                absolute_path(path, key)?;
            }
        }
        for root in self.assistant.project_roots.iter().flatten() {
            absolute_path(root, Key::AssistantProjectRoots)?;
        }
        Ok(())
    }

    fn validate_capture(&self) -> Result<(), SettingsError> {
        let capture = &self.capture;
        match capture.backend.unwrap_or(CaptureBackendName::Microphone) {
            CaptureBackendName::Microphone => {
                if capture.pace.is_some() {
                    return Err(invalid(
                        Key::CapturePace,
                        InvalidValue::NotApplicable {
                            backend: "microphone".to_owned(),
                        },
                    ));
                }
            }
            CaptureBackendName::WavFile => {
                if capture.path.is_none() {
                    return Err(invalid(
                        Key::CapturePath,
                        InvalidValue::Required {
                            backend: "wav_file".to_owned(),
                        },
                    ));
                }
            }
        }
        Ok(())
    }

    /// The Aria key, `None` while it is off.
    pub(crate) fn aria_key(&self) -> Option<AriaKey> {
        match self.shortcuts.aria_key {
            None | Some(AriaKeyName::RightOption) => Some(AriaKey::RightOption),
            Some(AriaKeyName::Fn) => Some(AriaKey::Fn),
            Some(AriaKeyName::Off) => None,
        }
    }

    /// The folder project vocabulary reads, `None` while it is off.
    pub(crate) fn project_vocabulary(&self) -> Option<crate::vocabulary::Directory> {
        if self.dictation.vocabulary.enabled != Some(true) {
            return None;
        }
        // Validation already accepted the folder, so it is a directory here.
        self.dictation
            .vocabulary
            .dir
            .clone()
            .and_then(|path| crate::vocabulary::Directory::new(path).ok())
    }

    /// Every value as one JSON tree, for the revision and for per-key diffs.
    pub(crate) fn values(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    pub(crate) fn revision(&self) -> Revision {
        let json = serde_json::to_vec(self).unwrap_or_default();
        let digest = Sha256::digest(&json);
        let mut head = [0u8; 8];
        head.copy_from_slice(&digest[..8]);
        Revision(u64::from_be_bytes(head))
    }
}

/// The keys whose values differ between two files.
pub(crate) fn changed_keys(before: &SettingsFile, after: &SettingsFile) -> Vec<Key> {
    let (before, after) = (before.values(), after.values());
    Key::ALL
        .iter()
        .copied()
        .filter(|key| value_at(&before, *key) != value_at(&after, *key))
        .collect()
}

/// Sets one key in a JSON tree from [`SettingsFile::values`]. `Null` removes it,
/// along with any table left empty, so the key reads as absent.
pub(crate) fn replace_at(values: &mut serde_json::Value, key: Key, value: serde_json::Value) {
    let segments: Vec<&str> = key.segments().collect();
    set_path(values, &segments, value);
}

fn set_path(node: &mut serde_json::Value, segments: &[&str], value: serde_json::Value) {
    let Some((name, rest)) = segments.split_first() else {
        return;
    };
    if !node.is_object() {
        if value.is_null() {
            return;
        }
        *node = serde_json::Value::Object(serde_json::Map::new());
    }
    let Some(object) = node.as_object_mut() else {
        return;
    };
    if rest.is_empty() {
        if value.is_null() {
            object.remove(*name);
        } else {
            object.insert((*name).to_owned(), value);
        }
        return;
    }
    let child = object
        .entry((*name).to_owned())
        .or_insert(serde_json::Value::Null);
    set_path(child, rest, value);
    let empty = child.is_null() || child.as_object().is_some_and(serde_json::Map::is_empty);
    if empty {
        object.remove(*name);
    }
}

/// The value one key holds in a JSON tree from [`SettingsFile::values`]. `Null`
/// when absent.
pub(crate) fn value_at(values: &serde_json::Value, key: Key) -> serde_json::Value {
    let mut current = values;
    for segment in key.segments() {
        match current.get(segment) {
            Some(child) => current = child,
            None => return serde_json::Value::Null,
        }
    }
    current.clone()
}

pub(crate) fn invalid(key: Key, source: InvalidValue) -> SettingsError {
    SettingsError::Invalid {
        key,
        source,
        position: None,
    }
}

/// Puts a value error at the line of its key, which validation of the typed
/// values cannot see.
fn locate(error: SettingsError, text: &str) -> SettingsError {
    let SettingsError::Invalid {
        key,
        source,
        position: None,
    } = error
    else {
        return error;
    };
    // The key's own line, else the table a missing required key belongs in.
    let position = toml_edit::Document::parse(text).ok().and_then(|document| {
        let paths = || std::iter::once(key.path()).chain(key.aliases().iter().copied());
        let at_key = paths().find_map(|path| {
            let (parents, name) = path.rsplit_once('.').unwrap_or(("", path));
            let table: &dyn TableLike = if parents.is_empty() {
                document.as_table()
            } else {
                lookup(document.as_table(), parents)?.as_table_like()?
            };
            let (found, item) = table.get_key_value(name)?;
            found.span().or_else(|| item.span())
        });
        let at_table = || {
            paths().find_map(|path| {
                let (parents, _name) = path.rsplit_once('.')?;
                lookup(document.as_table(), parents)?.span()
            })
        };
        at_key
            .or_else(at_table)
            .map(|span| super::Position::of(text, span.start))
    });
    SettingsError::Invalid {
        key,
        source,
        position,
    }
}

/// Reports the clash at the key the person can still change.
fn validate_shortcuts(settings: &Settings) -> Result<(), SettingsError> {
    if !settings.shares_a_binding() {
        return Ok(());
    }
    let ask = settings.ask_shortcut;
    let key = if ask.is_some() && ask == settings.prompt_list_shortcut {
        Key::ShortcutsPromptList
    } else {
        Key::Prompts
    };
    Err(invalid(key, InvalidValue::SharedBinding))
}

fn supported(value: &str, names: &[&str], key: Key) -> Result<(), SettingsError> {
    if names.contains(&value) {
        return Ok(());
    }
    Err(invalid(
        key,
        InvalidValue::Unsupported {
            value: value.to_owned(),
            supported: listed(names),
        },
    ))
}

/// "a and b", or "a, b, and c".
fn listed(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

fn non_empty(value: &str, key: Key) -> Result<(), SettingsError> {
    if value.trim().is_empty() {
        return Err(invalid(key, InvalidValue::Empty));
    }
    Ok(())
}

/// A relative path would mean something else in each program's folder.
fn absolute_path(path: &Path, key: Key) -> Result<(), SettingsError> {
    if path.as_os_str().is_empty() {
        return Err(invalid(key, InvalidValue::Empty));
    }
    if !path.is_absolute() {
        return Err(invalid(key, InvalidValue::RelativePath));
    }
    Ok(())
}

fn duration(seconds: f64, key: Key) -> Result<Duration, SettingsError> {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(invalid(key, InvalidValue::Duration(seconds)));
    }
    Duration::try_from_secs_f64(seconds)
        .map_err(|_error| invalid(key, InvalidValue::Duration(seconds)))
}

fn expand_home(path: &Path, home: &Path) -> Result<PathBuf, InvalidValue> {
    let Some(text) = path.to_str() else {
        return Ok(path.to_path_buf());
    };
    if text == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(relative) = text.strip_prefix("~/") {
        return Ok(home.join(relative));
    }
    if text.starts_with('~') {
        return Err(InvalidValue::HomeSyntax);
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = include_str!("../../../../settings.example.toml");
    pub(crate) const HOME: &str = "/Users/you";

    pub(crate) fn load(text: &str) -> Loaded {
        Loaded::parse(text, Path::new(HOME)).expect("the text loads")
    }

    fn refused(text: &str, key: Key) {
        let error = Loaded::parse(text, Path::new(HOME)).expect_err("refused");
        assert!(
            matches!(&error, SettingsError::Invalid { key: found, .. } if *found == key),
            "{text}: {error}"
        );
    }

    /// A binding clashes with a default just as with a written one.
    #[test]
    fn a_shortcut_that_takes_the_others_default_is_invalid() {
        for (text, key) in [
            (
                "[shortcuts]\nprompt_list = \"shift\"\n",
                Key::ShortcutsPromptList,
            ),
            ("[shortcuts]\nask = \"command\"\n", Key::ShortcutsPromptList),
        ] {
            let error = Loaded::parse(text, Path::new(HOME)).expect_err(text);
            assert!(
                matches!(
                    error,
                    SettingsError::Invalid { key: found, source: InvalidValue::SharedBinding, .. }
                        if found == key
                ),
                "{error}"
            );
        }
        load("[shortcuts]\nask = \"command\"\nprompt_list = \"shift\"\n");
        load("[shortcuts]\nask = \"off\"\nprompt_list = \"shift\"\n");
    }

    #[test]
    fn the_example_file_loads_with_its_values() {
        let loaded = load(EXAMPLE);
        let settings = &loaded.settings;
        assert_eq!(settings.dictation.output_mode, Some(OutputModeName::Insert));
        assert_eq!(settings.dictation.silence_duration_ms, Some(1500));
        assert_eq!(settings.stt.language.as_deref(), Some("en"));
        assert_eq!(settings.assistant.id.as_deref(), Some("claude-code"));
        assert_eq!(settings.assistant.project_roots, Some(vec![]));
        assert_eq!(settings.shortcuts.aria_key, Some(AriaKeyName::RightOption));
        assert_eq!(settings.shortcuts.ask, Some(ShortcutEntry::Shift));
        assert_eq!(settings.tts.kokoro.voice.as_deref(), Some("af_heart"));
        assert_eq!(settings.dictation.timeout_secs, Some(660.0));
        assert_eq!(
            settings.capture.backend,
            Some(CaptureBackendName::Microphone)
        );
        assert_eq!(
            settings.memory.dir.as_deref(),
            Some(Path::new("/Users/you/.aria/memory"))
        );
    }

    #[test]
    fn an_unknown_key_is_refused_with_its_position() {
        let error = Loaded::parse(
            "[dictation]\noutput_mode = \"copy\"\nloudness = 3\n",
            Path::new(HOME),
        )
        .expect_err("an unknown key is refused");
        let SettingsError::Parse(parse) = error else {
            panic!("{error}");
        };
        assert!(parse.to_string().contains("loudness"), "{parse}");
        assert_eq!(
            parse.position(),
            Some(super::super::Position { line: 3, column: 1 })
        );
    }

    const RENAMED: &[(Key, &str)] = &[(Key::AssistantId, "chat.agent")];

    #[test]
    fn an_alias_loads_and_is_renamed_in_the_text() {
        let loaded = Loaded::parse_with(
            "[chat]\n# who answers\nagent = \"codex\"\n",
            Path::new(HOME),
            RENAMED,
        )
        .expect("loads");
        assert_eq!(loaded.settings.assistant.id.as_deref(), Some("codex"));
        let text = loaded.document.to_string();
        assert!(text.contains("[assistant]"), "{text}");
        assert!(text.contains("# who answers\nid = \"codex\""), "{text}");
        assert!(!text.contains("agent"), "{text}");
    }

    #[test]
    fn an_alias_beside_its_current_key_is_refused_by_both_names() {
        let error = Loaded::parse_with(
            "[assistant]\nid = \"codex\"\n[chat]\nagent = \"claude-code\"\n",
            Path::new(HOME),
            RENAMED,
        )
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("assistant.id"), "{message}");
        assert!(message.contains("chat.agent"), "{message}");
    }

    #[test]
    fn a_bad_value_keeps_its_line_when_an_alias_moved() {
        let error = Loaded::parse_with(
            "[chat]\nagent = \"codex\"\n[memory]\nenabled = 1\n",
            Path::new(HOME),
            RENAMED,
        )
        .unwrap_err();

        assert_eq!(error.position().map(|position| position.line), Some(4));
    }

    #[test]
    fn an_unknown_key_says_it_may_come_from_a_newer_aria() {
        let error = Loaded::parse("[memory]\nsoon = 1\n", Path::new(HOME)).unwrap_err();

        let message = error.to_string();
        assert!(message.contains("soon"), "{message}");
        assert!(message.contains("a newer Aria"), "{message}");
    }

    /// The settings types and the key table are two lists. Every section is
    /// named without `..`, so a new one stops this compiling.
    #[test]
    fn every_key_is_a_field_and_every_field_a_key() {
        let SettingsFile {
            dictation,
            history,
            listen,
            assistant,
            memory,
            prompts: _,
            application_overrides: _,
            shortcuts,
            wake,
            phone,
            delivery,
            stt,
            tts,
            speaker,
            capture,
        } = SettingsFile::default();
        let as_json = |section: serde_json::Result<serde_json::Value>| section.expect("serializes");
        let llm = CleanupLlm {
            endpoint: String::new(),
            model: String::new(),
        };
        let sections = [
            ("dictation", as_json(serde_json::to_value(&dictation))),
            ("dictation.cleanup_llm", as_json(serde_json::to_value(llm))),
            (
                "dictation.vocabulary",
                as_json(serde_json::to_value(&dictation.vocabulary)),
            ),
            ("history", as_json(serde_json::to_value(history))),
            ("listen", as_json(serde_json::to_value(listen))),
            ("assistant", as_json(serde_json::to_value(assistant))),
            ("memory", as_json(serde_json::to_value(memory))),
            ("shortcuts", as_json(serde_json::to_value(shortcuts))),
            ("wake", as_json(serde_json::to_value(wake))),
            ("phone", as_json(serde_json::to_value(phone))),
            ("delivery", as_json(serde_json::to_value(delivery))),
            ("stt", as_json(serde_json::to_value(stt))),
            ("tts", as_json(serde_json::to_value(&tts))),
            ("tts.system", as_json(serde_json::to_value(&tts.system))),
            ("tts.kokoro", as_json(serde_json::to_value(&tts.kokoro))),
            ("tts.qwen", as_json(serde_json::to_value(&tts.qwen))),
            ("speaker", as_json(serde_json::to_value(speaker))),
            ("capture", as_json(serde_json::to_value(capture))),
        ];
        let mut fields: std::collections::BTreeSet<String> =
            ["prompts", "application_overrides", "assistant.models"]
                .map(str::to_owned)
                .into();
        for (section, value) in sections {
            let object = value.as_object().expect("a table");
            fields.extend(
                object
                    .keys()
                    .filter(|name| *name != "cleanup_llm" && *name != "vocabulary")
                    .map(|name| format!("{section}.{name}")),
            );
        }
        let keys: std::collections::BTreeSet<String> =
            Key::ALL.iter().map(|key| key.path().to_owned()).collect();

        assert_eq!(fields, keys);
    }

    #[test]
    fn a_relative_path_is_refused_for_every_program_alike() {
        for text in [
            "[memory]\ndir = \"notes\"\n",
            "[assistant]\nworkdir = \"src\"\n",
            "[assistant]\nproject_roots = [\"src\"]\n",
            "[listen]\ntranscripts_dir = \"out\"\n",
        ] {
            let error = Loaded::parse(text, Path::new(HOME)).unwrap_err();
            assert!(error.to_string().contains("absolute"), "{error}");
        }
    }

    #[test]
    fn a_tilde_expands_in_every_path() {
        let loaded = load(
            "[dictation.vocabulary]\ndir = \"~/src/writ\"\n\
             [assistant]\nworkdir = \"~\"\nproject_roots = [\"~/src\"]\n\
             [prompts.p]\nname = \"P\"\nkind = \"rewrite\"\nfile = \"~/Prompts/p.md\"\n",
        );
        let settings = loaded.settings;
        assert_eq!(
            settings.dictation.vocabulary.dir.as_deref(),
            Some(Path::new("/Users/you/src/writ"))
        );
        assert_eq!(
            settings.assistant.workdir.as_deref(),
            Some(Path::new("/Users/you"))
        );
        assert_eq!(
            settings.assistant.project_roots,
            Some(vec![PathBuf::from("/Users/you/src")])
        );
        let (id, prompt) = &settings.prompts.0[0];
        assert_eq!(id, "p");
        assert_eq!(prompt.file, Path::new("/Users/you/Prompts/p.md"));
        assert!(prompt.enabled);

        let error = Loaded::parse("[memory]\ndir = \"~you/.aria\"\n", Path::new(HOME))
            .expect_err("refused");
        assert!(matches!(
            error,
            SettingsError::Invalid {
                key: Key::MemoryDir,
                source: InvalidValue::HomeSyntax,
                ..
            }
        ));
    }

    #[test]
    fn a_tilde_expands_in_every_path_a_loader_reads() {
        let loaded = load(
            "[memory]\ndir = \"~/a\"\n\
             [wake]\nmodel = \"~/a\"\n\
             [tts.kokoro]\nmodel = \"~/a\"\nvoices = \"~/a\"\n\
             [tts.qwen]\nmodel = \"~/a\"\ntokenizer = \"~/a\"\n\
             [stt]\nbackend = \"parakeet-mlx\"\nmodel = \"~/a\"\ninterpreter = \"~/a\"\n\
             [listen]\nmodel = \"~/a\"\ntranscripts_dir = \"~/a\"\n\
             [speaker]\nmodel = \"~/a\"\nsegmentation_model = \"~/a\"\nprofiles_dir = \"~/a\"\n\
             [capture]\npath = \"~/a\"\n",
        );
        let values = loaded.settings.values();
        for key in [
            Key::MemoryDir,
            Key::WakeModel,
            Key::TtsKokoroModel,
            Key::TtsKokoroVoices,
            Key::TtsQwenModel,
            Key::TtsQwenTokenizer,
            Key::SttModel,
            Key::SttInterpreter,
            Key::ListenModel,
            Key::ListenTranscriptsDir,
            Key::SpeakerModel,
            Key::SpeakerSegmentationModel,
            Key::SpeakerProfilesDir,
            Key::CapturePath,
        ] {
            assert_eq!(
                value_at(&values, key),
                serde_json::json!("/Users/you/a"),
                "{key}"
            );
        }
        refused("[stt]\nmodel = \"~you/tdt\"\n", Key::SttModel);
    }

    #[test]
    fn an_omitted_chord_modifier_is_off() {
        let loaded = load("[shortcuts]\nask = { key_code = 49, shift = true }\n");
        assert_eq!(
            loaded.settings.shortcuts.ask,
            Some(ShortcutEntry::Chord(ChordEntry {
                key_code: 49,
                control: false,
                option: false,
                shift: true,
                command: false,
            }))
        );
        let error = Loaded::parse("[shortcuts]\nask = { key_code = 49 }\n", Path::new(HOME))
            .expect_err("a bare key is refused");
        assert!(matches!(
            error,
            SettingsError::Invalid {
                key: Key::ShortcutsAsk,
                source: InvalidValue::Chord(_),
                ..
            }
        ));
    }

    #[test]
    fn keyed_entries_keep_file_order_and_a_reorder_is_a_new_revision() {
        let prompt = |id: &str| {
            format!("[prompts.{id}]\nname = \"P\"\nkind = \"rewrite\"\nfile = \"/{id}.md\"\n")
        };
        let forward = load(&format!(
            "{}{}{}",
            prompt("zeta"),
            prompt("alpha"),
            prompt("mid")
        ));
        let ids: Vec<&str> = forward
            .settings
            .prompts
            .iter()
            .map(|(id, _prompt)| id)
            .collect();
        assert_eq!(ids, ["zeta", "alpha", "mid"]);

        let reordered = load(&format!(
            "{}{}{}",
            prompt("alpha"),
            prompt("zeta"),
            prompt("mid")
        ));
        assert_ne!(forward.revision, reordered.revision);

        let apps = load(
            "[application_overrides.\"com.tinyspeck.slackmacgap\"]\noutput_mode = \"copy\"\n\
             [application_overrides.\"com.apple.Notes\"]\nexcluded = true\n",
        );
        let ids: Vec<&str> = apps
            .settings
            .application_overrides
            .iter()
            .map(|(id, _entry)| id)
            .collect();
        assert_eq!(ids, ["com.tinyspeck.slackmacgap", "com.apple.Notes"]);

        let error = Loaded::parse(
            "[application_overrides.\"com app\"]\nexcluded = true\n",
            Path::new(HOME),
        )
        .expect_err("a malformed bundle id is refused");
        assert!(matches!(
            error,
            SettingsError::Invalid {
                key: Key::ApplicationOverrides,
                ..
            }
        ));
    }

    #[test]
    fn the_revision_follows_values_not_comments() {
        let original = load("[dictation]\ncleanup = true\n");
        let commented = load("# mine\n[dictation]\ncleanup = true # yes\n");
        let changed = load("[dictation]\ncleanup = false\n");
        assert_eq!(original.revision, commented.revision);
        assert_ne!(original.revision, changed.revision);
    }

    #[test]
    fn one_shortcut_for_two_things_is_refused() {
        for text in [
            "[shortcuts]\nask = \"shift\"\nprompt_list = \"shift\"\n",
            "[shortcuts]\nask = { key_code = 0, control = true }\n\
             [prompts.p]\nname = \"P\"\nkind = \"rewrite\"\nfile = \"/p.md\"\n\
             chord = { key_code = 0, control = true }\n",
        ] {
            let error = Loaded::parse(text, Path::new(HOME)).expect_err("refused");
            assert!(
                matches!(
                    error,
                    SettingsError::Invalid {
                        source: InvalidValue::SharedBinding,
                        ..
                    }
                ),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn a_value_its_rule_forbids_is_refused() {
        refused("[stt]\nbackend = \"server\"\n", Key::SttServer);
        refused("[stt]\nserver = \"https://a.test:9851\"\n", Key::SttServer);
        refused(
            "[stt]\ndevice = \"gpu\"\nbackend = \"whisper\"\n",
            Key::SttDevice,
        );
        refused("[tts]\nfallback = \"system\"\n", Key::TtsFallback);
        refused("[tts]\nfallback = \"none\"\n", Key::TtsFallback);
        refused("[tts]\nrate = 9.0\n", Key::TtsRate);
        refused(
            "[wake]\nenabled = true\nbackend = \"livekit\"\n",
            Key::WakeModel,
        );
        refused(
            "[dictation]\nmax_recording_secs = 700.0\ntimeout_secs = 600.0\n",
            Key::DictationMaxRecordingSecs,
        );
        refused(
            "[assistant]\ntimeout_secs = 600.0\n",
            Key::AssistantTimeoutSecs,
        );
        refused("[capture]\nbackend = \"wav_file\"\n", Key::CapturePath);
        refused("[capture]\npace = \"realtime\"\n", Key::CapturePace);
        refused(
            "[dictation]\nauto_stop_ms = 2999\n",
            Key::DictationAutoStopMs,
        );
        load("[dictation]\nauto_stop_ms = 0\n");
    }

    #[test]
    fn a_fault_in_a_setting_keeps_its_line_and_column() {
        let prompt = "name = \"P\"\nkind = \"rewrite\"\nfile = \"/p.md\"\n";
        for (text, key, line, column) in [
            (
                "[stt]\n  language = \"not a language\"\n".to_owned(),
                Key::SttLanguage,
                2,
                3,
            ),
            (
                "[dictation]\n\nsilence_duration_ms = 1\n".to_owned(),
                Key::DictationSilenceDurationMs,
                3,
                1,
            ),
            (
                "[dictation]\nauto_stop_ms = 2999\n".to_owned(),
                Key::DictationAutoStopMs,
                2,
                1,
            ),
            ("[tts]\nrate = 9.0\n".to_owned(), Key::TtsRate, 2, 1),
            (
                "[assistant]\nid = \"nobody\"\n".to_owned(),
                Key::AssistantId,
                2,
                1,
            ),
            (
                "[assistant.models]\nnobody = \"m\"\n".to_owned(),
                Key::AssistantModels,
                1,
                12,
            ),
            (
                format!("# a note\n[prompts.\"not an id\"]\n{prompt}"),
                Key::Prompts,
                2,
                2,
            ),
            (
                "[application_overrides.\"com app\"]\nexcluded = true\n".to_owned(),
                Key::ApplicationOverrides,
                1,
                2,
            ),
        ] {
            let error = Loaded::parse(&text, Path::new(HOME)).expect_err(&text);
            assert!(
                matches!(&error, SettingsError::Invalid { key: found, .. } if *found == key),
                "{text}: {error}"
            );
            assert_eq!(
                error.position(),
                Some(super::super::Position { line, column }),
                "{text}"
            );
        }
    }

    #[test]
    fn a_word_outside_a_choice_is_refused_by_the_parser() {
        for text in [
            "[capture]\nbackend = \"pulseaudio\"\n",
            "[capture]\npace = \"double\"\n",
            "[stt]\ndevice = \"tpu\"\n",
            "[tts]\nfallback = \"maybe\"\n",
        ] {
            let error = Loaded::parse(text, Path::new(HOME)).expect_err(text);
            assert_eq!(error.position().map(|position| position.line), Some(2));
            assert!(matches!(error, SettingsError::Parse(_)), "{text}");
        }
    }

    #[test]
    fn a_setting_its_backend_does_not_have_is_refused() {
        let server = "[stt]\nbackend = \"server\"\nserver = \"https://a.test\"\n";
        refused(&format!("{server}model = \"unused\"\n"), Key::SttModel);
        refused(&format!("{server}device = \"gpu\"\n"), Key::SttDevice);
        refused(
            &format!("{server}interpreter = \"/unused\"\n"),
            Key::SttInterpreter,
        );
        refused(
            "[stt]\nbackend = \"parakeet\"\nserver = \"https://a.test\"\n",
            Key::SttServer,
        );
        refused(
            "[stt]\nbackend = \"whisper\"\ninterpreter = \"/usr/bin/python3\"\n",
            Key::SttInterpreter,
        );
        refused("[stt]\nbackend = \"parakeet-mlx\"\n", Key::SttInterpreter);
        refused("[listen]\nbackend = \"server\"\n", Key::ListenBackend);
    }

    #[test]
    fn a_value_no_engine_can_use_is_refused_even_while_dormant() {
        refused("[stt]\nbackend = \"vosk\"\n", Key::SttBackend);
        refused("[tts]\nbackend = \"elevenlabs\"\n", Key::TtsBackend);
        refused("[wake]\nbackend = \"porcupine\"\n", Key::WakeBackend);
        refused(
            "[wake]\nenabled = false\nthreshold = 7.5\n",
            Key::WakeThreshold,
        );
        refused(
            "[delivery]\ntimeout_secs = -1.0\n",
            Key::DeliveryTimeoutSecs,
        );
        refused(
            "[memory]\nenabled = false\nextract_model = \"\"\n",
            Key::MemoryExtractModel,
        );
        refused("[stt]\nmodel = \"  \"\n", Key::SttModel);
        refused(
            "[stt]\nbackend = \"parakeet\"\nmodel = \"models/tdt\"\n",
            Key::SttModel,
        );
        refused("[speaker]\nmodel = \"\"\n", Key::SpeakerModel);
        refused(
            "[speaker]\nprofiles_dir = \"  \"\n",
            Key::SpeakerProfilesDir,
        );
        refused("[capture]\npath = \"\"\n", Key::CapturePath);
    }

    #[test]
    fn a_timeout_written_alone_is_checked_against_the_others_default() {
        for (text, key) in [
            (
                "[dictation]\ntimeout_secs = 300.0\n",
                Key::DictationTimeoutSecs,
            ),
            (
                "[dictation]\nmax_recording_secs = 700.0\n",
                Key::DictationMaxRecordingSecs,
            ),
        ] {
            let error = Loaded::parse(text, Path::new(HOME)).expect_err(text);
            assert!(
                matches!(&error, SettingsError::Invalid { key: found, .. } if *found == key),
                "{text}: {error}"
            );
            assert_eq!(
                error.position(),
                Some(super::super::Position { line: 2, column: 1 }),
                "{text}"
            );
        }
        let error = Loaded::parse("[dictation]\ntimeout_secs = 300.0\n", Path::new(HOME))
            .expect_err("shorter than the default recording");
        assert_eq!(
            error.to_string(),
            "line 2, column 1: dictation.timeout_secs (300s) must be longer than \
             dictation.max_recording_secs (600s)"
        );
        load("[dictation]\ntimeout_secs = 601.0\n");
        load("[dictation]\nmax_recording_secs = 659.0\n");
    }

    #[test]
    fn a_path_that_is_not_a_file_is_a_file_fault() {
        let dir = tempfile::tempdir().expect("a temp dir");

        let error = read_text(dir.path()).err().expect("refused");

        assert!(matches!(error, SettingsError::NotAFile { .. }), "{error}");
        assert!(error.is_file_fault());
    }

    #[test]
    fn a_read_that_sees_two_versions_retries_once_then_gives_up() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        fs::write(&path, "[dictation]\ncleanup = true\n").expect("written");
        let stamp = |len: u64| Stamp {
            len,
            modified: SystemTime::UNIX_EPOCH,
            inode: 0,
        };

        let mut calls = 0;
        let text = read_text_with(&path, |_path| {
            calls += 1;
            // The first pair disagrees; the second pair agrees.
            Ok(Some(stamp(if calls == 2 { 1 } else { 0 })))
        })
        .expect("the retry reads a settled file")
        .expect("the file exists");
        assert_eq!(calls, 4);
        assert_eq!(text.raw, "[dictation]\ncleanup = true\n");

        let mut calls = 0;
        let error = read_text_with(&path, |_path| {
            calls += 1;
            Ok(Some(stamp(calls)))
        })
        .err()
        .expect("a file that keeps changing is refused");
        assert!(matches!(error, SettingsError::TornRead { .. }));
    }
}
