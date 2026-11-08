//! Saving a prompt: its Markdown file first, then its entry in settings, so a
//! failed write leaves the prompt list as it was.
//!
//! A new file goes in the prompts folder, named after the prompt and never
//! over a file already there. An existing file is replaced whole, at the file
//! a symlink names. Removing a prompt keeps its file.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use aria_primitives::private_fs::{self, PRIVATE_FILE_MODE, PrivateDirectoryError};
use aria_settings::rpc::RpcErrorCode;
use aria_settings::{
    MAX_PROMPT_NAME_BYTES, PROMPTS_DIR, Prompt, PromptError, PromptId, PromptKind, Prompts,
    RESERVED_PROMPT_FILES, Settings, SettingsPatch, SettingsStore, StoreError,
};

use crate::chat::{Instructions, InstructionsError, read_instructions};

/// A name is saved trimmed.
#[derive(Debug, Clone)]
pub enum PromptDraft {
    New {
        kind: PromptKind,
        source: NewSource,
    },
    /// Each `None` keeps what the stored prompt has.
    Edit {
        id: PromptId,
        name: Option<String>,
        kind: Option<PromptKind>,
        /// The file the prompt reads from now on.
        file: Option<PathBuf>,
        /// Replace the file's text.
        instructions: Option<Instructions>,
    },
}

/// Where a new prompt's instructions live.
#[derive(Debug, Clone)]
pub enum NewSource {
    /// A file of its own in the prompts folder, named after the prompt.
    Written {
        name: String,
        instructions: Instructions,
    },
    /// A file the caller chose, used where it is. A `None` name comes from the file's name. `None`
    /// instructions leave the file's text as it is.
    Chosen {
        file: PathBuf,
        name: Option<String>,
        instructions: Option<Instructions>,
    },
}

/// Where an editor reads a prompt's instructions from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    Stored(PromptId),
    File(PathBuf),
}

/// A prompt file's text as an editor opens it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptContents {
    /// Empty when the file is blank, so an editor can fill it.
    Instructions(String),
    /// No file is there. Saving instructions creates it.
    Missing,
}

/// Reads `file` as a run would, but returns a blank file as empty text and a missing one as
/// `Missing`.
pub fn contents(file: &Path) -> Result<PromptContents, InstructionsError> {
    match read_instructions(file) {
        Ok(instructions) => Ok(PromptContents::Instructions(
            instructions.as_str().to_owned(),
        )),
        Err(InstructionsError::Empty(_)) => Ok(PromptContents::Instructions(String::new())),
        Err(InstructionsError::Unreadable { source, .. })
            if source.kind() == io::ErrorKind::NotFound =>
        {
            Ok(PromptContents::Missing)
        }
        Err(error) => Err(error),
    }
}

/// Writes the draft's file, then its entry. A new entry is on and has no chord. An edited entry
/// keeps the chord and on state it has when the list is edited.
///
/// The draft is checked against the list as the settings file holds it now. Its file is written
/// with no lock held. Only the list edit takes the settings lock, and it reads the list again. So
/// an edit keeps what another writer changed and the draft left out. Two saves never pick the same
/// new file name, since a new file is never made over one already there. A refused entry removes a
/// file the save created. A file the edit rewrote stays as written. If the edit rewrote the entry's
/// file and the entry names another file by the time the list is edited, the save returns
/// [`SaveError::FileChanged`].
pub fn save(
    home: &Path,
    settings: &SettingsStore,
    draft: PromptDraft,
) -> Result<Prompt, SaveError> {
    let (entry, target) = Entry::of(draft)?;
    let current = current_settings(settings)?;
    let entries = current.prompts.entries();
    let placed = Placed::of(entries, &entry)?;
    let (written, created) = match &target {
        Target::Chosen(file, instructions) => {
            if instructions.is_none() {
                read_instructions(file).map_err(SaveError::Unreadable)?;
            }
            placed.listed(entries, &entry, file.clone())?;
            if let Some(instructions) = instructions {
                replace(file, instructions)?;
            }
            (file.clone(), None)
        }
        Target::Kept(instructions) => {
            let file = placed.edited_file(entries)?.to_path_buf();
            placed.listed(entries, &entry, file.clone())?;
            if let Some(instructions) = instructions {
                replace(&file, instructions)?;
            }
            (file, None)
        }
        Target::Created(instructions) => {
            let folder = home.join(PROMPTS_DIR);
            // The file's name is known only once it is made, so the list is
            // checked first with the name it would most likely get.
            placed.listed(entries, &entry, folder.join(file_name(&placed.name, &[])))?;
            let created = CreatedFile::create(&folder, &placed.name, instructions)?;
            (created.path.clone(), Some(created))
        }
    };
    let snapshot = settings.modify_with(|current| {
        let entries = current.prompts.entries();
        let placed = Placed::of(entries, &entry)?;
        let file = match &target {
            Target::Chosen(..) | Target::Created(_) => written.clone(),
            Target::Kept(instructions) => {
                let named = placed.edited_file(entries)?;
                if instructions.is_some() && named != written {
                    return Err(SaveError::FileChanged);
                }
                named.to_path_buf()
            }
        };
        placed.listed(entries, &entry, file)
    })?;
    if let Some(created) = created {
        created.keep();
    }
    snapshot
        .settings
        .prompts
        .find(entry.id().as_str())
        .cloned()
        .ok_or(SaveError::UnknownPrompt)
}

/// The settings as the file holds them now. The store's own copy can lag another writer's. A save
/// must not write a file the entry no longer names. An empty patch writes nothing, so the lock is
/// held only to read.
fn current_settings(settings: &SettingsStore) -> Result<Settings, SaveError> {
    let snapshot = settings.modify_with(|_current| Ok::<_, SaveError>(SettingsPatch::default()))?;
    Ok(snapshot.settings)
}

/// A draft resolved against one snapshot of the list: the entry it edits, and the name and kind it
/// saves.
struct Placed {
    stored: Option<usize>,
    name: String,
    kind: PromptKind,
}

impl Placed {
    fn of(entries: &[Prompt], entry: &Entry) -> Result<Self, SaveError> {
        Ok(match entry {
            Entry::Add { name, kind, .. } => Self {
                stored: None,
                name: name.clone(),
                kind: *kind,
            },
            Entry::Edit { id, name, kind } => {
                let index = entries
                    .iter()
                    .position(|prompt| prompt.id() == id)
                    .ok_or(SaveError::UnknownPrompt)?;
                Self {
                    stored: Some(index),
                    name: name
                        .clone()
                        .unwrap_or_else(|| entries[index].name().to_owned()),
                    kind: kind.unwrap_or(entries[index].kind()),
                }
            }
        })
    }

    fn edited_file<'a>(&self, entries: &'a [Prompt]) -> Result<&'a Path, SaveError> {
        let index = self.stored.ok_or(SaveError::UnknownPrompt)?;
        Ok(entries[index].file())
    }

    /// The list patch with this entry pointing at `file`.
    fn listed(
        &self,
        entries: &[Prompt],
        entry: &Entry,
        file: PathBuf,
    ) -> Result<SettingsPatch, SaveError> {
        let mut entries = entries.to_vec();
        let id = entry.id().clone();
        match self.stored {
            Some(index) => {
                let before = &entries[index];
                let (enabled, chord) = (before.enabled(), before.chord());
                entries[index] =
                    Prompt::new(id, self.name.clone(), self.kind, file, enabled, chord)?;
            }
            None => entries.push(Prompt::new(
                id,
                self.name.clone(),
                self.kind,
                file,
                true,
                None,
            )?),
        }
        prompts_patch(entries)
    }
}

/// Takes the prompt off the list and leaves its file where it is. Returns the
/// prompt as it stood.
pub fn remove(settings: &SettingsStore, id: &PromptId) -> Result<Prompt, SaveError> {
    let mut removed = None;
    settings.modify_with(|current| {
        let mut entries = current.prompts.entries().to_vec();
        let index = entries
            .iter()
            .position(|prompt| prompt.id() == id)
            .ok_or(SaveError::UnknownPrompt)?;
        removed = Some(entries.remove(index));
        prompts_patch(entries)
    })?;
    removed.ok_or(SaveError::UnknownPrompt)
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    #[error("no stored prompt has that ID")]
    UnknownPrompt,

    #[error("the chosen prompt file can't be read")]
    Unreadable(#[source] InstructionsError),

    /// Its file was written, and stays as written.
    #[error("the prompt moved to another file while it was saved; try again")]
    FileChanged,

    #[error("the prompt can't be stored")]
    Prompt(#[from] PromptError),

    #[error("failed to prepare the prompts folder {}", path.display())]
    Folder {
        path: PathBuf,
        #[source]
        source: PrivateDirectoryError,
    },

    #[error("failed to write the prompt file {}", path.display())]
    File {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("failed to make a prompt ID")]
    Id(#[source] getrandom::Error),

    #[error("failed to save the prompt list")]
    Settings(#[from] StoreError),
}

impl From<&SaveError> for RpcErrorCode {
    fn from(error: &SaveError) -> Self {
        match error {
            SaveError::UnknownPrompt => Self::PromptMissing,
            SaveError::Unreadable(_) => Self::PromptUnreadable,
            SaveError::Prompt(PromptError::TooMany) => Self::PromptsFull,
            // Maps like a settings file that changed during a write.
            SaveError::FileChanged => Self::Busy,
            SaveError::Prompt(
                PromptError::Id | PromptError::Name | PromptError::File | PromptError::DuplicateId,
            ) => Self::InvalidRequest,
            SaveError::Folder { .. } | SaveError::File { .. } => Self::PromptWriteFailed,
            SaveError::Id(_) => Self::Internal,
            SaveError::Settings(error) => Self::from(error),
        }
    }
}

/// The entry a save writes, with the name and kind the draft gives it. An edited entry must still
/// be in the list when the edit lands.
enum Entry {
    Add {
        id: PromptId,
        name: String,
        kind: PromptKind,
    },
    Edit {
        id: PromptId,
        name: Option<String>,
        kind: Option<PromptKind>,
    },
}

/// The file a save writes, and the text it writes there.
enum Target {
    /// A file the draft names. `None` leaves its text as it is.
    Chosen(PathBuf, Option<Instructions>),
    /// The file the edited entry names.
    Kept(Option<Instructions>),
    /// A new file in the prompts folder.
    Created(Instructions),
}

impl Entry {
    fn of(draft: PromptDraft) -> Result<(Self, Target), SaveError> {
        let trimmed = |name: String| name.trim().to_owned();
        Ok(match draft {
            PromptDraft::New { kind, source } => {
                let (name, target) = match source {
                    NewSource::Written { name, instructions } => {
                        (trimmed(name), Target::Created(instructions))
                    }
                    NewSource::Chosen {
                        file,
                        name,
                        instructions,
                    } => (
                        name.map_or_else(|| name_from_file(&file), trimmed),
                        Target::Chosen(file, instructions),
                    ),
                };
                let id = fresh_id()?;
                (Self::Add { id, name, kind }, target)
            }
            PromptDraft::Edit {
                id,
                name,
                kind,
                file,
                instructions,
            } => {
                let target = match file {
                    Some(file) => Target::Chosen(file, instructions),
                    None => Target::Kept(instructions),
                };
                let name = name.map(trimmed);
                (Self::Edit { id, name, kind }, target)
            }
        })
    }

    fn id(&self) -> &PromptId {
        match self {
            Self::Add { id, .. } | Self::Edit { id, .. } => id,
        }
    }
}

fn prompts_patch(entries: Vec<Prompt>) -> Result<SettingsPatch, SaveError> {
    Ok(SettingsPatch {
        prompts: Some(Prompts::new(entries)?),
        ..SettingsPatch::default()
    })
}

/// The file's name without `.md`, cut to fit a prompt's name.
fn name_from_file(file: &Path) -> String {
    let name = file
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let stem = match name.len().checked_sub(3) {
        Some(end) if name.is_char_boundary(end) && name[end..].eq_ignore_ascii_case(".md") => {
            &name[..end]
        }
        _ => &name,
    };
    fit_name(stem)
}

/// Names a one-off run after the first non-blank line of its instructions, cut to fit a prompt's
/// name. The instructions are never blank, so that line exists.
pub fn one_off_name(instructions: &Instructions) -> String {
    let line = instructions
        .as_str()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    fit_name(line)
}

fn fit_name(text: &str) -> String {
    let text = text.trim();
    let mut end = text.len().min(MAX_PROMPT_NAME_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].trim_end().to_owned()
}

/// A UUID, as the app names a prompt it makes.
fn fresh_id() -> Result<PromptId, SaveError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(SaveError::Id)?;
    // Version 4, variant RFC 4122.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    Ok(PromptId::new(id)?)
}

/// `name`.md, numbered past every name `taken` holds and every reserved
/// one. A Mac's file system ignores case, so the names are compared
/// without it.
fn file_name(name: &str, taken: &[String]) -> String {
    let separated: String = name
        .chars()
        .map(|character| match character {
            '/' | ':' => '-',
            character => character,
        })
        .collect();
    let base = separated.trim_start_matches('.');
    let taken: Vec<String> = taken
        .iter()
        .map(String::as_str)
        .chain(RESERVED_PROMPT_FILES)
        .map(str::to_lowercase)
        .collect();
    let mut candidate = format!("{base}.md");
    let mut number = 2;
    while taken.contains(&candidate.to_lowercase()) {
        candidate = format!("{base} {number}.md");
        number += 1;
    }
    candidate
}

/// A prompt file this save made. Dropping it before [`CreatedFile::keep`] removes the file, so a
/// refused save leaves no file behind to number past.
struct CreatedFile {
    path: PathBuf,
    kept: bool,
}

impl CreatedFile {
    /// `name`.md in `folder`, never over a file already there. The folder is made private if it
    /// isn't there yet. One that is, or a link to one, is used as it is.
    fn create(folder: &Path, name: &str, instructions: &Instructions) -> Result<Self, SaveError> {
        if !folder.is_dir() {
            // Aria's own folder first: a home Aria never wrote to has neither.
            for directory in [folder.parent().unwrap_or(folder), folder] {
                private_fs::prepare_directory(directory).map_err(|source| SaveError::Folder {
                    path: directory.to_path_buf(),
                    source,
                })?;
            }
        }
        let listing_failed = |source| SaveError::Folder {
            path: folder.to_path_buf(),
            source: PrivateDirectoryError::Inspect(source),
        };
        let mut taken = fs::read_dir(folder)
            .map_err(listing_failed)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<String>>>()
            .map_err(listing_failed)?;
        loop {
            let candidate = file_name(name, &taken);
            let path = folder.join(&candidate);
            match Self::create_new(path.clone(), instructions) {
                // A name the listing didn't show, such as another
                // normalization of the same name, which APFS treats as one.
                Err(source)
                    if source.kind() == io::ErrorKind::AlreadyExists
                        && taken.len() < MAX_TAKEN_NAMES =>
                {
                    taken.push(candidate);
                }
                Err(source) => return Err(SaveError::File { path, source }),
                Ok(created) => {
                    sync_directory(folder);
                    return Ok(created);
                }
            }
        }
    }

    fn create_new(path: PathBuf, instructions: &Instructions) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(PRIVATE_FILE_MODE)
            .open(&path)?;
        let created = Self { path, kept: false };
        file.write_all(instructions.as_str().as_bytes())?;
        file.sync_all()?;
        Ok(created)
    }

    fn keep(mut self) {
        self.kept = true;
    }
}

impl Drop for CreatedFile {
    fn drop(&mut self) {
        if self.kept {
            return;
        }
        if let Err(error) = fs::remove_file(&self.path) {
            tracing::warn!(%error, "a prompt file from a failed save was not removed");
        }
    }
}

/// Bounds the names one save tries, past those the folder listed.
const MAX_TAKEN_NAMES: usize = 1024;

/// Bounds a chain of links, as the system's own limit does.
const MAX_SYMLINK_HOPS: usize = 32;

/// Replaces the text whole, at the file a symlink names so the link stays.
/// A file already there keeps its mode.
fn replace(path: &Path, instructions: &Instructions) -> Result<(), SaveError> {
    let failed = |source| SaveError::File {
        path: path.to_path_buf(),
        source,
    };
    let target = link_target(path).map_err(failed)?;
    let directory = target.parent().ok_or_else(|| {
        failed(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the prompt file has no folder",
        ))
    })?;
    let mode = match fs::metadata(&target) {
        Ok(metadata) => metadata.permissions().mode() & 0o777,
        Err(error) if error.kind() == io::ErrorKind::NotFound => PRIVATE_FILE_MODE,
        Err(error) => return Err(failed(error)),
    };
    let mut temporary = tempfile::NamedTempFile::new_in(directory).map_err(failed)?;
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(mode))
        .and_then(|()| temporary.write_all(instructions.as_str().as_bytes()))
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(failed)?;
    temporary
        .persist(&target)
        .map_err(|error| failed(error.error))?;
    sync_directory(directory);
    Ok(())
}

/// The file `path` finally names. A dangling link names its missing target.
fn link_target(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _hop in 0..MAX_SYMLINK_HOPS {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(parent) if link.is_relative() => parent.join(link),
                    _ => link,
                };
            }
            Ok(_) => return Ok(current),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "the prompt file is behind too many links",
    ))
}

/// Makes a new name in `directory` survive a power cut, as the settings
/// file's does. The file is already in place, so this can't fail the save.
fn sync_directory(directory: &Path) {
    if let Err(error) = File::open(directory).and_then(|directory| directory.sync_all()) {
        tracing::warn!(%error, "prompts folder not flushed");
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt as _;

    use super::*;

    struct Harness {
        home: tempfile::TempDir,
        store: SettingsStore,
    }

    impl Harness {
        fn new(settings: &str) -> Self {
            let home = tempfile::tempdir().expect("home");
            fs::write(home.path().join("settings.toml"), settings).expect("settings");
            let store = SettingsStore::open(home.path().join("settings.toml")).expect("store");
            Self { home, store }
        }

        fn with_polish(text: &str) -> (Self, PathBuf) {
            let home = tempfile::tempdir().expect("home");
            let file = home.path().join("polish.md");
            fs::write(&file, text).expect("file");
            fs::write(home.path().join("settings.toml"), polish(&file)).expect("settings");
            let store = SettingsStore::open(home.path().join("settings.toml")).expect("store");
            (Self { home, store }, file)
        }

        fn save(&self, draft: PromptDraft) -> Result<Prompt, SaveError> {
            save(self.home.path(), &self.store, draft)
        }

        fn save_as_other_writer(&self, draft: PromptDraft) -> Result<Prompt, SaveError> {
            let other = SettingsStore::open(self.home.path().join("settings.toml")).expect("other");
            save(self.home.path(), &other, draft)
        }

        fn folder(&self) -> PathBuf {
            self.home.path().join(PROMPTS_DIR)
        }

        fn settings_text(&self) -> String {
            fs::read_to_string(self.home.path().join("settings.toml")).expect("settings")
        }

        fn prompts(&self) -> Prompts {
            SettingsStore::open(self.home.path().join("settings.toml"))
                .expect("store")
                .snapshot()
                .expect("snapshot")
                .settings
                .prompts
        }
    }

    fn instructions(text: &str) -> Instructions {
        Instructions::new(text.to_owned()).expect("instructions")
    }

    #[test]
    fn a_one_off_name_is_cut_on_a_character_boundary_to_fit_a_prompt_name() {
        // Byte 128 falls inside an é.
        let name = one_off_name(&instructions(&format!("a{}", "é".repeat(100))));

        assert_eq!(name, format!("a{}", "é".repeat(63)));
    }

    fn new_draft(name: &str, text: &str) -> PromptDraft {
        PromptDraft::New {
            kind: PromptKind::Compose,
            source: NewSource::Written {
                name: name.to_owned(),
                instructions: instructions(text),
            },
        }
    }

    fn chosen(name: Option<&str>, file: &Path, text: Option<&str>) -> PromptDraft {
        PromptDraft::New {
            kind: PromptKind::Compose,
            source: NewSource::Chosen {
                file: file.to_path_buf(),
                name: name.map(str::to_owned),
                instructions: text.map(instructions),
            },
        }
    }

    fn edit(id: &str, file: Option<&Path>, text: Option<&str>) -> PromptDraft {
        PromptDraft::Edit {
            id: PromptId::new(id).expect("id"),
            name: None,
            kind: None,
            file: file.map(Path::to_path_buf),
            instructions: text.map(instructions),
        }
    }

    fn polish(file: &Path) -> String {
        format!(
            "[prompts.polish]\nname = \"Polish\"\nkind = \"compose\"\nfile = {:?}\n\
             enabled = false\nchord = {{ key_code = 1, control = true }}\n",
            file.display().to_string()
        )
    }

    #[test]
    fn a_new_prompt_writes_its_file_then_adds_an_entry_that_is_on() {
        let harness = Harness::new("");

        let saved = harness
            .save(new_draft("  Summary ", "Three bullets."))
            .expect("saves");

        assert_eq!(saved.name(), "Summary");
        assert_eq!(saved.kind(), PromptKind::Compose);
        assert_eq!(saved.file(), harness.folder().join("Summary.md"));
        assert!(saved.enabled());
        assert_eq!(saved.chord(), None);
        assert_eq!(saved.id().as_str().len(), 36);
        assert_eq!(
            fs::read_to_string(saved.file()).expect("file"),
            "Three bullets."
        );
        assert_eq!(harness.prompts().entries(), [saved]);
    }

    #[test]
    fn a_new_file_never_replaces_one_already_there() {
        let harness = Harness::new("");

        let first = harness.save(new_draft("Summary", "One")).expect("first");
        let second = harness.save(new_draft("summary", "Two")).expect("second");

        assert_eq!(second.file(), harness.folder().join("summary 2.md"));
        assert_eq!(fs::read_to_string(first.file()).expect("first"), "One");
        assert_ne!(first.id(), second.id());
    }

    #[test]
    fn a_new_file_is_named_after_the_prompt_numbered_past_taken_and_reserved_names() {
        assert_eq!(file_name("Summary", &[]), "Summary.md");
        assert_eq!(
            file_name("Summary", &["summary.md".to_owned()]),
            "Summary 2.md"
        );
        assert_eq!(
            file_name(
                "Summary",
                &["Summary.md".to_owned(), "SUMMARY 2.md".to_owned()]
            ),
            "Summary 3.md"
        );
        assert_eq!(file_name("extract", &[]), "extract 2.md");
        assert_eq!(file_name("Cleanup", &[]), "Cleanup 2.md");
        assert_eq!(file_name("a/b:c", &[]), "a-b-c.md");
        assert_eq!(file_name(".hidden", &[]), "hidden.md");
    }

    #[test]
    fn a_chosen_file_is_used_where_it_is_and_left_alone_without_instructions() {
        let harness = Harness::new("");
        let file = harness.home.path().join("Email reply.md");
        fs::write(&file, "Reply kindly.").expect("chosen");
        let saved = harness
            .save(chosen(Some("Email reply"), &file, None))
            .expect("saves");

        assert_eq!(saved.file(), file);
        assert_eq!(fs::read_to_string(&file).expect("chosen"), "Reply kindly.");
        assert!(!harness.folder().exists());
    }

    #[test]
    fn a_chosen_file_without_usable_instructions_is_refused_and_nothing_is_saved() {
        let harness = Harness::new("");
        let empty = harness.home.path().join("empty.md");
        fs::write(&empty, " \n").expect("empty");
        let large = harness.home.path().join("large.md");
        fs::write(&large, "x".repeat(16 * 1024 + 1)).expect("large");
        let missing = harness.home.path().join("missing.md");
        let before = harness.settings_text();

        for file in [empty, large, missing] {
            let refused = harness
                .save(chosen(Some("Chosen"), &file, None))
                .expect_err("the file is refused");

            assert!(matches!(refused, SaveError::Unreadable(_)), "{refused:?}");
            assert_eq!(RpcErrorCode::from(&refused), RpcErrorCode::PromptUnreadable);
            assert_eq!(harness.settings_text(), before, "{}", file.display());
        }
    }

    #[test]
    fn contents_read_a_file_as_a_run_would_but_open_a_blank_or_missing_one() {
        let directory = tempfile::tempdir().expect("directory");
        let path = |name: &str| directory.path().join(name);
        fs::write(path("polish.md"), "Fix it.").expect("polish");
        fs::write(path("blank.md"), " \n").expect("blank");
        fs::write(path("large.md"), "x".repeat(16 * 1024 + 1)).expect("large");
        fs::write(path("binary.md"), [0xff, 0xfe]).expect("binary");

        assert_eq!(
            contents(&path("polish.md")).expect("reads"),
            PromptContents::Instructions("Fix it.".to_owned())
        );
        assert_eq!(
            contents(&path("blank.md")).expect("reads"),
            PromptContents::Instructions(String::new())
        );
        assert_eq!(
            contents(&path("gone.md")).expect("reads"),
            PromptContents::Missing
        );
        for unreadable in [
            path("large.md"),
            path("binary.md"),
            directory.path().to_path_buf(),
        ] {
            assert!(contents(&unreadable).is_err(), "{}", unreadable.display());
        }
    }

    #[test]
    fn an_edit_writes_its_file_in_place_and_keeps_the_chord_and_switch() {
        let (harness, file) = Harness::with_polish("Fix it.");
        let before = harness.prompts().entries()[0].clone();

        let saved = harness
            .save(PromptDraft::Edit {
                id: PromptId::new("polish").expect("id"),
                name: Some("Polish more".to_owned()),
                kind: Some(PromptKind::Rewrite),
                file: None,
                instructions: Some(instructions("Fix it well.")),
            })
            .expect("saves");

        assert_eq!(fs::read_to_string(&file).expect("file"), "Fix it well.");
        assert_eq!(saved.name(), "Polish more");
        assert_eq!(saved.kind(), PromptKind::Rewrite);
        assert_eq!(saved.file(), file);
        assert!(!saved.enabled());
        assert_eq!(saved.chord(), before.chord());
        assert_eq!(harness.prompts().entries(), [saved]);
    }

    #[test]
    fn an_edit_without_instructions_does_not_rewrite_the_file() {
        let (harness, file) = Harness::with_polish("Fix it.");
        let inode = fs::metadata(&file).expect("file").ino();

        harness.save(edit("polish", None, None)).expect("saves");

        assert_eq!(fs::metadata(&file).expect("file").ino(), inode);
        assert_eq!(fs::read_to_string(&file).expect("file"), "Fix it.");
    }

    #[test]
    fn an_edit_writes_through_a_symlink_and_keeps_the_link() {
        let directory = tempfile::tempdir().expect("directory");
        let target = directory.path().join("real.md");
        let link = directory.path().join("link.md");
        fs::write(&target, "Old").expect("target");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        let harness = Harness::new(&polish(&link));

        harness
            .save(edit("polish", Some(&link), Some("New")))
            .expect("saves");

        assert_eq!(fs::read_link(&link).expect("still a link"), target);
        assert_eq!(fs::read_to_string(&target).expect("target"), "New");
    }

    #[test]
    fn a_failed_write_leaves_the_prompt_list_alone() {
        let directory = tempfile::tempdir().expect("directory");
        let file = directory.path().join("gone").join("polish.md");
        let harness = Harness::new(&polish(&file));
        let before = harness.settings_text();

        let refused = harness.save(edit("polish", None, Some("Fix it well.")));

        assert!(
            matches!(refused, Err(SaveError::File { .. })),
            "{refused:?}"
        );
        assert_eq!(harness.settings_text(), before);
    }

    #[test]
    fn an_unknown_id_is_refused_before_any_file_is_written() {
        let directory = tempfile::tempdir().expect("directory");
        let file = directory.path().join("other.md");
        let harness = Harness::new("");

        let refused = harness.save(edit("missing", Some(&file), Some("Text")));

        let refused = refused.expect_err("the ID is unknown");
        assert!(matches!(refused, SaveError::UnknownPrompt), "{refused:?}");
        assert_eq!(RpcErrorCode::from(&refused), RpcErrorCode::PromptMissing);
        assert!(!file.exists());
    }

    #[test]
    fn a_refused_entry_leaves_no_new_file_behind() {
        let full: String = (0..16)
            .map(|n| {
                format!("[prompts.p{n}]\nname = \"P\"\nkind = \"rewrite\"\nfile = \"/p.md\"\n")
            })
            .collect();
        let harness = Harness::new(&full);

        let refused = harness.save(new_draft("Summary", "Three bullets."));

        let refused = refused.expect_err("the list is full");
        assert!(
            matches!(refused, SaveError::Prompt(PromptError::TooMany)),
            "{refused:?}"
        );
        assert_eq!(RpcErrorCode::from(&refused), RpcErrorCode::PromptsFull);
        assert!(!harness.folder().join("Summary.md").exists());
        assert_eq!(harness.settings_text(), full);
    }

    #[test]
    fn a_full_list_refuses_before_a_chosen_file_is_rewritten() {
        let directory = tempfile::tempdir().expect("directory");
        let mine = directory.path().join("mine.md");
        fs::write(&mine, "Mine.").expect("chosen");
        let full: String = (0..16)
            .map(|n| {
                format!("[prompts.p{n}]\nname = \"P\"\nkind = \"rewrite\"\nfile = \"/p.md\"\n")
            })
            .collect();
        let harness = Harness::new(&full);

        let refused = harness.save(chosen(Some("Mine"), &mine, Some("Overwritten.")));

        assert!(
            matches!(refused, Err(SaveError::Prompt(PromptError::TooMany))),
            "{refused:?}"
        );
        assert_eq!(fs::read_to_string(&mine).expect("chosen"), "Mine.");
    }

    #[test]
    fn an_edit_writes_the_file_another_writer_chose_after_the_store_read() {
        let directory = tempfile::tempdir().expect("directory");
        let old = directory.path().join("old.md");
        let new = directory.path().join("new.md");
        fs::write(&old, "Old.").expect("old");
        fs::write(&new, "New.").expect("new");
        let harness = Harness::new(&polish(&old));
        harness
            .store
            .snapshot()
            .expect("the store reads the old file");
        harness
            .save_as_other_writer(edit("polish", Some(&new), None))
            .expect("other saves");

        let saved = harness
            .save(edit("polish", None, Some("Edited.")))
            .expect("saves");

        assert_eq!(saved.file(), new);
        assert_eq!(fs::read_to_string(&new).expect("new"), "Edited.");
        assert_eq!(fs::read_to_string(&old).expect("old"), "Old.");
    }

    #[test]
    fn an_edit_whose_entry_moves_while_its_file_is_written_is_refused_and_keeps_the_file() {
        let harness = Harness::new("");
        let settings = harness.home.path().join("settings.toml");
        let elsewhere = harness.home.path().join("elsewhere.md");
        // The prompt reads the settings file itself, so writing the prompt's
        // file moves its entry between the write and the list edit.
        fs::write(&settings, polish(&settings)).expect("settings");
        let moved = polish(&elsewhere);

        let refused = harness
            .save(edit("polish", None, Some(&moved)))
            .expect_err("the entry moved");

        assert!(matches!(refused, SaveError::FileChanged), "{refused:?}");
        assert_eq!(RpcErrorCode::from(&refused), RpcErrorCode::Busy);
        assert_eq!(harness.settings_text(), moved, "the written file stays");
        assert_eq!(harness.prompts().entries()[0].file(), elsewhere);
    }

    #[test]
    fn an_edit_keeps_a_name_and_kind_another_writer_changed_and_it_left_out() {
        let (harness, file) = Harness::with_polish("Fix it.");
        harness
            .store
            .snapshot()
            .expect("the store reads the old name");
        let rename = PromptDraft::Edit {
            id: PromptId::new("polish").expect("id"),
            name: Some("Polish more".to_owned()),
            kind: Some(PromptKind::Rewrite),
            file: None,
            instructions: None,
        };
        harness.save_as_other_writer(rename).expect("other renames");
        let only_text = PromptDraft::Edit {
            id: PromptId::new("polish").expect("id"),
            name: None,
            kind: None,
            file: None,
            instructions: Some(instructions("Fix it well.")),
        };

        let saved = harness.save(only_text).expect("saves");

        assert_eq!(saved.name(), "Polish more");
        assert_eq!(saved.kind(), PromptKind::Rewrite);
        assert_eq!(fs::read_to_string(&file).expect("file"), "Fix it well.");
    }

    #[test]
    fn a_new_prompt_without_a_name_is_named_after_its_chosen_file() {
        let harness = Harness::new("");
        let file = harness.home.path().join(" Email reply.MD");
        fs::write(&file, "Reply kindly.").expect("chosen");

        let saved = harness.save(chosen(None, &file, None)).expect("saves");

        assert_eq!(saved.name(), "Email reply");
    }

    #[test]
    fn a_long_file_name_is_cut_to_fit_on_a_character_boundary() {
        let long = format!("{}é tail", "a".repeat(MAX_PROMPT_NAME_BYTES - 1));

        let name = name_from_file(&Path::new("/p").join(format!("{long}.md")));

        assert_eq!(name, "a".repeat(MAX_PROMPT_NAME_BYTES - 1));
        assert_eq!(name_from_file(Path::new("/p/notes.txt")), "notes.txt");
    }

    #[test]
    fn an_edit_through_a_dangling_link_writes_its_target_and_keeps_the_link() {
        let directory = tempfile::tempdir().expect("directory");
        let target = directory.path().join("gone.md");
        let link = directory.path().join("link.md");
        std::os::unix::fs::symlink(&target, &link).expect("link");
        let harness = Harness::new(&polish(&link));

        harness
            .save(edit("polish", None, Some("Back.")))
            .expect("saves");

        assert_eq!(fs::read_link(&link).expect("still a link"), target);
        assert_eq!(fs::read_to_string(&target).expect("target"), "Back.");
    }

    #[test]
    fn a_linked_prompts_folder_takes_new_files_as_it_is() {
        let harness = Harness::new("");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        fs::set_permissions(elsewhere.path(), Permissions::from_mode(0o755)).expect("mode");
        fs::create_dir_all(harness.folder().parent().expect("aria")).expect("aria");
        std::os::unix::fs::symlink(elsewhere.path(), harness.folder()).expect("link");

        let saved = harness
            .save(new_draft("Summary", "Three bullets."))
            .expect("saves");

        assert_eq!(
            fs::read_to_string(elsewhere.path().join("Summary.md")).expect("file"),
            "Three bullets."
        );
        assert_eq!(saved.file(), harness.folder().join("Summary.md"));
        let mode = fs::metadata(elsewhere.path()).expect("folder").mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn a_name_the_listing_missed_is_numbered_past() {
        let harness = Harness::new("");
        fs::create_dir_all(harness.folder()).expect("folder");
        // Decomposed é. APFS takes the composed name below as this one.
        fs::write(harness.folder().join("Cafe\u{301}.md"), "Theirs.").expect("theirs");

        let saved = harness
            .save(new_draft("Caf\u{e9}", "Mine."))
            .expect("saves");

        assert_eq!(fs::read_to_string(saved.file()).expect("mine"), "Mine.");
        assert_eq!(
            fs::read_to_string(harness.folder().join("Cafe\u{301}.md")).expect("theirs"),
            "Theirs."
        );
    }

    #[test]
    fn a_save_keeps_an_entry_another_writer_added_after_the_store_read() {
        let harness = Harness::new("");
        harness
            .save_as_other_writer(new_draft("From the CLI", "One"))
            .expect("other saves");

        harness
            .save(new_draft("From the app", "Two"))
            .expect("saves");

        let names: Vec<String> = harness
            .prompts()
            .entries()
            .iter()
            .map(|prompt| prompt.name().to_owned())
            .collect();
        assert_eq!(names, ["From the CLI", "From the app"]);
    }

    #[test]
    fn removing_a_prompt_keeps_its_file() {
        let (harness, file) = Harness::with_polish("Fix it.");

        let removed =
            remove(&harness.store, &PromptId::new("polish").expect("id")).expect("removes");

        assert_eq!(removed.file(), file);
        assert!(harness.prompts().entries().is_empty());
        assert_eq!(fs::read_to_string(&file).expect("file"), "Fix it.");
        assert!(matches!(
            remove(&harness.store, removed.id()),
            Err(SaveError::UnknownPrompt)
        ));
    }
}
