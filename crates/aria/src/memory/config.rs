//! What the assistant remembers across sessions and how it learns it.

use std::path::{Path, PathBuf};

use aria_settings::config::memory_directory;
use aria_settings::settings::Memory;

/// What the assistant remembers across sessions and how it learns it.
/// `dir` also holds the lexicon, which works whether or not `enabled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemoryConfig {
    pub(crate) enabled: bool,
    /// A model name the `claude` CLI accepts for fact extraction.
    pub(crate) extract_model: String,
    pub(crate) max_facts_per_session: u32,
    pub(crate) dir: PathBuf,
}

const DEFAULT_MEMORY_EXTRACT_MODEL: &str = "sonnet";

const DEFAULT_MEMORY_MAX_FACTS: u32 = 10;

impl MemoryConfig {
    pub(crate) fn load(memory: &Memory, home: &Path) -> Self {
        Self {
            enabled: memory.enabled.unwrap_or(false),
            extract_model: memory
                .extract_model
                .clone()
                .unwrap_or_else(|| DEFAULT_MEMORY_EXTRACT_MODEL.to_owned()),
            max_facts_per_session: memory
                .max_facts_per_session
                .unwrap_or(DEFAULT_MEMORY_MAX_FACTS),
            dir: memory_directory(memory, home),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aria_settings::settings::Loaded;

    #[test]
    fn the_defaults_match_what_code_does_without_them() {
        let home = Path::new("/home/user");
        let defaults = Loaded::parse(&aria_settings::settings::testing::defaults_text(), home)
            .expect("every default is valid")
            .settings;
        assert_eq!(
            MemoryConfig::load(&defaults.memory, home),
            MemoryConfig::load(&Memory::default(), home)
        );
    }
}
