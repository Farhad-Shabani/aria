use std::path::PathBuf;

use aria_daemon::DaemonSession;
use aria_voice::vocabulary::dictionary::{AddWordOutcome, RemoveWordOutcome};
use aria_voice::vocabulary::{Activity, Completeness, Failure, Status};
use clap::Subcommand;

use super::runtime_paths;

#[derive(Subcommand)]
pub(super) enum VocabularyCommand {
    /// Show whether a directory has a prepared vocabulary
    Status { directory: Option<PathBuf> },
    /// Rescan file and folder names to prepare a directory's vocabulary
    Refresh { directory: Option<PathBuf> },
    /// Remove a directory's cached vocabulary
    Forget { directory: Option<PathBuf> },
    /// Add a word to your dictionary
    Add { word: String },
    /// Remove a word from your dictionary
    Remove { word: String },
    /// List the words in your dictionary
    List,
}

pub(super) fn run(command: VocabularyCommand) -> anyhow::Result<()> {
    let paths = runtime_paths()?;
    let session = DaemonSession::connect_at(&paths)?;
    let status = match command {
        VocabularyCommand::Add { word } => {
            match session.add_word(&word)? {
                AddWordOutcome::Added => println!("Added {}", word.trim()),
                AddWordOutcome::AlreadyListed => {
                    println!("{} is already in the dictionary", word.trim());
                }
            }
            return Ok(());
        }
        VocabularyCommand::Remove { word } => {
            match session.remove_word(&word)? {
                RemoveWordOutcome::Removed => println!("Removed {}", word.trim()),
                RemoveWordOutcome::NotListed => println!("{} isn't in the dictionary", word.trim()),
            }
            return Ok(());
        }
        VocabularyCommand::List => {
            let words = session.list_words()?;
            if words.is_empty() {
                println!("Your dictionary is empty");
            }
            for word in words {
                println!("{word}");
            }
            return Ok(());
        }
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
    let Status::Folder { list, activity } = status else {
        println!("Disabled");
        return Ok(());
    };
    let state = match (activity, &list) {
        (Activity::Refreshing, _) => "Refreshing",
        (Activity::Removing, _) => "Removing",
        (Activity::Failed(Failure::Limit), _) => "TooLarge",
        (Activity::Idle, Some(list)) if !list.is_stale => "Ready",
        (_, Some(_)) => "Stale",
        (_, None) => "Unavailable",
    };
    match &list {
        Some(list) => println!(
            "{state}: {} of {} names{}{}",
            list.terms,
            list.found,
            if list.completeness.is_some_and(Completeness::is_partial) {
                ", partial"
            } else {
                ""
            },
            list.catalog_revision
                .map(|revision| format!(", catalog revision {revision}"))
                .unwrap_or_default(),
        ),
        None => println!("{state}"),
    }
    if let Activity::Failed(failure) = activity {
        let cause = match failure {
            Failure::Limit => "vocabulary refresh exceeded a resource limit",
            Failure::Refresh => "vocabulary refresh failed",
            Failure::Evicted => "vocabulary list was dropped to free memory",
            Failure::Removal => "vocabulary removal failed",
        };
        anyhow::bail!("{cause}; the previous list stays in use when available");
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
