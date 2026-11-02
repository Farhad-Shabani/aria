//! What the assistant remembers across sessions and how it learns it.

use std::path::PathBuf;

/// What the assistant remembers across sessions and how it learns it.
/// `dir` also holds the lexicon, which works whether or not `enabled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryConfig {
    pub enabled: bool,
    /// A model name the `claude` CLI accepts for fact extraction.
    pub extract_model: String,
    pub max_facts_per_session: u32,
    pub dir: PathBuf,
}
