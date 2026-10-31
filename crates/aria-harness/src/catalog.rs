//! Scenario catalog: case files on disk, their schema, and what each case
//! asserts about observable behavior.

use std::path::{Path, PathBuf};
use std::process::Output;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Expectations {
    /// How `aria daemon` starts. A case without it runs its client steps
    /// with no daemon.
    #[serde(default)]
    daemon: Option<DaemonExpectations>,
    /// Client steps that run before the daemon starts.
    #[serde(default)]
    before_daemon: Vec<ClientStep>,
    #[serde(default)]
    client: Vec<ClientStep>,
}

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
    pub(crate) fn check(&self, output: &Output) -> Result<(), String> {
        let mut unmet = Vec::new();
        match self.exit {
            Exit::Success if !output.status.success() => unmet.push("expected success".to_owned()),
            Exit::Failure if output.status.success() => unmet.push("expected failure".to_owned()),
            Exit::Success | Exit::Failure => {}
        }
        unmet.extend(missing("stdout", &output.stdout, &self.stdout_contains));
        unmet.extend(missing("stderr", &output.stderr, &self.stderr_contains));
        judged(&format!("`aria {}`", self.run.join(" ")), &unmet, output)
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
    pub(crate) fn daemon_startup(&self) -> Option<Startup> {
        self.daemon.as_ref().map(|daemon| daemon.startup)
    }

    pub(crate) fn steps_before_daemon(&self) -> &[ClientStep] {
        &self.before_daemon
    }

    pub(crate) fn client_steps(&self) -> &[ClientStep] {
        &self.client
    }

    pub(crate) fn check_daemon(&self, output: &Output) -> Result<(), String> {
        let Some(daemon) = &self.daemon else {
            return Ok(());
        };
        let mut unmet = Vec::new();
        match daemon.startup {
            Startup::Success if !output.status.success() => {
                unmet.push("expected a clean exit".to_owned());
            }
            Startup::Failure if output.status.success() => {
                unmet.push("expected startup to fail".to_owned());
            }
            Startup::Success | Startup::Failure => {}
        }
        unmet.extend(missing("stderr", &output.stderr, &daemon.stderr_contains));
        judged("`aria daemon`", &unmet, output)
    }
}

fn missing<'a>(
    stream: &'a str,
    written: &[u8],
    needles: &'a [String],
) -> impl Iterator<Item = String> + 'a {
    let written = String::from_utf8_lossy(written).into_owned();
    needles
        .iter()
        .filter(move |needle| !written.contains(needle.as_str()))
        .map(move |needle| format!("{stream} lacks {needle:?}"))
}

/// Everything a failed check needs to be debugged from its message alone.
fn judged(command: &str, unmet: &[String], output: &Output) -> Result<(), String> {
    if unmet.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{command}: {}\nexit: {}\nstdout:\n{}\nstderr:\n{}",
        unmet.join("; "),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    ))
}

const CASE_EXTENSION: &str = "yaml";
const PLACEHOLDER_ENV_PREFIX: &str = "ARIA_TEST_";
const FIXTURES_PLACEHOLDER: &str = "FIXTURES";
const PLACEHOLDER_OPEN: &str = "${";
const PLACEHOLDER_CLOSE: &str = "}";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Case {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) platform: Option<Platform>,
    #[serde(default)]
    pub(crate) settings: String,
    pub(crate) expect: Expectations,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Platform {
    Macos,
    Linux,
}

impl Platform {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }

    pub(crate) fn is_current(self) -> bool {
        self.name() == std::env::consts::OS
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

    #[error("failed to copy the fixtures in {}", path.display())]
    CopyFixtures {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

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

    #[error("case {} has client steps, but its daemon is expected to fail startup", path.display())]
    ClientStepsAfterFailedStartup { path: PathBuf },

    #[error("case {} expects neither a daemon nor a client step", path.display())]
    NothingToCheck { path: PathBuf },

    #[error("unclosed {PLACEHOLDER_OPEN} placeholder in the settings of case {name:?}")]
    UnclosedPlaceholder { name: String },
}

/// The settings with each `${NAME}` filled from `ARIA_TEST_NAME` and
/// `${FIXTURES}` from the fixtures directory, or the first variable that is
/// unset. The runner ignores such a case instead of failing it.
pub(crate) enum ResolvedSettings {
    Ready(String),
    MissingEnvironment(String),
}

impl Case {
    /// Placeholders let a case name machine-local inputs (a model file)
    /// without hard-coding paths into the catalog.
    pub(crate) fn resolve_settings(
        &self,
        fixtures: &Path,
    ) -> Result<ResolvedSettings, CatalogError> {
        let mut resolved = String::new();
        let mut rest = self.settings.as_str();
        while let Some(start) = rest.find(PLACEHOLDER_OPEN) {
            resolved.push_str(&rest[..start]);
            let after = &rest[start + PLACEHOLDER_OPEN.len()..];
            let Some(end) = after.find(PLACEHOLDER_CLOSE) else {
                return Err(CatalogError::UnclosedPlaceholder {
                    name: self.name.clone(),
                });
            };
            let name = &after[..end];
            rest = &after[end + PLACEHOLDER_CLOSE.len()..];
            if name == FIXTURES_PLACEHOLDER {
                resolved.push_str(&fixtures.to_string_lossy());
                continue;
            }
            let variable = format!("{PLACEHOLDER_ENV_PREFIX}{name}");
            match std::env::var(&variable) {
                Ok(value) => resolved.push_str(&value),
                Err(_) => return Ok(ResolvedSettings::MissingEnvironment(variable)),
            }
        }
        resolved.push_str(rest);
        Ok(ResolvedSettings::Ready(resolved))
    }
}

impl Case {
    pub(crate) fn load(path: &Path) -> Result<Self, CatalogError> {
        let source = std::fs::read_to_string(path).map_err(|source| CatalogError::ReadCase {
            path: path.to_path_buf(),
            source,
        })?;
        let case: Self =
            serde_saphyr::from_str(&source).map_err(|source| CatalogError::ParseCase {
                path: path.to_path_buf(),
                source: Box::new(source),
            })?;
        let has_clients = !case.expect.client.is_empty();
        match case.expect.daemon_startup() {
            Some(Startup::Failure) if has_clients => {
                Err(CatalogError::ClientStepsAfterFailedStartup {
                    path: path.to_path_buf(),
                })
            }
            None if !has_clients => Err(CatalogError::NothingToCheck {
                path: path.to_path_buf(),
            }),
            Some(_) | None => Ok(case),
        }
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
