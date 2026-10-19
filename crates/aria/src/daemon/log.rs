//! Secure daemon tracing log ownership and rotation.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use rustix::fs::{Mode, OFlags};
use tracing_subscriber::fmt::MakeWriter;

use crate::primitives::private_fs::{self, PrivateFileError};

const MAX_LOG_FILE_BYTES: u64 = 8 * 1024 * 1024;
const LOG_TAIL_LINES: usize = 50;
const LOG_READ_CHUNK_BYTES: usize = 8 * 1024;

type Result<T> = std::result::Result<T, DaemonLogError>;

struct DaemonLog {
    file: File,
    length: u64,
    modified: SystemTime,
}

impl DaemonLog {
    fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = open_file(&path)?;
        let metadata =
            private_fs::prepare_file(&file).map_err(|source| DaemonLogError::Unusable {
                path: path.clone(),
                source,
            })?;
        let modified = metadata
            .modified()
            .map_err(|source| DaemonLogError::Inspect { path, source })?;

        Ok(Self {
            file,
            length: metadata.len(),
            modified,
        })
    }

    /// Opening may have just created an empty file, so it never counts.
    fn last_written(&self) -> SystemTime {
        if self.length == 0 {
            SystemTime::UNIX_EPOCH
        } else {
            self.modified
        }
    }
}

/// A synchronous two-file writer that bounds retained tracing output.
struct DaemonLogWriter {
    state: Mutex<LogState>,
}

impl DaemonLogWriter {
    fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let primary = DaemonLog::open(path)?;
        let secondary = DaemonLog::open(rotated_path(path))?;
        // A CLI that hosts a daemon writes beside the service. Resuming the
        // newer file keeps its lines where `read_log_tail` looks.
        let (active, written) = if secondary.last_written() > primary.last_written() {
            (1, secondary.length)
        } else {
            (0, primary.length)
        };

        Ok(Self {
            state: Mutex::new(LogState {
                files: [primary.file, secondary.file],
                active,
                written,
            }),
        })
    }

    fn lock_state(&self) -> MutexGuard<'_, LogState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub fn open_log_writer(path: PathBuf) -> Result<impl for<'writer> MakeWriter<'writer>> {
    DaemonLogWriter::open(path)
}

pub fn read_log_tail(path: PathBuf) -> Result<String> {
    let mut tails = [path.clone(), rotated_path(&path)]
        .into_iter()
        .filter_map(|path| read_file_tail(path).transpose())
        .collect::<Result<Vec<_>>>()?;
    if tails.is_empty() {
        return Err(DaemonLogError::NotFound { path });
    }

    tails.sort_by_key(|tail| tail.modified);
    let lines: Vec<String> = tails.into_iter().flat_map(|tail| tail.lines).collect();
    let first = lines.len().saturating_sub(LOG_TAIL_LINES);
    Ok(lines[first..].join("\n"))
}

struct LogTail {
    modified: SystemTime,
    lines: Vec<String>,
}

fn read_file_tail(path: PathBuf) -> Result<Option<LogTail>> {
    let Some(mut file) = open_log_reader(&path)? else {
        return Ok(None);
    };
    let metadata = private_fs::verify_file(&file).map_err(|source| DaemonLogError::Unusable {
        path: path.clone(),
        source,
    })?;
    let modified = metadata
        .modified()
        .map_err(|source| DaemonLogError::Inspect {
            path: path.clone(),
            source,
        })?;
    let lines = read_last_lines(&mut file, &path)?;
    Ok(Some(LogTail { modified, lines }))
}

fn read_last_lines(file: &mut File, path: &Path) -> Result<Vec<String>> {
    let mut end = file
        .seek(SeekFrom::End(0))
        .map_err(|source| DaemonLogError::Read {
            path: path.to_path_buf(),
            source,
        })?;
    let earliest = end.saturating_sub(MAX_LOG_FILE_BYTES);
    let mut chunks = Vec::new();
    let mut newline_count = 0;

    while end > earliest && newline_count <= LOG_TAIL_LINES {
        let chunk_length = (end - earliest).min(LOG_READ_CHUNK_BYTES as u64) as usize;
        let start = end - chunk_length as u64;
        file.seek(SeekFrom::Start(start))
            .map_err(|source| DaemonLogError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        let mut chunk = vec![0; chunk_length];
        file.read_exact(&mut chunk)
            .map_err(|source| DaemonLogError::Read {
                path: path.to_path_buf(),
                source,
            })?;
        newline_count += chunk.iter().filter(|&&byte| byte == b'\n').count();
        chunks.push(chunk);
        end = start;
    }

    chunks.reverse();
    let mut bytes: Vec<u8> = chunks.into_iter().flatten().collect();
    if end > 0 {
        let first_complete_line = bytes
            .iter()
            .position(|&byte| byte == b'\n')
            .map_or(bytes.len(), |newline| newline + 1);
        bytes.drain(..first_complete_line);
    }
    if bytes.last().is_some_and(|&byte| byte != b'\n') {
        let complete_length = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |newline| newline + 1);
        bytes.truncate(complete_length);
    }
    let text = String::from_utf8(bytes).map_err(|source| DaemonLogError::InvalidUtf8 {
        path: path.to_path_buf(),
        source,
    })?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let first = lines.len().saturating_sub(LOG_TAIL_LINES);
    Ok(lines[first..].to_vec())
}

impl<'writer> MakeWriter<'writer> for DaemonLogWriter {
    type Writer = EventWriter<'writer>;

    fn make_writer(&'writer self) -> Self::Writer {
        EventWriter {
            state: self.lock_state(),
            started: false,
        }
    }
}

struct LogState {
    files: [File; 2],
    active: usize,
    /// Counts only this process's lines, so a rotation can empty a file
    /// another process still writes.
    written: u64,
}

impl LogState {
    fn rotate_if_full(&mut self) -> io::Result<()> {
        if self.written < MAX_LOG_FILE_BYTES {
            return Ok(());
        }

        let next = (self.active + 1) % self.files.len();
        self.files[next].set_len(0)?;
        self.active = next;
        self.written = 0;
        Ok(())
    }

    fn active_file(&mut self) -> &mut File {
        &mut self.files[self.active]
    }
}

struct EventWriter<'writer> {
    state: MutexGuard<'writer, LogState>,
    started: bool,
}

impl EventWriter<'_> {
    fn prepare(&mut self) -> io::Result<()> {
        if !self.started {
            self.state.rotate_if_full()?;
            self.started = true;
        }
        Ok(())
    }
}

impl Write for EventWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.prepare()?;
        let written = self.state.active_file().write(buffer)?;
        self.state.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state.active_file().flush()
    }
}

fn rotated_path(path: &Path) -> PathBuf {
    let mut rotated = OsString::from(path.as_os_str());
    rotated.push(".1");
    PathBuf::from(rotated)
}

fn open_file(path: &Path) -> Result<File> {
    let flags = OFlags::WRONLY
        | OFlags::APPEND
        | OFlags::CREATE
        | OFlags::CLOEXEC
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK;
    let descriptor = rustix::fs::open(path, flags, Mode::RUSR | Mode::WUSR).map_err(|source| {
        DaemonLogError::Open {
            path: path.to_path_buf(),
            source: source.into(),
        }
    })?;
    Ok(File::from(descriptor))
}

fn open_log_reader(path: &Path) -> Result<Option<File>> {
    let flags = OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let descriptor = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            let source: io::Error = error.into();
            if source.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(DaemonLogError::Open {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    Ok(Some(File::from(descriptor)))
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonLogError {
    #[error("no daemon log exists at {}", path.display())]
    NotFound { path: PathBuf },

    #[error("failed to open daemon log {}", path.display())]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("daemon log {} is not usable", path.display())]
    Unusable {
        path: PathBuf,
        #[source]
        source: PrivateFileError,
    },

    #[error("failed to inspect daemon log {}", path.display())]
    Inspect {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to read daemon log {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("daemon log {} is not valid UTF-8", path.display())]
    InvalidUtf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;

    #[test]
    fn log_tail_returns_only_the_latest_entries() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("daemon.log");
        let mut contents: String = (0..60).map(|line| format!("line {line}\n")).collect();
        contents.push_str("incomplete");
        write_private(&path, contents.as_bytes());

        let tail = read_log_tail(path).expect("the log tail should be read");
        let expected: Vec<String> = (10..60).map(|line| format!("line {line}")).collect();

        assert_eq!(tail.lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn log_tail_refuses_a_symbolic_link() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let target = directory.path().join("target.log");
        let path = directory.path().join("daemon.log");
        write_private(&target, b"private log\n");
        symlink(target, &path).expect("symbolic link should be created");

        let error = read_log_tail(path.clone()).expect_err("a symbolic link must be refused");

        assert!(matches!(error, DaemonLogError::Open { path: actual, .. } if actual == path));
    }

    #[test]
    fn log_tail_reports_a_missing_log() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("daemon.log");

        let error = read_log_tail(path.clone()).expect_err("a missing log should be reported");

        assert!(matches!(error, DaemonLogError::NotFound { path: actual } if actual == path));
    }

    #[test]
    fn a_full_log_rotates_into_the_other_file() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let mut state = LogState {
            files: [
                DaemonLog::open(directory.path().join("daemon.log"))
                    .expect("the primary log should open")
                    .file,
                DaemonLog::open(directory.path().join("daemon.log.1"))
                    .expect("the rotated log should open")
                    .file,
            ],
            active: 0,
            written: MAX_LOG_FILE_BYTES,
        };

        state.rotate_if_full().expect("the full log should rotate");
        assert_eq!(state.active, 1);
        assert_eq!(state.written, 0);

        state
            .rotate_if_full()
            .expect("the fresh log should be inspected");
        assert_eq!(state.active, 1, "a log with room does not rotate");
    }

    #[test]
    fn a_writer_resumes_the_file_written_last() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("daemon.log");
        let rotated = rotated_path(&path);
        write_private(&path, b"older\n");
        write_private(&rotated, b"newer\n");
        let earlier = SystemTime::now() - std::time::Duration::from_secs(60);
        File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(earlier))
            .expect("the older log's time should be set");

        write_line(&path, b"resumed\n");

        assert_eq!(
            fs::read(&path).expect("the older log should read"),
            b"older\n"
        );
        assert_eq!(
            fs::read(&rotated).expect("the newer log should read"),
            b"newer\nresumed\n"
        );
    }

    #[test]
    fn a_writer_ignores_an_empty_rotated_file() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let path = directory.path().join("daemon.log");
        write_private(&path, b"existing\n");

        write_line(&path, b"appended\n");

        assert_eq!(
            fs::read(&path).expect("the log should read"),
            b"existing\nappended\n"
        );
    }

    fn write_line(path: &Path, line: &[u8]) {
        let writer = DaemonLogWriter::open(path).expect("the log writer should open");
        writer
            .make_writer()
            .write_all(line)
            .expect("the line should be written");
    }

    fn write_private(path: &Path, contents: &[u8]) {
        fs::write(path, contents).expect("test log should be written");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .expect("test log should be private");
    }
}
