use std::path::PathBuf;

use aria_daemon::DaemonSession;
use clap::Subcommand;

use super::runtime_paths;

#[derive(Subcommand)]
pub(super) enum VocabularyCommand {
    /// Show whether a directory has a prepared vocabulary
    Status { directory: Option<PathBuf> },
    /// Rescan local filenames and text to prepare a directory's vocabulary
    Refresh { directory: Option<PathBuf> },
    /// Remove a directory's cached vocabulary
    Forget { directory: Option<PathBuf> },
}

pub(super) fn run(command: VocabularyCommand) -> anyhow::Result<()> {
    let paths = runtime_paths()?;
    let session = DaemonSession::connect_at(&paths)?;
    let status = match command {
        VocabularyCommand::Status { directory } => {
            session.vocabulary_status(directory.map(vocabulary_dir).transpose()?.as_ref())?
        }
        VocabularyCommand::Refresh { directory } => {
            session.vocabulary_refresh(directory.map(vocabulary_dir).transpose()?.as_ref())?
        }
        VocabularyCommand::Forget { directory } => {
            session.vocabulary_forget(directory.map(vocabulary_dir).transpose()?.as_ref())?
        }
    };
    println!(
        "{:?}: {} terms, catalog revision {}, runtime revision {}",
        status.state, status.terms, status.catalog_revision, status.runtime_revision
    );
    if status.state != aria_voice::vocabulary::State::Disabled && !status.error.is_empty() {
        anyhow::bail!(
            "vocabulary {} failed; the previous snapshot is retained when available",
            status.error
        );
    }
    Ok(())
}

pub(super) fn vocabulary_dir(path: PathBuf) -> anyhow::Result<aria_settings::Directory> {
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
        Err(error) => return Err(error.into()),
    };
    Ok(aria_settings::Directory::new(path)?)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::Parser;

    use crate::commands::{Cli, Command};

    #[test]
    fn vocabulary_commands_and_recording_context_parse() {
        for arguments in [
            vec!["aria", "vocabulary", "status"],
            vec!["aria", "vocabulary", "refresh", "/projects/aria"],
            vec!["aria", "vocabulary", "forget", "/deleted/project"],
        ] {
            assert!(matches!(
                Cli::try_parse_from(arguments).unwrap().command,
                Command::Vocabulary { .. }
            ));
        }
        assert!(matches!(
            Cli::try_parse_from(["aria", "dictate", "--no-project"])
                .unwrap()
                .command,
            Command::Dictate {
                no_project: true,
                ..
            }
        ));
        assert!(
            matches!(Cli::try_parse_from(["aria", "ask", "--project", "/projects/aria", "hello"]).unwrap().command, Command::Ask { project: Some(path), .. } if path == Path::new("/projects/aria"))
        );
        assert!(
            Cli::try_parse_from([
                "aria",
                "ask",
                "--project",
                "/projects/aria",
                "--cancel",
                "1"
            ])
            .is_err()
        );
    }
}
