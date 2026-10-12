//! Scenario catalog: case files on disk, their schema, and what each case
//! asserts about observable behavior.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Expectations {
    daemon: DaemonExpectations,
    /// Commands to run against the daemon once it is serving, in order. A
    /// case with none asserts only on the daemon's own output.
    #[serde(default)]
    client: Vec<ClientStep>,
}

/// One `aria` invocation against the running daemon.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ClientStep {
    /// The arguments after `aria`, one per element.
    pub(crate) run: Vec<String>,
    #[serde(default)]
    exit: Exit,
    #[serde(default)]
    stdout_contains: Vec<String>,
    #[serde(default)]
    stderr_contains: Vec<String>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Exit {
    #[default]
    Success,
    Failure,
}

impl ClientStep {
    fn check(&self, case_name: &str, output: &Output) {
        let command = self.run.join(" ");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        match self.exit {
            Exit::Success => assert!(
                output.status.success(),
                "{case_name}: `aria {command}` failed ({:?}); stderr:\n{stderr}",
                output.status
            ),
            Exit::Failure => assert!(
                !output.status.success(),
                "{case_name}: `aria {command}` unexpectedly succeeded; stdout:\n{stdout}"
            ),
        }
        for needle in &self.stdout_contains {
            assert!(
                stdout.contains(needle),
                "{case_name}: `aria {command}` stdout does not contain {needle:?}:\n{stdout}"
            );
        }
        for needle in &self.stderr_contains {
            assert!(
                stderr.contains(needle),
                "{case_name}: `aria {command}` stderr does not contain {needle:?}:\n{stderr}"
            );
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonExpectations {
    startup: Startup,
    // Every entry must appear, so a case can pin both a failure's message and
    // the cause under it.
    #[serde(default)]
    stderr_contains: Vec<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Startup {
    Success,
    Failure,
}

impl Expectations {
    pub(crate) fn daemon_startup(&self) -> Startup {
        self.daemon.startup
    }

    pub(crate) fn client_steps(&self) -> &[ClientStep] {
        &self.client
    }

    pub(crate) fn check_client(&self, case_name: &str, step: usize, output: &Output) {
        self.client[step].check(case_name, output);
    }

    pub(crate) fn check(&self, case_name: &str, output: &Output) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        match self.daemon.startup {
            Startup::Success => assert!(
                output.status.success(),
                "{case_name}: daemon did not exit cleanly ({:?}); stderr:\n{stderr}",
                output.status
            ),
            Startup::Failure => assert!(
                !output.status.success(),
                "{case_name}: daemon startup unexpectedly succeeded"
            ),
        }
        for needle in &self.daemon.stderr_contains {
            assert!(
                stderr.contains(needle),
                "{case_name}: stderr does not contain {needle:?}:\n{stderr}"
            );
        }
    }
}

const CASE_EXTENSION: &str = "yaml";
const PLACEHOLDER_ENV_PREFIX: &str = "ARIA_TEST_";
const PLACEHOLDER_OPEN: &str = "${";
const PLACEHOLDER_CLOSE: &str = "}";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Case {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) platform: Option<Platform>,
    pub(crate) config: String,
    pub(crate) expect: Expectations,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Platform {
    Macos,
    Linux,
}

impl Platform {
    pub(crate) fn is_current(self) -> bool {
        let name = match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
        };
        name == std::env::consts::OS
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("failed to read case directory {}", path.display())]
    ReadDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("unreadable entry in case directory {}", path.display())]
    ReadEntry {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("no cases in {}", path.display())]
    Empty { path: PathBuf },

    #[error("failed to read case {}", path.display())]
    ReadCase {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("invalid case {}", path.display())]
    ParseCase {
        path: PathBuf,
        #[source]
        source: Box<serde_saphyr::Error>,
    },

    #[error("unclosed {PLACEHOLDER_OPEN} placeholder in the config of case {name:?}")]
    UnclosedPlaceholder { name: String },
}

/// The config with `${NAME}` placeholders filled from `ARIA_TEST_NAME`
/// environment variables, or the variable that is missing — the runner skips
/// such a case instead of failing the suite.
pub(crate) enum ResolvedConfig {
    Ready(String),
    MissingEnvironment(String),
}

impl Case {
    /// Placeholders let a case name machine-local inputs (a model file)
    /// without hard-coding paths into the catalog.
    pub(crate) fn resolve_config(&self) -> Result<ResolvedConfig, CatalogError> {
        let mut resolved = String::new();
        let mut rest = self.config.as_str();
        while let Some(start) = rest.find(PLACEHOLDER_OPEN) {
            resolved.push_str(&rest[..start]);
            let after = &rest[start + PLACEHOLDER_OPEN.len()..];
            let Some(end) = after.find(PLACEHOLDER_CLOSE) else {
                return Err(CatalogError::UnclosedPlaceholder {
                    name: self.name.clone(),
                });
            };
            let variable = format!("{PLACEHOLDER_ENV_PREFIX}{}", &after[..end]);
            match std::env::var(&variable) {
                Ok(value) => resolved.push_str(&value),
                Err(_) => return Ok(ResolvedConfig::MissingEnvironment(variable)),
            }
            rest = &after[end + PLACEHOLDER_CLOSE.len()..];
        }
        resolved.push_str(rest);
        Ok(ResolvedConfig::Ready(resolved))
    }
}

impl Case {
    pub(crate) fn load(path: &Path) -> Result<Self, CatalogError> {
        let source = std::fs::read_to_string(path).map_err(|source| CatalogError::ReadCase {
            path: path.to_path_buf(),
            source,
        })?;
        serde_saphyr::from_str(&source).map_err(|source| CatalogError::ParseCase {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }
}

pub(crate) fn discover(directory: &Path) -> Result<Vec<PathBuf>, CatalogError> {
    let entries = std::fs::read_dir(directory).map_err(|source| CatalogError::ReadDirectory {
        path: directory.to_path_buf(),
        source,
    })?;
    let mut cases = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|source| CatalogError::ReadEntry {
                path: directory.to_path_buf(),
                source,
            })?
            .path();
        if path
            .extension()
            .is_some_and(|extension| extension == CASE_EXTENSION)
        {
            cases.push(path);
        }
    }
    cases.sort();
    if cases.is_empty() {
        return Err(CatalogError::Empty {
            path: directory.to_path_buf(),
        });
    }
    Ok(cases)
}
