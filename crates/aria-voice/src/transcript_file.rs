//! A Mac Audio transcript on disk: how it is named, read, copied and
//! removed.

use std::fs::{DirBuilder, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{Mode, OFlags};
use time::format_description::BorrowedFormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};

/// How much of a transcript its history entry keeps, and search covers.
pub(crate) const EXCERPT_BYTES: usize = 1024;

pub(crate) const MAX_READ_BYTES: u64 = 1024 * 1024;

const MAX_NAME_ATTEMPTS: u32 = 100;

/// When a session started, as its name holds it.
const STAMP: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]Z");

/// How many hex digits of random tag follow the stamp.
const TAG_DIGITS: usize = 8;

/// A new session's name: when it started, then a random tag, as
/// `2026-11-25T10-00-00Z-1a2b3c4d.txt`. Each call draws a new tag, so a
/// name a removed transcript had never comes back for a later session.
pub(crate) fn name_at(at: OffsetDateTime) -> io::Result<String> {
    let stamp = at.format(STAMP).map_err(io::Error::other)?;
    let tag = getrandom::u32().map_err(io::Error::other)?;
    Ok(format!("{stamp}-{tag:08x}.txt"))
}

pub(crate) fn names_at(at: OffsetDateTime) -> impl Iterator<Item = io::Result<String>> {
    (0..MAX_NAME_ATTEMPTS).map(move |_| name_at(at))
}

/// When a session started, in ms, as [`name_at`] wrote it into its name.
/// `None` for any other name.
pub(crate) fn session_start(name: &str) -> Option<u64> {
    let (stamp, tag) = name.strip_suffix(".txt")?.rsplit_once('-')?;
    if tag.len() != TAG_DIGITS
        || !tag
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let at = PrimitiveDateTime::parse(stamp, STAMP).ok()?.assume_utc();
    u64::try_from(at.unix_timestamp()).ok()?.checked_mul(1_000)
}

pub(crate) fn taken() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "every name for this transcript is taken",
    )
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    #[error("the transcript is gone")]
    Missing,
    #[error("the transcript isn't a regular file of yours")]
    Foreign,
    #[error("the transcript couldn't be read")]
    Io(#[source] io::Error),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ReadError {
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error("the transcript isn't text")]
    NotText,
    #[error("the transcript couldn't be read")]
    Io(#[source] io::Error),
}

fn is_ours(is_file: bool, uid: u32) -> bool {
    is_file && uid == rustix::process::getuid().as_raw()
}

/// What is at `transcript`, without following a link: a regular file of ours.
pub(crate) fn inspect(transcript: &Path) -> Result<Metadata, OpenError> {
    let metadata = match transcript.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(OpenError::Missing),
        Err(error) => return Err(OpenError::Io(error)),
    };
    if !is_ours(metadata.is_file(), metadata.uid()) {
        return Err(OpenError::Foreign);
    }
    Ok(metadata)
}

pub(crate) fn modified_ms(metadata: &Metadata) -> Option<u64> {
    let modified = metadata.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// Read-only, never through a link, and only a regular file of ours.
pub(crate) fn open(transcript: &Path) -> Result<File, OpenError> {
    let descriptor = rustix::fs::open(
        transcript,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| match error {
        rustix::io::Errno::NOENT => OpenError::Missing,
        rustix::io::Errno::LOOP => OpenError::Foreign,
        error => OpenError::Io(error.into()),
    })?;
    let file = File::from(descriptor);
    let metadata = file.metadata().map_err(OpenError::Io)?;
    if !is_ours(metadata.is_file(), metadata.uid()) {
        return Err(OpenError::Foreign);
    }
    Ok(file)
}

/// The first `EXCERPT_BYTES` of a file already open, from its start, as
/// text, or `None` when the whole file is blank. A file blank only at its
/// start has an empty excerpt.
pub(crate) fn excerpt_of(file: &mut File) -> Result<Option<String>, ReadError> {
    let mut excerpt = Vec::with_capacity(EXCERPT_BYTES);
    file.seek(SeekFrom::Start(0)).map_err(ReadError::Io)?;
    file.take(EXCERPT_BYTES as u64)
        .read_to_end(&mut excerpt)
        .map_err(ReadError::Io)?;
    let excerpt = leading_text(excerpt).ok_or(ReadError::NotText)?;
    if !excerpt.trim().is_empty() {
        return Ok(Some(excerpt));
    }
    if is_blank(file).map_err(ReadError::Io)? {
        return Ok(None);
    }
    Ok(Some(String::new()))
}

/// What a file's name is to a folder that ignores case and Unicode
/// normalization, as macOS volumes do by default. Two names with one key can
/// be one file.
pub(crate) fn name_key(name: &std::ffi::OsStr) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.to_string_lossy()
        .nfd()
        .collect::<String>()
        .to_lowercase()
}

/// A transcript's text, as much as one read takes.
pub(crate) enum Text {
    Whole(String),
    /// The first `MAX_READ_BYTES`, ending on a whole character.
    Truncated(String),
}

/// Whether the open `file` holds nothing but whitespace, all the way
/// through. A byte outside ASCII counts as text, so a doubtful file stays.
pub(crate) fn is_blank(file: &mut File) -> io::Result<bool> {
    file.seek(SeekFrom::Start(0))?;
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(true);
        }
        if !buffer[..read].iter().all(u8::is_ascii_whitespace) {
            return Ok(false);
        }
    }
}

/// The text of a file already open, from its start, up to `MAX_READ_BYTES`.
pub(crate) fn read_file(file: &mut File) -> Result<Text, ReadError> {
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0)).map_err(ReadError::Io)?;
    file.take(MAX_READ_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    let whole = bytes.len() as u64 <= MAX_READ_BYTES;
    bytes.truncate(MAX_READ_BYTES as usize);
    if whole {
        String::from_utf8(bytes).ok().map(Text::Whole)
    } else {
        leading_text(bytes).map(Text::Truncated)
    }
    .ok_or(ReadError::NotText)
}

/// The bytes as text, dropping a character the cut split at the end.
fn leading_text(mut bytes: Vec<u8>) -> Option<String> {
    if let Err(error) = std::str::from_utf8(&bytes) {
        if error.error_len().is_some() {
            return None;
        }
        bytes.truncate(error.valid_up_to());
    }
    String::from_utf8(bytes).ok()
}

/// Both name one file, however each is spelled.
pub(crate) fn same_file(a: &Metadata, b: &Metadata) -> bool {
    (a.dev(), a.ino()) == (b.dev(), b.ino())
}

const MAX_STAGING_ATTEMPTS: u32 = 100;

/// Makes a new private file at a hidden name beside `destination`, for a
/// copy to write before it takes that name. A crash then leaves no partial
/// file under a transcript's name. Returns the file and its path.
pub(crate) fn create_staging(destination: &Path) -> io::Result<(File, PathBuf)> {
    for attempt in 1..=MAX_STAGING_ATTEMPTS {
        let staging = staging_path(destination, attempt);
        match create_new(&staging) {
            Ok(file) => return Ok((file, staging)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(taken())
}

/// A hidden name beside `destination` that no other copy uses.
fn staging_path(destination: &Path, attempt: u32) -> PathBuf {
    let name = destination
        .file_name()
        .map_or_else(Default::default, |name| name.to_string_lossy());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    destination.with_file_name(format!(
        ".{name}.{}-{now:x}-{attempt}.staged",
        std::process::id()
    ))
}

/// How [`create`] treats a file already at its path.
#[derive(Clone, Copy)]
pub(crate) enum Creation {
    /// Refused.
    New,
    /// Appended to. It keeps its mode.
    Append,
}

/// Opens the file a session writes at `path`, to append, never through a
/// link. A folder or file this makes is private to its owner. In a shared
/// folder another user can plant the file first, so only a regular file of
/// ours with one link is taken. A planted FIFO would block the open without
/// `NONBLOCK`.
pub(crate) fn create(path: &Path, creation: Creation) -> io::Result<File> {
    if let Some(parent) = path.parent() {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    let mut options = OpenOptions::new();
    match creation {
        Creation::New => options.create_new(true),
        Creation::Append => options.create(true),
    };
    let file = options
        .append(true)
        .mode(0o600)
        .custom_flags((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    if !is_ours(metadata.is_file(), metadata.uid()) || metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the transcript file belongs to another user or has other links",
        ));
    }
    Ok(file)
}

/// Makes a new private file at `staging`, refused when anything is there.
fn create_new(staging: &Path) -> io::Result<File> {
    // Appending: once renamed in, it is the session's file, and a writer
    // appending beside the session isn't overwritten.
    OpenOptions::new()
        .append(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(staging)
}

/// Whether `path` is `folder` or inside it, however a link or a firmlink
/// spells either, and whether or not either exists yet. Each side resolves
/// through its nearest folder that does exist, then keeps the parts that
/// don't.
pub(crate) fn is_within(path: &Path, folder: &Path) -> io::Result<bool> {
    if resolved(path)?.starts_with(resolved(folder)?) {
        return Ok(true);
    }
    let folder = match folder.metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let Some(existing) = path.ancestors().find(|ancestor| ancestor.exists()) else {
        return Ok(false);
    };
    for ancestor in existing.canonicalize()?.ancestors() {
        if same_file(&ancestor.metadata()?, &folder) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `path` with its nearest existing folder resolved and the rest kept.
fn resolved(path: &Path) -> io::Result<PathBuf> {
    let Some(existing) = path.ancestors().find(|ancestor| ancestor.exists()) else {
        return Ok(path.to_path_buf());
    };
    let rest = path.strip_prefix(existing).unwrap_or(Path::new(""));
    Ok(existing.canonicalize()?.join(rest))
}

/// Removes the regular file of ours at `transcript`, never through a link,
/// then syncs its folder so the removal lasts. A failed sync is an error,
/// since a crash could still bring the file back.
pub(crate) fn unlink(transcript: &Path) -> io::Result<Removal> {
    let Some(name) = transcript.file_name() else {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    };
    let directory = match open_directory(transcript) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Removal::Unavailable),
        Err(error) => return Err(error),
    };
    let removal = match rustix::fs::statat(&directory, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
    {
        Ok(stat)
            if is_ours(
                rustix::fs::FileType::from_raw_mode(stat.st_mode).is_file(),
                stat.st_uid,
            ) =>
        {
            match rustix::fs::unlinkat(&directory, name, rustix::fs::AtFlags::empty()) {
                Ok(()) => Removal::Removed,
                Err(rustix::io::Errno::NOENT) => Removal::Missing,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(_) => return Ok(Removal::Foreign),
        Err(rustix::io::Errno::NOENT) => Removal::Missing,
        Err(error) => return Err(error.into()),
    };
    directory.sync_all()?;
    Ok(removal)
}

/// Replaces the file at `path` with one holding `contents`, in `own`'s
/// mode. The new file is whole before it takes the name, so a crash leaves
/// the old file or the new one there, and at most a staging file beside it.
/// Returns the new file, open to append.
pub(crate) fn rewrite(path: &Path, own: &File, contents: &[u8]) -> io::Result<File> {
    let permissions = own.metadata()?.permissions();
    let file = replace(path, Some(permissions), |file| {
        io::Write::write_all(file, contents)
    })?;
    // A crash may still bring back the old file, which is as much the
    // session's.
    if let Err(error) = sync_folder(path) {
        tracing::warn!(%error, "a rewritten transcript's folder could not be synced");
    }
    Ok(file)
}

/// Copies all of `source` to `destination`, over any file there. The copy is
/// whole before it takes the name, and its folder is synced after, so once
/// this returns a crash can't lose it. It takes the mode of a regular file it
/// replaces, and is private to its owner otherwise.
pub(crate) fn copy(source: &mut File, destination: &Path) -> io::Result<()> {
    let permissions = match destination.symlink_metadata() {
        Ok(metadata) if metadata.is_file() => Some(metadata.permissions()),
        Ok(_) => None,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    source.seek(SeekFrom::Start(0))?;
    replace(destination, permissions, |file| {
        io::copy(source, file).map(drop)
    })?;
    sync_folder(destination)
}

/// Writes a staging file beside `path` with `fill`, syncs it, then renames
/// it onto `path`. On any failure the staging file goes. Returns the file
/// now at `path`, open to append.
fn replace(
    path: &Path,
    permissions: Option<std::fs::Permissions>,
    fill: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<File> {
    let (mut file, staging) = create_staging(path)?;
    let written = (|| {
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        fill(&mut file)?;
        file.sync_all()?;
        std::fs::rename(&staging, path)
    })();
    if let Err(error) = written {
        if let Err(error) = std::fs::remove_file(&staging) {
            tracing::warn!(%error, "a staging file was left behind");
        }
        return Err(error);
    }
    Ok(file)
}

/// Removes every staging file a crash left in `folder`.
/// Runs only while no session can be rewriting there.
pub(crate) fn remove_staging(folder: &Path) {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_staging = name
            .to_str()
            .is_some_and(|name| name.starts_with('.') && name.ends_with(".staged"));
        if is_staging && let Err(error) = unlink(&entry.path()) {
            tracing::warn!(%error, "a rewrite's staging file was left behind");
        }
    }
}

/// Whether nothing is at `path`, in a folder that is there. A folder that is
/// away, as on a drive that isn't connected, leaves it undecided.
pub(crate) fn is_gone(path: &Path) -> bool {
    matches!(path.symlink_metadata(), Err(error) if error.kind() == io::ErrorKind::NotFound)
        && path.parent().is_some_and(Path::is_dir)
}

/// What removing a transcript found at its path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Removal {
    Removed,
    /// Nothing there, in a folder that is.
    Missing,
    /// Its folder isn't there, as on a drive that isn't connected.
    Unavailable,
    /// A link, or anything but a regular file the person owns. Left in place.
    Foreign,
}

/// A folder's device and inode, which tell whether two spellings name it.
pub(crate) type FolderIdentity = (u64, u64);

pub(crate) fn folder_identity(folder: &Path) -> Option<FolderIdentity> {
    folder
        .metadata()
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// Syncs the folder `file` is in, so every rename there lasts a crash.
pub(crate) fn sync_folder(file: &Path) -> io::Result<()> {
    open_directory(file)?.sync_all()
}

fn open_directory(file: &Path) -> io::Result<File> {
    let parent = file
        .parent()
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let directory = rustix::fs::open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(directory))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_staging_file_a_crash_left_goes_and_a_transcript_stays() {
        let folder = tempfile::tempdir().unwrap();
        let transcript = folder.path().join("2026-11-25T10-00-00Z.txt");
        std::fs::write(&transcript, "heard").unwrap();
        let (_file, staging) = create_staging(&transcript).unwrap();

        remove_staging(folder.path());

        assert!(transcript.exists());
        assert!(!staging.exists());
    }

    #[test]
    fn a_name_reads_back_as_the_start_it_was_given() {
        let at = OffsetDateTime::from_unix_timestamp(1_795_600_800).unwrap();
        let name = name_at(at).unwrap();
        assert!(name.starts_with("2026-11-25T10-00-00Z-"), "{name}");
        assert_eq!(session_start(&name), Some(1_795_600_800_000));
        for refused in [
            "2026-11-25T10-00-00Z.txt",
            "2026-11-25T10-00-00Z-2.txt",
            "2026-11-25T10-00-00Z-1A2B3C4D.txt",
            "2026-11-25T10-00-00Z-1a2b3c4d5.txt",
            "2026-11-25T10-00-00-1a2b3c4d.txt",
            "../2026-11-25T10-00-00Z-1a2b3c4d.txt",
            "notes.txt",
        ] {
            assert_eq!(session_start(refused), None, "{refused}");
        }
    }

    #[test]
    fn sessions_in_one_second_never_share_a_name() {
        let at = OffsetDateTime::from_unix_timestamp(1_795_600_800).unwrap();
        let names: std::collections::HashSet<String> =
            (0..100).map(|_| name_at(at).unwrap()).collect();
        assert_eq!(names.len(), 100);
    }
}
