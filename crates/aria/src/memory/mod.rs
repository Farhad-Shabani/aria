//! What the assistant remembers across sessions: facts the user stated,
//! extracted from their asks after the fact and shown to the agent on the
//! next one; and the lexicon that fixes the words a transcriber gets wrong.
//! Everything lives under the memory directory: `memory.db`, `profile.md`,
//! and the user's own `lexicon.txt`.

mod consolidate;
mod extract;
mod lexicon;
mod profile;
mod store;

#[cfg(test)]
mod eval;
#[cfg(test)]
mod eval_stats;

use std::path::PathBuf;

use aria_kit::cancel::CancelToken;

use crate::config::{ConfigError, Environment, memory_directory};

pub use consolidate::ConsolidationReport;
pub(crate) use consolidate::run_backlog;
pub use extract::ConsolidationResult;
pub(crate) use lexicon::Lexicon;
pub use lexicon::load_terms;
pub(crate) use profile::load_profile_block;
pub use profile::write_profile;
pub use store::{DerivedPair, Fact, Store, StoreError};

pub(crate) const DB_FILE: &str = "memory.db";
pub const LEXICON_FILE: &str = "lexicon.txt";
pub(crate) const PROFILE_FILE: &str = "profile.md";

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("failed to read the memory configuration")]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The store and its directory as this shell's configuration finds them.
pub fn open() -> Result<(Store, PathBuf), MemoryError> {
    let dir = memory_directory(&Environment::capture()?)?;
    let store = Store::open(&dir.join(DB_FILE))?;
    Ok((store, dir))
}

/// Extracts from every session with new turns, now, in this process.
pub fn consolidate_now() -> Result<ConsolidationReport, MemoryError> {
    run_backlog(&Environment::capture()?, &CancelToken::never())
}
