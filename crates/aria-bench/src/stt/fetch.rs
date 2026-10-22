//! Files a committed list pins by sha256, and the tool runs that produce
//! them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aria::primitives::cancel::CancelToken;
use aria::primitives::process::{Bounds, Capture, RunError, run};
use aria_harness::artifact::{ArtifactError, fingerprint};
use signal_hook::consts::SIGINT;

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const DIAGNOSTIC_BYTES: usize = 64 * 1024;

pub(super) struct Row<'a> {
    pub(super) id: &'a str,
    pub(super) sha256: &'a str,
    pub(super) text: &'a str,
}

pub(super) fn parse(list: &str) -> Result<Vec<Row<'_>>, FetchError> {
    list.lines()
        .enumerate()
        .map(|(index, line)| {
            let mut fields = line.split('\t');
            match (fields.next(), fields.next(), fields.next(), fields.next()) {
                (Some(id), Some(sha256), Some(text), None) if !text.is_empty() => {
                    Ok(Row { id, sha256, text })
                }
                _ => Err(FetchError::MalformedList { line: index + 1 }),
            }
        })
        .collect()
}

pub(super) fn ensure<E: From<FetchError>>(
    row: &Row<'_>,
    path: &Path,
    produce: impl FnOnce(&Path) -> Result<(), E>,
) -> Result<(), E> {
    if pinned(path, row.sha256)? {
        return Ok(());
    }
    produce(path)?;
    if pinned(path, row.sha256)? {
        return Ok(());
    }
    // Nothing downstream can tell the stray file from a pinned one.
    fs::remove_file(path).map_err(io_error(path))?;
    Err(FetchError::Unpinned(row.id.to_owned()).into())
}

fn pinned(path: &Path, sha256: &str) -> Result<bool, FetchError> {
    if !path.is_file() {
        return Ok(false);
    }
    Ok(fingerprint(path)? == sha256)
}

/// `version_flag` makes the tool exit cleanly without input.
pub(super) fn require(tool: &'static str, version_flag: &str) -> Result<(), FetchError> {
    run_tool(tool, Command::new(tool).arg(version_flag), PROBE_TIMEOUT)
}

pub(super) fn download(url: &str, archive: &Path) -> Result<(), FetchError> {
    tracing::info!(url, "downloading");
    let mut curl = Command::new("curl");
    curl.args(["--silent", "--show-error", "--fail", "--location"])
        .args(["--retry", "3", "--output"])
        .arg(archive)
        .arg(url);
    run_tool("curl", &mut curl, DOWNLOAD_TIMEOUT)
}

pub(super) fn run_tool(
    tool: &'static str,
    command: &mut Command,
    timeout: Duration,
) -> Result<(), FetchError> {
    let bounds = Bounds {
        timeout,
        stdout: Capture::KeepTail(DIAGNOSTIC_BYTES),
        stderr: Capture::KeepTail(DIAGNOSTIC_BYTES),
    };
    let interrupt = Interrupt::arm()?;
    let Output { status, stderr, .. } = match run(command, &[], bounds, &interrupt.token()) {
        Ok(output) => output,
        Err(RunError::Cancelled) => return Err(FetchError::Interrupted),
        Err(RunError::Spawn(source)) if source.kind() == io::ErrorKind::NotFound => {
            return Err(FetchError::Missing(tool));
        }
        Err(source) => return Err(FetchError::Run { tool, source }),
    };
    // A Ctrl-C between the tool's exit and here is read by nobody else.
    let interrupted = interrupt.raised();
    drop(interrupt);
    if interrupted {
        return Err(FetchError::Interrupted);
    }
    if status.success() {
        return Ok(());
    }
    Err(FetchError::Failed {
        tool,
        stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
    })
}

pub(super) fn create_dir(path: &Path) -> Result<(), FetchError> {
    fs::create_dir_all(path).map_err(io_error(path))
}

pub(super) fn io_error(path: &Path) -> impl FnOnce(io::Error) -> FetchError + '_ {
    move |source| FetchError::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// Ctrl-C stops the running tool too. Each tool runs in its own process
/// group, which the terminal's interrupt never reaches. signal-hook cannot
/// restore SIGINT's default action, so `terminate` picks which handler acts.
/// Outside a tool run, Ctrl-C still ends the program.
struct Interrupt {
    flags: &'static Flags,
}

struct Flags {
    /// True whenever no tool is running, so the default action applies.
    terminate: Arc<AtomicBool>,
    raised: Arc<AtomicBool>,
}

static FLAGS: OnceLock<Flags> = OnceLock::new();

impl Interrupt {
    fn arm() -> Result<Self, FetchError> {
        let flags = match FLAGS.get() {
            Some(flags) => flags,
            None => {
                let flags = Flags {
                    terminate: Arc::new(AtomicBool::new(true)),
                    raised: Arc::new(AtomicBool::new(false)),
                };
                // Registered first so it acts first: a raised `terminate`
                // ends the process before `raised` is read.
                signal_hook::flag::register_conditional_default(
                    SIGINT,
                    Arc::clone(&flags.terminate),
                )
                .map_err(FetchError::Signal)?;
                signal_hook::flag::register(SIGINT, Arc::clone(&flags.raised))
                    .map_err(FetchError::Signal)?;
                FLAGS.get_or_init(|| flags)
            }
        };
        flags.raised.store(false, Ordering::SeqCst);
        flags.terminate.store(false, Ordering::SeqCst);
        Ok(Self { flags })
    }

    fn raised(&self) -> bool {
        self.flags.raised.load(Ordering::SeqCst)
    }

    fn token(&self) -> CancelToken {
        let raised = Arc::clone(&self.flags.raised);
        CancelToken::from_fn(move || raised.load(Ordering::SeqCst))
    }
}

impl Drop for Interrupt {
    fn drop(&mut self) {
        self.flags.terminate.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("line {line} of the list needs an id, a sha256, and a text")]
    MalformedList { line: usize },

    #[error("{0} produced bytes the list does not pin")]
    Unpinned(String),

    #[error("needs {0}; install it with `brew install {0}`")]
    Missing(&'static str),

    #[error("{tool} did not finish")]
    Run {
        tool: &'static str,
        #[source]
        source: RunError,
    },

    #[error("{tool} failed: {stderr}")]
    Failed { tool: &'static str, stderr: String },

    #[error("interrupted")]
    Interrupted,

    #[error("failed to handle Ctrl-C")]
    Signal(#[source] io::Error),

    #[error("failed to update {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    Hash(#[from] ArtifactError),
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in super::super) fn sha256(bytes: &[u8]) -> String {
        let file = tempfile::NamedTempFile::new().unwrap();
        fs::write(file.path(), bytes).unwrap();
        fingerprint(file.path()).unwrap()
    }

    #[test]
    fn a_row_needs_three_fields_and_a_text() {
        assert_eq!(parse("a\tb\tc\n").unwrap().len(), 1);
        for list in ["a\tb\n", "a\tb\t\n", "a\tb\tc\td\n"] {
            assert!(matches!(
                parse(list),
                Err(FetchError::MalformedList { line: 1 })
            ));
        }
    }

    #[test]
    fn ensure_produces_a_missing_or_differing_file_and_checks_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pinned");
        let sha256 = sha256(b"pinned");
        let row = Row {
            id: "one",
            sha256: &sha256,
            text: "x",
        };
        let write = |bytes: &'static [u8]| {
            move |path: &Path| {
                fs::write(path, bytes).unwrap();
                Ok::<_, FetchError>(())
            }
        };

        ensure(&row, &path, write(b"pinned")).unwrap();
        ensure(&row, &path, |_| -> Result<(), FetchError> {
            panic!("a pinned file is kept")
        })
        .unwrap();

        fs::write(&path, "stale").unwrap();
        ensure(&row, &path, write(b"pinned")).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"pinned");

        fs::write(&path, "stale").unwrap();
        assert!(matches!(
            ensure(&row, &path, write(b"other")),
            Err(FetchError::Unpinned(id)) if id == "one"
        ));
        assert!(!path.exists(), "an unpinned file is not left behind");
    }
}
