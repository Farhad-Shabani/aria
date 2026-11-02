//! What the assistant remembers across sessions: facts the user stated,
//! extracted from their asks after the fact and shown to the assistant on the
//! next one; and the lexicon that fixes the words a transcriber gets wrong.
//! Everything lives under the memory directory: `memory.db`, `profile.md`,
//! and the user's own `lexicon.txt`.

mod config;
pub use config::MemoryConfig;
mod consolidate;
mod extract;
mod lexicon;
mod profile;
mod store;

#[cfg(test)]
mod eval;
#[cfg(test)]
mod eval_stats;

use std::path::Path;

pub use consolidate::ConsolidationReport;
pub use consolidate::run_backlog;
pub use extract::ConsolidationResult;
pub use lexicon::Lexicon;
pub use lexicon::load_terms;
pub use profile::load_profile_block;
pub use profile::write_profile;
pub use store::{DerivedPair, Fact, Store, StoreError};

pub const DB_FILE: &str = "memory.db";
/// The extraction and lexicon cases the memory eval and the bench score.
pub const EVAL_CASES: &str = include_str!("eval_cases.json");
pub const LEXICON_FILE: &str = "lexicon.txt";
pub(crate) const PROFILE_FILE: &str = "profile.md";

/// The store in the memory directory.
pub fn open(dir: &Path) -> Result<Store, StoreError> {
    Store::open(&dir.join(DB_FILE))
}
