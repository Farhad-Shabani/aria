mod context;
pub use context::{Activity, Context, Failure, List, Status};
pub mod catalog;
pub mod collection;
pub mod discover;
#[cfg(test)]
mod evaluation;
pub mod extract;
pub mod select;

pub use aria_kit::transcription::layer::vocabulary::{MatchError, PersonalRules, Term, Vocabulary};

use std::ops::Range;
use std::path::PathBuf;

pub(crate) const MAX_TERMS: usize = 100_000;
pub(crate) const MAX_CANDIDATES: usize = 128;
pub(crate) const SELECTED_TERMS: usize = 2048;
pub(crate) const MAX_SOURCE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SourceKind {
    DirectoryName,
    FileName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub canonical: String,
    pub span: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub relative_path: PathBuf,
    pub kind: SourceKind,
    pub fingerprint: [u8; 32],
    pub input_bytes: usize,
    pub candidates: Vec<Candidate>,
}

pub struct Discovery {
    pub selected: PathBuf,
    pub root: PathBuf,
    pub sources: Vec<Source>,
    pub coverage: Coverage,
}

#[derive(Default, Debug)]
pub struct Coverage {
    pub entries: usize,
    pub hidden: usize,
    pub symlinks: usize,
    pub unsupported: usize,
    pub ignore_bytes: usize,
    /// Nested folders whose `.git` is a file: linked worktrees and submodules.
    pub linked: usize,
    pub unreadable: usize,
    pub completeness: Completeness,
}

/// How much of the selected folder a scan listed. Ordered from most to least
/// complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Completeness {
    #[default]
    Whole,
    /// A limit, or a folder it could not read, left names out.
    Partial,
    /// The deadline passed, so a later scan of the same folder may find more.
    OutOfTime,
}

impl Completeness {
    pub fn is_partial(self) -> bool {
        self != Self::Whole
    }
}

pub(crate) fn source_bytes(source: &Source) -> usize {
    512 + 2 * source.relative_path.as_os_str().len()
        + source
            .candidates
            .iter()
            .map(|candidate| 256 + 2 * candidate.canonical.len())
            .sum::<usize>()
}

pub fn cache_key(extractor_key: &str) -> Option<String> {
    if extractor_key.is_empty()
        || extractor_key.len() > 128
        || !extractor_key.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return None;
    }
    Some(format!("names1:terms{SELECTED_TERMS}:{extractor_key}"))
}
