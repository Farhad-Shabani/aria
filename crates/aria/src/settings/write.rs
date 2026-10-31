//! The one write path: lock, re-read, edit only the keys named, validate the
//! whole result, and replace the file atomically.

use std::fs::{self, File, Permissions};
use std::io::{self, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use rustix::fs::{FlockOperation, Mode, OFlags, RawMode};
use tempfile::NamedTempFile;
use toml_edit::{DocumentMut, Item, TableLike, Value};

use super::document::{self, Keyed, Loaded, Stamp, Text, read_text};
use super::{Apply, InvalidValue, Key, MAX_SETTINGS_BYTES, Revision, SettingsError, lock_path};
use crate::primitives::private_fs;

const LOCK_RETRY_BUDGET: Duration = Duration::from_secs(2);
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// An editor saving in place is still writing for about this long, and a
/// prefix of its file can parse. A file younger than this gets one wait.
const SETTLE: Duration = Duration::from_millis(200);
const MAX_SYMLINK_HOPS: usize = 32;
/// The header's `#:schema` line names this file, beside settings.toml.
const SCHEMA_FILE_NAME: &str = "settings.schema.json";
pub(super) const SCHEMA_TITLE: &str = "Aria settings";
const HEADER: &str = "\
#:schema ./settings.schema.json
# Aria settings. Every key is optional, and a missing key takes its default.
# Check this file with `aria settings check`.
";

/// What `create` starts from, commented out so every default stays in code.
const EXAMPLE: &str = include_str!("../../../../settings.example.toml");

/// The keys one write changes. Everything else in the file stays as it was.
#[derive(Debug, Clone, Default)]
pub(crate) struct Patch {
    changes: Vec<Edit>,
}

/// One change at `below`, a path inside the key: empty for the key itself,
/// an entry's id, or an entry's id and a field.
#[derive(Debug, Clone)]
struct Edit {
    key: Key,
    below: Vec<String>,
    change: Change,
}

#[derive(Debug, Clone)]
enum Change {
    Set(Item),
    Reset,
    /// Puts the entries of a keyed table in this order.
    Order(Vec<String>),
}

impl Patch {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn set(&mut self, key: Key, value: impl Into<Value>) -> &mut Self {
        self.push(key, Vec::new(), Change::Set(Item::Value(value.into())))
    }

    /// A value inside a keyed table: an entry, or a field below one.
    pub(crate) fn set_below(&mut self, key: Key, below: Vec<String>, value: Value) -> &mut Self {
        let item = match below.as_slice() {
            [_entry] => entry_item(value),
            _ => Item::Value(value),
        };
        self.push(key, below, Change::Set(item))
    }

    /// Any value the key's type serializes, as one value.
    pub(crate) fn set_encoded<T: serde::Serialize + ?Sized>(
        &mut self,
        key: Key,
        value: &T,
    ) -> Result<&mut Self, SettingsError> {
        let value = encode(key, value)?;
        Ok(self.push(key, Vec::new(), Change::Set(Item::Value(value))))
    }

    /// A keyed table, changed entry by entry and field by field. An entry or
    /// field that is the same in both stays as written, comments and all. A
    /// removed entry takes only its own table with it.
    pub(crate) fn set_entries<T: serde::Serialize>(
        &mut self,
        key: Key,
        current: &Keyed<T>,
        next: &Keyed<T>,
    ) -> Result<&mut Self, SettingsError> {
        let current = encode_entries(key, current)?;
        let next = encode_entries(key, next)?;
        for (id, _value) in &current {
            if !next.iter().any(|(next_id, _)| next_id == id) {
                self.push(key, vec![id.clone()], Change::Reset);
            }
        }
        for (id, value) in &next {
            let below = vec![id.clone()];
            match current.iter().find(|(current_id, _)| current_id == id) {
                None => {
                    self.push(key, below, Change::Set(entry_item(value.clone())));
                }
                Some((_id, written)) => self.set_fields(key, below, written, value),
            }
        }
        // Only the prompt list's order means something.
        let kept: Vec<&String> = current
            .iter()
            .map(|(id, _)| id)
            .filter(|id| next.iter().any(|(next_id, _)| next_id == *id))
            .collect();
        let wanted: Vec<&String> = next
            .iter()
            .map(|(id, _)| id)
            .filter(|id| current.iter().any(|(current_id, _)| current_id == *id))
            .collect();
        let added_last = next
            .iter()
            .skip(wanted.len())
            .all(|(id, _)| !current.iter().any(|(current_id, _)| current_id == id));
        if key == Key::Prompts && (kept != wanted || !added_last) {
            let order = next.iter().map(|(id, _)| id.clone()).collect();
            self.push(key, Vec::new(), Change::Order(order));
        }
        Ok(self)
    }

    fn set_fields(&mut self, key: Key, below: Vec<String>, written: &Value, value: &Value) {
        let (Some(written_fields), Some(fields)) =
            (written.as_inline_table(), value.as_inline_table())
        else {
            if !matches_ignoring_format(written, value) {
                self.push(key, below, Change::Set(Item::Value(value.clone())));
            }
            return;
        };
        for (field, _written_value) in written_fields.iter() {
            if fields.get(field).is_none() {
                let mut path = below.clone();
                path.push(field.to_owned());
                self.push(key, path, Change::Reset);
            }
        }
        for (field, field_value) in fields.iter() {
            let unchanged = written_fields
                .get(field)
                .is_some_and(|written_value| matches_ignoring_format(written_value, field_value));
            if !unchanged {
                let mut path = below.clone();
                path.push(field.to_owned());
                self.push(key, path, Change::Set(Item::Value(field_value.clone())));
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Removes the key's line, so the default applies.
    pub(crate) fn reset(&mut self, key: Key) -> &mut Self {
        self.push(key, Vec::new(), Change::Reset)
    }

    /// Removes an entry, or a field below one, from a keyed table.
    pub(crate) fn reset_below(&mut self, key: Key, below: Vec<String>) -> &mut Self {
        self.push(key, below, Change::Reset)
    }

    /// Each key the patch touches, once.
    pub(crate) fn keys(&self) -> impl Iterator<Item = Key> + use<> {
        let mut keys: Vec<Key> = Vec::new();
        for edit in &self.changes {
            if !keys.contains(&edit.key) {
                keys.push(edit.key);
            }
        }
        keys.into_iter()
    }

    fn push(&mut self, key: Key, below: Vec<String>, change: Change) -> &mut Self {
        self.changes.push(Edit { key, below, change });
        self
    }

    fn apply_to(&self, document: &mut DocumentMut) -> Result<(), SettingsError> {
        for Edit { key, below, change } in &self.changes {
            let mut path: Vec<&str> = key.segments().collect();
            path.extend(below.iter().map(String::as_str));
            let Some((name, _parents)) = path.split_last() else {
                continue;
            };
            let not_a_table = || SettingsError::Invalid {
                key: *key,
                source: InvalidValue::NotATable,
                position: None,
            };
            match change {
                Change::Set(item) => {
                    let table = document::table_at(document.as_table_mut(), path.iter().copied())
                        .ok_or_else(not_a_table)?;
                    let mut item = item.clone();
                    // The key, its comment above, the spacing and the comment
                    // after the value all belong to the line, not the value.
                    match table.get_mut(name) {
                        Some(existing) => {
                            if let (Some(old), Some(new)) =
                                (existing.as_value(), item.as_value_mut())
                            {
                                *new.decor_mut() = old.decor().clone();
                            }
                            // File order is the prompt list's order.
                            if let (Some(old), Some(new)) =
                                (existing.as_table(), item.as_table_mut())
                            {
                                new.set_position(old.position());
                                *new.decor_mut() = old.decor().clone();
                            }
                            *existing = item;
                        }
                        None => {
                            table.insert(name, item);
                        }
                    }
                }
                Change::Reset => {
                    if let Some(table) = document::existing_table_at(
                        document.as_table_mut(),
                        path[..path.len() - 1].iter().copied(),
                    ) {
                        table.remove(name);
                    }
                }
                Change::Order(order) => {
                    let table =
                        document::existing_table_at(document.as_table_mut(), path.iter().copied())
                            .ok_or_else(not_a_table)?;
                    reorder(table, order);
                }
            }
        }
        Ok(())
    }
}

fn encode<T: serde::Serialize + ?Sized>(key: Key, value: &T) -> Result<Value, SettingsError> {
    value
        .serialize(toml_edit::ser::ValueSerializer::new())
        .map_err(|source| SettingsError::Encode { key, source })
}

fn encode_entries<T: serde::Serialize>(
    key: Key,
    entries: &Keyed<T>,
) -> Result<Vec<(String, Value)>, SettingsError> {
    entries
        .iter()
        .map(|(id, entry)| Ok((id.to_owned(), encode(key, entry)?)))
        .collect()
}

/// An entry of its own: a `[key.<id>]` table, or a plain value for a map of
/// values such as `assistant.models`.
fn entry_item(value: Value) -> Item {
    match value {
        Value::InlineTable(table) => Item::Table(table.into_table()),
        value => Item::Value(value),
    }
}

/// The same value whatever its spacing or comments.
fn matches_ignoring_format(left: &Value, right: &Value) -> bool {
    let bare = |value: &Value| {
        let mut value = value.clone();
        value.decor_mut().clear();
        value.to_string()
    };
    bare(left) == bare(right)
}

/// Puts entries in `order`. An entry table keeps its comments; the tables
/// trade the places they held in the file.
fn reorder(table: &mut dyn TableLike, order: &[String]) {
    let mut places: Vec<isize> = order
        .iter()
        .filter_map(|id| table.get(id)?.as_table()?.position())
        .collect();
    places.sort_unstable();
    let mut places = places.into_iter();
    let entries: Vec<(toml_edit::Key, Item)> = order
        .iter()
        .filter_map(|id| {
            let key = table.key(id)?.clone();
            let item = table.remove(id)?;
            Some((key, item))
        })
        .collect();
    for (key, mut item) in entries {
        if let Some(entry) = item.as_table_mut()
            && entry.position().is_some()
        {
            entry.set_position(places.next());
        }
        table.insert(key.get(), item);
        if let Some(mut inserted) = table.key_mut(key.get()) {
            *inserted.leaf_decor_mut() = key.leaf_decor().clone();
        }
    }
}

#[derive(Debug)]
pub(crate) enum Written {
    Applied(Loaded),
    /// Written, but a restart-marked key changed.
    AppliedRestartKey(Loaded),
    Refused(Refusal),
}

impl Written {
    fn stamp_with(&mut self, stamp: Option<Stamp>) {
        match self {
            Self::Applied(loaded) | Self::AppliedRestartKey(loaded) => loaded.stamp = stamp,
            Self::Refused(_) => {}
        }
    }
}

#[derive(Debug)]
pub(crate) enum Refusal {
    /// The file no longer holds the revision the patch was made against.
    /// `current` is what it holds instead.
    Conflict { current: Box<Loaded> },
    /// The file is invalid as it stands, so nothing is written over the
    /// edit in progress.
    FileInvalid(SettingsError),
    /// The patch would make the file invalid.
    Invalid(SettingsError),
    /// Something kept writing the file while the write waited.
    ChangedRecently,
}

/// The one write path, with the patch made by `edit` from the file as it
/// stands under the lock.
pub(crate) fn modify<E: From<SettingsError>>(
    path: &Path,
    home: &Path,
    expected: Option<Revision>,
    edit: impl FnOnce(&super::SettingsFile) -> Result<Patch, E>,
) -> Result<Written, E> {
    let _lock = Lock::acquire(&lock_path(path))?;
    let target = resolve_symlinks(path)?;
    let text = match settled_text(&target) {
        Ok(Settled::Text(text)) => text,
        Ok(Settled::Changing) => return Ok(Written::Refused(Refusal::ChangedRecently)),
        Err(error) if error.is_file_fault() => {
            return Ok(Written::Refused(Refusal::FileInvalid(error)));
        }
        Err(error) => return Err(error.into()),
    };
    let mut current = match Loaded::parse(text.as_ref().map_or("", |text| &text.raw), home) {
        Ok(current) => current,
        Err(error) => return Ok(Written::Refused(Refusal::FileInvalid(error))),
    };
    current.stamp = text.as_ref().map(|text| text.stamp);
    if let Some(expected) = expected
        && expected != current.revision
    {
        return Ok(Written::Refused(Refusal::Conflict {
            current: Box::new(current),
        }));
    }
    let patch = edit(&current.settings)?;
    if patch.is_empty() {
        return Ok(Written::Applied(current));
    }

    let (edited, mut written) = match edit_text(&current, text.is_none(), &patch, home) {
        Ok(edited) => edited,
        Err(refusal) => return Ok(Written::Refused(refusal)),
    };
    if text.as_ref().is_some_and(|text| text.raw == edited) {
        written.stamp_with(current.stamp);
        return Ok(written);
    }
    let mode = mode_for(&target, text.is_some())?;
    // An editor takes no lock, so its save may have landed since the read.
    if Stamp::of(&target)? != current.stamp {
        return Ok(Written::Refused(Refusal::ChangedRecently));
    }
    let stamp = commit(&target, &edited, mode)?;
    written.stamp_with(Some(stamp));
    if text.is_none() {
        write_schema_beside(&target);
    }
    for key in patch.keys() {
        tracing::info!(%key, "settings key written");
    }
    Ok(written)
}

/// Makes a missing file from the example, with every key commented out. A
/// file already there stays as it is.
pub(super) fn create(path: &Path) -> Result<(), SettingsError> {
    let _lock = Lock::acquire(&lock_path(path))?;
    let target = resolve_symlinks(path)?;
    if read_text(&target)?.is_some() {
        return Ok(());
    }
    commit(&target, &starter_text(), mode_for(&target, false)?)?;
    write_schema_beside(&target);
    Ok(())
}

fn starter_text() -> String {
    let mut text = format!("#:schema ./{SCHEMA_FILE_NAME}\n");
    for line in EXAMPLE.lines() {
        if !line.is_empty() && !line.starts_with('#') {
            text.push_str("# ");
        }
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// What [`modify`] would do with `patch`, without the lock and without
/// writing: a check before work that a refused write would waste.
pub(crate) fn check_patch(
    path: &Path,
    home: &Path,
    patch: &Patch,
) -> Result<Written, SettingsError> {
    let target = resolve_symlinks(path)?;
    let text = match read_text(&target) {
        Ok(text) => text,
        Err(error) if error.is_file_fault() => {
            return Ok(Written::Refused(Refusal::FileInvalid(error)));
        }
        Err(error) => return Err(error),
    };
    let current = match Loaded::parse(text.as_ref().map_or("", |text| &text.raw), home) {
        Ok(current) => current,
        Err(error) => return Ok(Written::Refused(Refusal::FileInvalid(error))),
    };
    if patch.is_empty() {
        return Ok(Written::Applied(current));
    }
    Ok(match edit_text(&current, text.is_none(), patch, home) {
        Ok((_edited, written)) => written,
        Err(refusal) => Written::Refused(refusal),
    })
}

/// The file's text with `patch` applied, and the settings it holds then.
fn edit_text(
    current: &Loaded,
    new_file: bool,
    patch: &Patch,
    home: &Path,
) -> Result<(String, Written), Refusal> {
    let mut document = current.document.clone();
    // A file of only comments holds them as trailing text, which would end
    // up under the first table written. They go above it instead, as does
    // the header of a new file.
    let prefix = if new_file {
        format!("{HEADER}\n")
    } else if document.as_table().is_empty() {
        let mut comments = document.trailing().as_str().unwrap_or_default().to_owned();
        document.set_trailing("");
        if !comments.is_empty() && !comments.ends_with('\n') {
            comments.push('\n');
        }
        comments
    } else {
        String::new()
    };
    patch.apply_to(&mut document).map_err(Refusal::Invalid)?;
    let edited = format!("{prefix}{document}");
    let next = Loaded::parse(&edited, home).map_err(Refusal::Invalid)?;
    let restart_needed = document::changed_keys(&current.settings, &next.settings)
        .into_iter()
        .any(|key| key.apply() == Apply::Restart);
    let written = if restart_needed {
        Written::AppliedRestartKey(next)
    } else {
        Written::Applied(next)
    };
    Ok((edited, written))
}

pub(super) enum Settled {
    Text(Option<Text>),
    Changing,
}

/// Text a writer has stopped writing. An editor saving in place shows a
/// prefix first, which can parse.
pub(super) fn settled_text(target: &Path) -> Result<Settled, SettingsError> {
    let text = read_text(target)?;
    let Some(stamp) = text.as_ref().map(|text| text.stamp) else {
        // An editor can save by deleting the file and creating it again.
        thread::sleep(SETTLE);
        return Ok(match Stamp::of(target)? {
            Some(_) => Settled::Changing,
            None => Settled::Text(None),
        });
    };
    let age = SystemTime::now()
        .duration_since(stamp.modified)
        .unwrap_or(Duration::ZERO);
    if age >= SETTLE {
        return Ok(Settled::Text(text));
    }
    thread::sleep(SETTLE - age);
    if Stamp::of(target)? != Some(stamp) {
        return Ok(Settled::Changing);
    }
    Ok(Settled::Text(text))
}

/// The lock file's own descriptor. Dropping it releases the lock, and so
/// does the process dying.
struct Lock {
    _descriptor: OwnedFd,
}

impl Lock {
    fn acquire(path: &Path) -> Result<Self, SettingsError> {
        let directory = path.parent().unwrap_or(Path::new("."));
        private_fs::prepare_directory(directory).map_err(|error| SettingsError::Io {
            action: "prepare the directory of",
            path: path.to_path_buf(),
            source: io::Error::other(error),
        })?;
        let flags = OFlags::RDWR | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW;
        let mode = Mode::from_bits_retain(private_fs::PRIVATE_FILE_MODE as RawMode);
        let descriptor =
            rustix::fs::open(path, flags, mode).map_err(|source| SettingsError::Io {
                action: "open",
                path: path.to_path_buf(),
                source: source.into(),
            })?;
        let started = Instant::now();
        loop {
            match rustix::fs::flock(&descriptor, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => {
                    return Ok(Self {
                        _descriptor: descriptor,
                    });
                }
                Err(rustix::io::Errno::WOULDBLOCK | rustix::io::Errno::INTR) => {
                    if started.elapsed() >= LOCK_RETRY_BUDGET {
                        return Err(SettingsError::Locked {
                            path: path.to_path_buf(),
                        });
                    }
                    thread::sleep(LOCK_RETRY_INTERVAL);
                }
                Err(source) => {
                    return Err(SettingsError::Io {
                        action: "lock",
                        path: path.to_path_buf(),
                        source: source.into(),
                    });
                }
            }
        }
    }
}

/// The file the path finally names. A dangling link resolves to its missing
/// target, which the write then creates.
pub(super) fn resolve_symlinks(path: &Path) -> Result<PathBuf, SettingsError> {
    static WARNED: Once = Once::new();
    let mut current = path.to_path_buf();
    for _hop in 0..MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&current).map_err(|source| SettingsError::Io {
                    action: "resolve",
                    path: current.clone(),
                    source,
                })?;
                current = if link.is_absolute() {
                    link
                } else {
                    current.parent().unwrap_or(Path::new(".")).join(link)
                };
            }
            Ok(_metadata) => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(SettingsError::Io {
                    action: "inspect",
                    path: current,
                    source,
                });
            }
        }
    }
    if current != path {
        WARNED.call_once(|| {
            tracing::warn!(
                target = %current.display(),
                "settings.toml is a symlink; settings live at its target"
            );
        });
    }
    Ok(current)
}

/// 0600 for a new file. An existing file keeps its mode unless group or others
/// have any access, which tightens it to 0600. A file its owner can't write is
/// refused.
fn mode_for(target: &Path, exists: bool) -> Result<u32, SettingsError> {
    if !exists {
        return Ok(private_fs::PRIVATE_FILE_MODE);
    }
    let metadata = fs::metadata(target).map_err(|source| SettingsError::Io {
        action: "inspect",
        path: target.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o200 == 0 {
        return Err(SettingsError::ReadOnly {
            path: target.to_path_buf(),
        });
    }
    if mode & 0o077 != 0 {
        tracing::info!(
            mode = format_args!("{mode:o}"),
            "tightening settings.toml to mode 600"
        );
        return Ok(private_fs::PRIVATE_FILE_MODE);
    }
    Ok(mode)
}

/// Writes to a sibling temporary file, then renames, so a crash leaves the
/// old file or the new one, never half of either.
fn commit(target: &Path, text: &str, mode: u32) -> Result<Stamp, SettingsError> {
    if text.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(SettingsError::SerializedTooLarge);
    }
    replace_file(target, text, mode)
}

fn replace_file(target: &Path, text: &str, mode: u32) -> Result<Stamp, SettingsError> {
    let directory = target.parent().unwrap_or(Path::new("."));
    let mut temporary = NamedTempFile::new_in(directory)
        .map_err(|source| io_error("create a temporary for", target, source))?;
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(mode))
        .and_then(|()| temporary.write_all(text.as_bytes()))
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| io_error("write", target, source))?;
    // Read before the rename, which keeps these values. Reading the path after
    // it could describe an editor's save instead.
    let stamp = temporary
        .as_file()
        .metadata()
        .map(|metadata| Stamp::from_metadata(&metadata))
        .map_err(|source| io_error("inspect", target, source))?;
    temporary
        .persist(target)
        .map_err(|error| io_error("replace", target, error.error))?;
    // The file is already replaced, so this can't fail the write.
    if let Err(error) = File::open(directory).and_then(|directory| directory.sync_all()) {
        tracing::warn!(%error, "settings directory not flushed");
    }
    Ok(stamp)
}

/// Keeps the schema the header names beside settings.toml. Editors are all
/// that read it, so a directory that refuses it, such as a read-only Nix
/// store, costs a warning and nothing else.
pub(crate) fn write_schema_beside(settings_path: &Path) {
    if let Err(error) = update_schema(settings_path) {
        tracing::warn!(
            path = %settings_path.display(),
            error = %crate::primitives::error_chain(&error),
            "settings schema not written"
        );
    }
}

/// The header's relative path resolves from the real file, not from a
/// symlink to it.
fn update_schema(settings_path: &Path) -> Result<(), SettingsError> {
    let path = &resolve_symlinks(settings_path)?.with_file_name(SCHEMA_FILE_NAME);
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(&super::schema()).map_err(|error| SettingsError::Io {
            action: "encode",
            path: path.to_path_buf(),
            source: io::Error::other(error),
        })?
    );
    if holds_text(path, &text) {
        return Ok(());
    }
    if !is_replaceable_schema(path) {
        tracing::warn!(path = %path.display(), "settings schema not written over another file");
        return Ok(());
    }
    replace_file(path, &text, private_fs::PRIVATE_FILE_MODE).map(|_stamp| ())
}

/// A symlink can put settings.toml in a folder where another tool owns a file
/// of this name. Nothing there, or an earlier Aria schema, can be replaced.
fn is_replaceable_schema(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return true;
    };
    metadata.is_file()
        && metadata.len() <= MAX_SETTINGS_BYTES
        && fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .is_some_and(|schema| schema["title"] == SCHEMA_TITLE)
}

/// The size check comes first so a FIFO or a huge file is never read.
fn holds_text(path: &Path, text: &str) -> bool {
    let same_size = fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == text.len() as u64);
    same_size && fs::read_to_string(path).is_ok_and(|written| written == text)
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> SettingsError {
    match source.kind() {
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
            SettingsError::ReadOnly {
                path: path.to_path_buf(),
            }
        }
        _ => SettingsError::Io {
            action,
            path: path.to_path_buf(),
            source,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::document::tests::HOME;
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn create_writes_every_key_commented_out_once_and_never_over_a_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");

        create(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(&format!("#:schema ./{SCHEMA_FILE_NAME}\n")));
        assert!(text.contains("# [dictation]\n"), "{text}");
        assert!(text.contains("# output_mode = \"insert\""), "{text}");
        assert_eq!(
            Loaded::parse(&text, Path::new(HOME)).unwrap().settings,
            super::super::SettingsFile::default()
        );
        assert!(directory.path().join(SCHEMA_FILE_NAME).is_file());

        fs::write(&path, "# mine\n").unwrap();
        create(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "# mine\n");
    }

    #[test]
    fn the_first_write_creates_the_schema_the_header_names() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let patch = one_key(Key::MemoryEnabled, true);

        modify::<SettingsError>(&path, Path::new(HOME), None, |_current| Ok(patch)).unwrap();

        let schema = fs::read_to_string(directory.path().join(SCHEMA_FILE_NAME)).unwrap();
        assert!(HEADER.contains(&format!("#:schema ./{SCHEMA_FILE_NAME}")));
        let parsed: serde_json::Value = serde_json::from_str(&schema).unwrap();
        assert_eq!(parsed, super::super::schema());
    }

    #[test]
    fn the_schema_goes_beside_the_file_a_symlink_names() {
        let directory = tempfile::tempdir().unwrap();
        let dotfiles = directory.path().join("dotfiles");
        let aria = directory.path().join("aria");
        fs::create_dir(&dotfiles).unwrap();
        fs::create_dir(&aria).unwrap();
        fs::write(dotfiles.join("settings.toml"), "").unwrap();
        let link = aria.join("settings.toml");
        std::os::unix::fs::symlink(dotfiles.join("settings.toml"), &link).unwrap();

        write_schema_beside(&link);

        assert!(dotfiles.join(SCHEMA_FILE_NAME).is_file());
        assert!(!aria.join(SCHEMA_FILE_NAME).exists());
    }

    #[test]
    fn an_unchanged_schema_is_left_alone_and_a_stale_one_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let settings = directory.path().join("settings.toml");
        let schema = directory.path().join(SCHEMA_FILE_NAME);

        write_schema_beside(&settings);
        let written = fs::metadata(&schema).unwrap().ino();
        write_schema_beside(&settings);
        assert_eq!(fs::metadata(&schema).unwrap().ino(), written);

        fs::write(&schema, format!("{{\"title\": \"{SCHEMA_TITLE}\"}}")).unwrap();
        write_schema_beside(&settings);
        let current: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&schema).unwrap()).unwrap();
        assert_eq!(current, super::super::schema());
    }

    #[test]
    fn another_tools_file_of_the_same_name_stays() {
        let directory = tempfile::tempdir().unwrap();
        let schema = directory.path().join(SCHEMA_FILE_NAME);
        fs::write(&schema, "{\"title\": \"Another tool\"}").unwrap();

        write_schema_beside(&directory.path().join("settings.toml"));

        assert_eq!(
            fs::read_to_string(&schema).unwrap(),
            "{\"title\": \"Another tool\"}"
        );
    }

    #[test]
    fn a_schema_that_cannot_be_written_never_blocks_the_settings_write() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        let in_the_way = directory.path().join(SCHEMA_FILE_NAME);
        fs::create_dir(&in_the_way).unwrap();
        fs::write(in_the_way.join("keep"), "").unwrap();
        let patch = one_key(Key::MemoryEnabled, true);

        let written =
            modify::<SettingsError>(&path, Path::new(HOME), None, |_current| Ok(patch)).unwrap();

        assert!(matches!(written, Written::Applied(_)));
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("enabled = true")
        );
        assert!(in_the_way.is_dir());
    }

    fn write(
        path: &Path,
        home: &Path,
        expected: Option<Revision>,
        patch: &Patch,
    ) -> Result<Written, SettingsError> {
        modify(path, home, expected, |_current| Ok(patch.clone()))
    }

    fn file_with(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        fs::write(&path, text).expect("written");
        (dir, path)
    }

    fn one_key(key: Key, value: impl Into<Value>) -> Patch {
        let mut patch = Patch::new();
        patch.set(key, value);
        patch
    }

    fn home() -> &'static Path {
        Path::new(HOME)
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    fn applied(written: Written) -> Loaded {
        match written {
            Written::Applied(loaded) => loaded,
            other => panic!("expected Applied, got {other:?}"),
        }
    }

    #[test]
    fn a_first_write_creates_a_private_file_with_a_header() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join(".aria/settings.toml");
        let patch = one_key(Key::MemoryEnabled, true);

        let loaded = applied(write(&path, home(), None, &patch).expect("writes"));

        assert_eq!(loaded.settings.memory.enabled, Some(true));
        let text = fs::read_to_string(&path).expect("read");
        assert_eq!(text, format!("{HEADER}\n[memory]\nenabled = true\n"));
        assert_eq!(mode(&path), 0o600);
        assert!(path.with_file_name("settings.lock").exists());
    }

    #[test]
    fn the_starter_comments_survive_a_set_and_its_reset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.toml");
        create(&path).unwrap();
        let starter = fs::read_to_string(&path).unwrap();
        let mut reset = Patch::new();
        reset.reset(Key::MemoryEnabled);

        write(&path, home(), None, &one_key(Key::MemoryEnabled, true)).expect("sets");
        write(&path, home(), None, &reset).expect("resets");

        assert!(fs::read_to_string(&path).unwrap().starts_with(&starter));
    }

    #[test]
    fn a_comment_with_no_final_newline_stays_above_the_first_table() {
        let (_dir, path) = file_with("# mine");
        let patch = one_key(Key::MemoryEnabled, true);

        applied(write(&path, home(), None, &patch).expect("writes"));

        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "# mine\n[memory]\nenabled = true\n"
        );
    }

    #[test]
    fn a_save_that_lands_during_a_write_is_kept() {
        let (_dir, path) = file_with("[memory]\nenabled = true\n");
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(1))
            .unwrap();

        let written = modify::<SettingsError>(&path, home(), None, |_current| {
            fs::write(&path, "[memory]\nenabled = false # mine\n").unwrap();
            Ok(one_key(Key::HistoryEnabled, false))
        })
        .expect("runs");

        assert!(matches!(
            written,
            Written::Refused(Refusal::ChangedRecently)
        ));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[memory]\nenabled = false # mine\n"
        );
    }

    #[test]
    fn a_file_back_during_the_wait_counts_as_changing() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        let editor_path = path.clone();
        let editor = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            fs::write(editor_path, "[history]\nenabled = false\n").unwrap();
        });

        let settled = settled_text(&path).expect("reads");

        editor.join().unwrap();
        assert!(matches!(settled, Settled::Changing));
    }

    #[test]
    fn a_failed_directory_flush_does_not_fail_the_write() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let real_dir = dir.path().join("dotfiles");
        fs::create_dir(&real_dir).unwrap();
        let real = real_dir.join("settings.toml");
        fs::write(&real, "[memory]\nenabled = true\n").unwrap();
        fs::File::options()
            .write(true)
            .open(&real)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(1))
            .unwrap();
        let link = dir.path().join("settings.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        fs::set_permissions(&real_dir, Permissions::from_mode(0o300)).unwrap();

        let written = write(&link, home(), None, &one_key(Key::MemoryEnabled, false));

        fs::set_permissions(&real_dir, Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(written, Ok(Written::Applied(_))), "{written:?}");
        assert!(
            fs::read_to_string(&real)
                .unwrap()
                .contains("enabled = false")
        );
    }

    #[test]
    fn a_write_changes_only_its_line() {
        let original = "# mine\n\n[dictation]\n# say less\ncleanup = true   # keep\nbacktrack = false\n\n[memory]\nenabled = false\n";
        let (_dir, path) = file_with(original);
        let patch = one_key(Key::DictationCleanup, false);

        applied(write(&path, home(), None, &patch).expect("writes"));

        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            original.replace("cleanup = true   # keep", "cleanup = false   # keep")
        );
    }

    #[test]
    fn a_reset_removes_the_line_and_nothing_else() {
        let (_dir, path) = file_with("[memory]\nenabled = true\nextract_model = \"opus\"\n");
        let mut patch = Patch::new();
        patch.reset(Key::MemoryEnabled).reset(Key::WakeEnabled);

        let loaded = applied(write(&path, home(), None, &patch).expect("writes"));

        assert_eq!(loaded.settings.memory.enabled, None);
        assert_eq!(
            fs::read_to_string(&path).expect("read"),
            "[memory]\nextract_model = \"opus\"\n"
        );
    }

    #[test]
    fn a_stale_revision_is_a_conflict() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.toml");
        let patch = one_key(Key::MemoryEnabled, true);
        let first = applied(write(&path, home(), None, &patch).expect("writes"));
        fs::write(&path, "[memory]\nenabled = false\n").expect("hand edit");

        let refused = write(&path, home(), Some(first.revision), &patch).expect("runs");

        assert!(
            matches!(&refused, Written::Refused(Refusal::Conflict { current }) if current.revision != first.revision),
            "{refused:?}"
        );
        assert!(matches!(
            write(
                &path,
                home(),
                Some(
                    Loaded::parse("[memory]\nenabled = false\n", home())
                        .expect("loads")
                        .revision
                ),
                &patch
            ),
            Ok(Written::Applied(_))
        ));
    }

    #[test]
    fn an_invalid_file_is_never_written_over() {
        let broken = "[memory]\nenabled = maybe\n";
        let (_dir, path) = file_with(broken);
        let patch = one_key(Key::MemoryEnabled, true);

        let refused = write(&path, home(), None, &patch).expect("runs");

        assert!(
            matches!(refused, Written::Refused(Refusal::FileInvalid(_))),
            "{refused:?}"
        );
        assert_eq!(fs::read_to_string(&path).expect("read"), broken);
    }

    #[test]
    fn two_writers_on_different_keys_both_land() {
        let (_dir, path) = file_with("# shared\n");

        thread::scope(|scope| {
            for (key, value) in [(Key::MemoryEnabled, true), (Key::DictationCleanup, true)] {
                let path = &path;
                scope.spawn(move || {
                    let mut patch = Patch::new();
                    patch.set(key, value);
                    applied(write(path, home(), None, &patch).expect("writes"));
                });
            }
        });

        let loaded =
            Loaded::parse(&fs::read_to_string(&path).expect("read"), home()).expect("loads");
        assert_eq!(loaded.settings.memory.enabled, Some(true));
        assert_eq!(loaded.settings.dictation.cleanup, Some(true));
        assert!(loaded.document.to_string().starts_with("# shared\n"));
    }

    #[test]
    fn a_read_only_file_is_a_typed_error() {
        let (_dir, path) = file_with("[memory]\nenabled = true\n");
        fs::set_permissions(&path, Permissions::from_mode(0o444)).expect("chmod");
        let patch = one_key(Key::MemoryEnabled, false);

        let error = write(&path, home(), None, &patch).expect_err("refused");

        assert!(matches!(error, SettingsError::ReadOnly { .. }), "{error}");
    }

    #[test]
    fn a_symlink_is_written_through_and_its_mode_kept() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let real = dir.path().join("dotfiles/settings.toml");
        fs::create_dir_all(real.parent().expect("parent")).expect("mkdir");
        fs::write(&real, "[memory]\nenabled = true\n").expect("written");
        fs::set_permissions(&real, Permissions::from_mode(0o600)).expect("chmod");
        let link = dir.path().join("settings.toml");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let patch = one_key(Key::MemoryEnabled, false);

        applied(write(&link, home(), None, &patch).expect("writes"));

        assert!(link.symlink_metadata().expect("link").is_symlink());
        assert_eq!(
            fs::read_to_string(&real).expect("read"),
            "[memory]\nenabled = false\n"
        );
        assert_eq!(mode(&real), 0o600);
        assert!(
            link.with_file_name("settings.lock").exists(),
            "the lock sits beside the link"
        );
    }

    #[test]
    fn a_dangling_symlink_gets_its_target_created() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let real = dir.path().join("dotfiles/settings.toml");
        fs::create_dir_all(real.parent().expect("parent")).expect("mkdir");
        let link = dir.path().join("settings.toml");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let patch = one_key(Key::MemoryEnabled, true);

        applied(write(&link, home(), None, &patch).expect("writes"));

        assert!(link.symlink_metadata().expect("link").is_symlink());
        assert!(real.is_file());
    }

    #[test]
    fn a_loose_mode_is_tightened_once() {
        let (_dir, path) = file_with("[memory]\nenabled = true\n");
        fs::set_permissions(&path, Permissions::from_mode(0o644)).expect("chmod");
        let patch = one_key(Key::MemoryEnabled, false);

        applied(write(&path, home(), None, &patch).expect("writes"));

        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn a_held_lock_is_a_typed_error_after_the_retry_budget() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let lock = dir.path().join("settings.lock");
        let _held = Lock::acquire(&lock).expect("acquires");

        let error = Lock::acquire(&lock).err().expect("refused");

        assert!(matches!(error, SettingsError::Locked { .. }), "{error}");
    }
}
